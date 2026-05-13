//! HTTP routes (session-centric).

use std::collections::HashMap;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use tower_http::trace::TraceLayer;

use super::AppState;
use super::curator;
use super::internal_api;
use super::ui;
use crate::domain::{FeedbackKind, FeedbackTargetRef};
use crate::lifecycle::{CommitSha, SessionId};
use crate::review_state::{ReviewGateState, ReviewPhase};
use crate::storage::{
    events as ev_store, feedback as feedback_store, feedback::FeedbackRecord,
    implementation_revisions as impl_revs, plan_revisions, plans, sessions,
};

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(home))
        .route("/events", get(home_events_stream))
        .route("/healthz", get(healthz))
        .route("/sessions/{session_id}", get(session_detail))
        .route("/sessions/{session_id}/history", get(session_history))
        .route(
            "/sessions/{session_id}/history/{plan_id}",
            get(history_plan_detail),
        )
        .route("/sessions/{session_id}/commits/{sha}", get(commit_diff))
        .route(
            "/sessions/{session_id}/plan_revisions/{rev_id}",
            get(plan_revision_view),
        )
        .route(
            "/sessions/{session_id}/plan_revisions/{rev_id}/diff",
            get(plan_revision_diff),
        )
        .route("/sessions/{session_id}/events", get(events_stream))
        .route("/sessions/{session_id}/delete", post(delete_session))
        .route("/sessions/{session_id}/claim", post(claim_session))
        .route("/sessions/{session_id}/finish", post(finish_session))
        .route(
            "/sessions/{session_id}/review_gate_override",
            post(review_gate_override),
        )
        .route("/sessions/{session_id}/comment", post(curator::comment))
        .route("/sessions/{session_id}/archive", post(curator::archive))
        .route("/sessions/{session_id}/rename", post(curator::rename))
        .route("/internal/tools", get(internal_api::list_tools))
        .route("/internal/tool_call", post(internal_api::call_tool))
        .layer(middleware::from_fn(origin_guard))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn home(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    let rows = sessions::list_active(&state.pool)
        .await
        .map_err(AppError::sqlx)?;
    let sessions_by_id: HashMap<String, sessions::Session> =
        rows.iter().map(|s| (s.id.clone(), s.clone())).collect();
    let mut out = Vec::with_capacity(rows.len());
    for s in rows {
        let active_plan_state = if let Some(id) = s.active_plan_id {
            plans::fetch(&state.pool, id)
                .await
                .map_err(AppError::sqlx)?
                .map(|p| p.state)
        } else {
            None
        };
        let finished = if active_plan_state.is_none() {
            let row: Option<(i64,)> = sqlx::query_as(
                "SELECT id FROM plans WHERE session_id = ? AND state = 'finished' \
                 ORDER BY finished_at DESC, id DESC LIMIT 1",
            )
            .bind(&s.id)
            .fetch_optional(&state.pool)
            .await
            .map_err(AppError::sqlx)?;
            row.is_some()
        } else {
            false
        };
        let sid = SessionId::from(s.id.as_str());
        let review_gate = review_gate_for_row(&state, &sid, active_plan_state.as_deref()).await?;
        let is_repo_effective = state
            .lifecycle
            .is_repo_effective_session(&sid)
            .await
            .map_err(AppError::lifecycle)?;
        out.push(ui::SessionRow {
            session: s,
            active_plan_state,
            review_gate,
            is_repo_effective,
            finished,
        });
    }
    let activity = build_home_activity(&state, &sessions_by_id).await?;
    Ok(Html(ui::home(&out, &activity).into_string()))
}

async fn review_gate_for_row(
    state: &AppState,
    sid: &SessionId,
    active_plan_state: Option<&str>,
) -> Result<Option<crate::review_state::ReviewGateDecision>, AppError> {
    let phase = match active_plan_state {
        Some("planning") => ReviewPhase::Plan,
        Some("implementing") => ReviewPhase::Impl,
        _ => return Ok(None),
    };
    state
        .lifecycle
        .review_gate_for_phase(sid, phase)
        .await
        .map_err(AppError::lifecycle)
}

#[derive(Default)]
struct TimelineMaps {
    plan_rev_number_by_id: HashMap<i64, i64>,
    plan_preview_by_id: HashMap<i64, String>,
    amend_by_sha: HashMap<String, crate::daemon::amend::AmendInfo>,
    commit_preview_by_sha: HashMap<String, String>,
    feedback_target_by_id: HashMap<i64, (String, String)>,
    feedback_preview_by_id: HashMap<i64, String>,
    feedback_kind_by_id: HashMap<i64, FeedbackKind>,
    feedback_author_by_id: HashMap<i64, String>,
    feedback_status_by_author_kind:
        HashMap<(FeedbackKind, String), crate::domain::FeedbackFileStatus>,
}

async fn build_home_activity(
    state: &AppState,
    sessions_by_id: &HashMap<String, sessions::Session>,
) -> Result<ui::HomeActivity, AppError> {
    let events = ev_store::recent_for_active_sessions(&state.pool, 40)
        .await
        .map_err(AppError::sqlx)?;
    let mut grouped: HashMap<String, Vec<crate::storage::events::Event>> = HashMap::new();
    for ev in events {
        grouped.entry(ev.session_id.clone()).or_default().push(ev);
    }
    let mut rows = Vec::new();
    for (session_id, mut events) in grouped {
        let Some(session) = sessions_by_id.get(&session_id) else {
            continue;
        };
        let sid = SessionId::from(session_id);
        let maps = build_timeline_maps(state, &sid).await?;
        events.sort_by_key(|ev| std::cmp::Reverse(ev.id));
        rows.extend(timeline_rows_for_events(session, &events, &maps, true));
    }
    rows.sort_by_key(|row| std::cmp::Reverse(row.event_id));
    Ok(ui::HomeActivity {
        rows,
        unavailable: false,
    })
}

async fn build_timeline_maps(state: &AppState, sid: &SessionId) -> Result<TimelineMaps, AppError> {
    let all_plans = plans::list_for_session(&state.pool, sid)
        .await
        .map_err(AppError::sqlx)?;
    let all_feedback = feedback_store::list_for_session(&state.pool, sid)
        .await
        .map_err(AppError::feedback)?;
    let feedback_ctx = state
        .lifecycle
        .build_feedback_context(sid)
        .await
        .map_err(AppError::lifecycle)?;

    let mut maps = TimelineMaps::default();
    for plan in &all_plans {
        let impl_rows = impl_revs::list_for_plan(&state.pool, plan.id)
            .await
            .map_err(AppError::sqlx)?;
        let amend_info = crate::daemon::amend::classify(&impl_rows);
        for (row, info) in impl_rows.into_iter().zip(amend_info) {
            if let Some(preview) = text_preview(&row.commit_message) {
                maps.commit_preview_by_sha
                    .insert(row.commit_sha.clone(), preview);
            }
            maps.amend_by_sha.insert(row.commit_sha, info);
        }

        let revs = plan_revisions::list_for_plan(&state.pool, plan.id)
            .await
            .map_err(AppError::sqlx)?;
        for rev in revs {
            maps.plan_rev_number_by_id
                .insert(rev.id, rev.revision_number);
            if let Some(preview) = text_preview(&rev.body) {
                maps.plan_preview_by_id.insert(rev.id, preview);
            }
        }
    }

    for rec in &all_feedback {
        let (kind_string, target_id, feedback_kind) = match &rec.target {
            FeedbackTargetRef::PlanRevision(rev_id) => (
                "plan_revision".to_string(),
                rev_id.to_string(),
                FeedbackKind::Plan,
            ),
            FeedbackTargetRef::ImplementationCommit(sha) => (
                "implementation_commit".to_string(),
                sha.as_str().to_string(),
                FeedbackKind::Impl,
            ),
        };
        maps.feedback_target_by_id
            .insert(rec.id, (kind_string, target_id));
        maps.feedback_kind_by_id.insert(rec.id, feedback_kind);
        maps.feedback_author_by_id
            .insert(rec.id, rec.author_label.as_str().to_string());
        if let Some(preview) = text_preview(&rec.body) {
            maps.feedback_preview_by_id.insert(rec.id, preview);
        }
    }

    for snapshot in feedback_ctx.plan_files {
        maps.feedback_status_by_author_kind.insert(
            (FeedbackKind::Plan, snapshot.row.author_label.clone()),
            snapshot.status,
        );
    }
    for snapshot in feedback_ctx.impl_files {
        maps.feedback_status_by_author_kind.insert(
            (FeedbackKind::Impl, snapshot.row.author_label.clone()),
            snapshot.status,
        );
    }

    Ok(maps)
}

fn timeline_rows_for_events(
    session: &sessions::Session,
    events: &[crate::storage::events::Event],
    maps: &TimelineMaps,
    with_session_prefix: bool,
) -> Vec<ui::TimelineRow> {
    let session_title = session.display_title.as_deref();
    let ctx = ui::TimelineRenderCtx {
        session_id: &session.id,
        session_id_for_prefix: with_session_prefix.then_some(session.id.as_str()),
        session_title_for_prefix: with_session_prefix
            .then_some(session_title.unwrap_or(session.id.as_str())),
        plan_rev_number_by_id: &maps.plan_rev_number_by_id,
        plan_preview_by_id: &maps.plan_preview_by_id,
        amend_by_sha: &maps.amend_by_sha,
        commit_preview_by_sha: &maps.commit_preview_by_sha,
        feedback_target_by_id: &maps.feedback_target_by_id,
        feedback_preview_by_id: &maps.feedback_preview_by_id,
        feedback_kind_by_id: &maps.feedback_kind_by_id,
        feedback_author_by_id: &maps.feedback_author_by_id,
        feedback_status_by_author_kind: &maps.feedback_status_by_author_kind,
    };
    events
        .iter()
        .rev()
        .map(|ev| ui::timeline_row_from_event(ev, &ctx))
        .collect()
}

fn text_preview(body: &str) -> Option<String> {
    body.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| {
            let mut out = String::new();
            for (idx, ch) in line.chars().enumerate() {
                if idx >= 180 {
                    out.push('…');
                    return out;
                }
                out.push(ch);
            }
            out
        })
}

async fn session_detail(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> Result<Html<String>, AppError> {
    let sid = SessionId::from(session_id.clone());
    let session = sessions::fetch(&state.pool, &sid)
        .await
        .map_err(AppError::sqlx)?
        .ok_or_else(|| AppError::not_found(format!("no session {session_id}")))?;
    let active_plan = if let Some(id) = session.active_plan_id {
        plans::fetch(&state.pool, id)
            .await
            .map_err(AppError::sqlx)?
    } else {
        None
    };
    let is_repo_effective = state
        .lifecycle
        .is_repo_effective_session(&sid)
        .await
        .map_err(AppError::lifecycle)?;

    let archived_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM plans WHERE session_id = ? AND state = 'archived'",
    )
    .bind(sid.as_str())
    .fetch_one(&state.pool)
    .await
    .map_err(AppError::sqlx)?;

    let events = ev_store::for_session(&state.pool, &sid)
        .await
        .map_err(AppError::sqlx)?;
    let max_event_id = events.iter().map(|e| e.id).max().unwrap_or(0);

    // Pull the full feedback context once; UI reads the pre-derived
    // status from each FeedbackFileSnapshot. The MCP tool, the watcher
    // dispatcher, and these UI tables all consume the same snapshot.
    let ctx = state
        .lifecycle
        .build_feedback_context(&sid)
        .await
        .map_err(AppError::lifecycle)?;

    let repo_root_path = std::path::Path::new(&session.repo_root);
    let git_logs_head_path = ctx
        .git_logs_head_path
        .as_ref()
        .map(|p| p.display().to_string());
    let plan_feedback_dir_path = Some(
        crate::feedback_path::feedback_dir(repo_root_path, &sid, crate::domain::FeedbackKind::Plan)
            .display()
            .to_string(),
    );
    let impl_feedback_dir_path = Some(
        crate::feedback_path::feedback_dir(repo_root_path, &sid, crate::domain::FeedbackKind::Impl)
            .display()
            .to_string(),
    );

    let timeline_maps = build_timeline_maps(&state, &sid).await?;
    let timeline_rows = timeline_rows_for_events(&session, &events, &timeline_maps, false);
    let plan_review_gate = state
        .lifecycle
        .review_gate_for_phase(&sid, ReviewPhase::Plan)
        .await
        .map_err(AppError::lifecycle)?;
    let impl_review_gate = state
        .lifecycle
        .review_gate_for_phase(&sid, ReviewPhase::Impl)
        .await
        .map_err(AppError::lifecycle)?;
    let active_plan_preview = match &active_plan {
        Some(plan) => plan_revisions::latest_for_plan(&state.pool, plan.id)
            .await
            .map_err(AppError::sqlx)?
            .map(|rev| {
                let is_long = rev.body.lines().count() > 24 || rev.body.len() > 3500;
                ui::ActivePlanPreview {
                    rev_id: rev.id,
                    revision_number: rev.revision_number,
                    body_html: render_markdown(&rev.body),
                    created_at: rev.created_at,
                    is_long,
                }
            }),
        None => None,
    };

    Ok(Html(
        ui::session_detail(&ui::SessionDetail {
            session,
            active_plan,
            is_repo_effective,
            active_plan_preview,
            archived_count,
            plan_feedback_files: ctx.plan_files,
            impl_feedback_files: ctx.impl_files,
            git_logs_head_path,
            plan_feedback_dir_path,
            impl_feedback_dir_path,
            plan_review_gate,
            impl_review_gate,
            timeline_rows,
            max_event_id,
        })
        .into_string(),
    ))
}

async fn session_history(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> Result<Html<String>, AppError> {
    let sid = SessionId::from(session_id.clone());
    let session = sessions::fetch(&state.pool, &sid)
        .await
        .map_err(AppError::sqlx)?
        .ok_or_else(|| AppError::not_found(format!("no session {session_id}")))?;
    let archived = plans::list_archived_for_session(&state.pool, &sid)
        .await
        .map_err(AppError::sqlx)?;
    Ok(Html(ui::session_history(&session, &archived).into_string()))
}

async fn history_plan_detail(
    State(state): State<AppState>,
    Path((session_id, plan_id)): Path<(String, i64)>,
) -> Result<Html<String>, AppError> {
    let sid = SessionId::from(session_id.clone());
    let session = sessions::fetch(&state.pool, &sid)
        .await
        .map_err(AppError::sqlx)?
        .ok_or_else(|| AppError::not_found(format!("no session {session_id}")))?;
    let plan = plans::fetch(&state.pool, plan_id)
        .await
        .map_err(AppError::sqlx)?
        .ok_or_else(|| AppError::not_found(format!("no plan {plan_id}")))?;
    if plan.session_id != session_id {
        return Err(AppError::not_found(format!(
            "plan {plan_id} not in session {session_id}"
        )));
    }
    let revisions = plan_revisions::list_for_plan(&state.pool, plan.id)
        .await
        .map_err(AppError::sqlx)?;
    let latest_body_html = revisions
        .last()
        .map(|r| render_markdown(&r.body))
        .unwrap_or_default();
    let impl_revisions = impl_revs::list_for_plan(&state.pool, plan.id)
        .await
        .map_err(AppError::sqlx)?;
    let feedback = feedback_store::list_for_plan(&state.pool, plan.id, None)
        .await
        .map_err(AppError::feedback)?;
    let history_feedback = build_history_feedback(&feedback, &revisions);
    Ok(Html(
        ui::history_plan_detail(
            &session,
            &plan,
            &revisions,
            &latest_body_html,
            &impl_revisions,
            &history_feedback,
        )
        .into_string(),
    ))
}

/// Map active-plan feedback into the read-only display shape used by the
/// archived-plan history page.
fn build_history_feedback(
    feedback: &[FeedbackRecord],
    revisions: &[plan_revisions::PlanRevision],
) -> Vec<ui::FeedbackItem> {
    feedback
        .iter()
        .map(|f| {
            let target_label = match &f.target {
                FeedbackTargetRef::PlanRevision(id) => revisions
                    .iter()
                    .find(|r| r.id == *id)
                    .map(|r| format!("rev #{}", r.revision_number)),
                FeedbackTargetRef::ImplementationCommit(sha) => Some(format!(
                    "commit {}",
                    sha.as_str().chars().take(12).collect::<String>()
                )),
            };
            ui::FeedbackItem {
                author_label: f.author_label.as_str().to_string(),
                body: f.body.clone(),
                verdict: crate::review_state::parse_verdict(&f.body),
                created_at: f.created_at,
                updated_at: f.updated_at,
                target_label,
            }
        })
        .collect()
}

#[derive(serde::Deserialize)]
struct CommitDiffQuery {
    #[serde(default)]
    vs: Option<String>,
}

async fn commit_diff(
    State(state): State<AppState>,
    Path((session_id, sha)): Path<(String, String)>,
    axum::extract::Query(query): axum::extract::Query<CommitDiffQuery>,
) -> Result<Html<String>, AppError> {
    let sid = SessionId::from(session_id.clone());
    let session = sessions::fetch(&state.pool, &sid)
        .await
        .map_err(AppError::sqlx)?
        .ok_or_else(|| AppError::not_found(format!("no session {session_id}")))?;
    let plans_in_session = plans::list_for_session(&state.pool, &sid)
        .await
        .map_err(AppError::sqlx)?;
    let mut found: Option<impl_revs::ImplementationRevision> = None;
    let mut plan_impl_rows = Vec::new();
    for p in plans_in_session {
        if let Some(r) = impl_revs::fetch_by_sha(&state.pool, p.id, &CommitSha::from(sha.clone()))
            .await
            .map_err(AppError::sqlx)?
        {
            plan_impl_rows = impl_revs::list_for_plan(&state.pool, p.id)
                .await
                .map_err(AppError::sqlx)?;
            found = Some(r);
            break;
        }
    }
    let rev = found
        .ok_or_else(|| AppError::not_found(format!("commit {sha} not registered in session")))?;
    let repo = std::path::Path::new(&session.repo_root);

    // Resolve the diff base: `?vs=<sha>` overrides the row's `parent_sha`.
    // Validate via `git rev-parse --verify` so a bogus SHA returns 400 rather
    // than a "(failed to compute diff: ...)" placeholder inside the page.
    let (base, base_label) = match query.vs.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(raw) => {
            let full = super::git::resolve_to_full_sha(repo, raw)
                .await
                .map_err(|e| AppError::bad(format!("invalid `vs` SHA `{raw}`: {e}")))?;
            (Some(full.clone()), DiffBaseLabel::Override(full))
        }
        None => (
            rev.parent_sha.clone(),
            DiffBaseLabel::Parent(rev.parent_sha.clone()),
        ),
    };
    let diff = match super::git::diff_text(repo, base.as_deref(), &rev.commit_sha).await {
        Ok(d) => d,
        Err(e) => format!("(failed to compute diff: {e})"),
    };
    let files = super::diff_parser::parse_diff(&diff);
    let is_amend = plan_impl_rows
        .iter()
        .zip(crate::daemon::amend::classify(&plan_impl_rows))
        .any(|(row, info)| {
            row.commit_sha == rev.commit_sha
                && matches!(info, crate::daemon::amend::AmendInfo::Amend { .. })
        });

    let all = feedback_store::list_for_session(&state.pool, &sid)
        .await
        .map_err(AppError::feedback)?;
    let feedback_ctx = state
        .lifecycle
        .build_feedback_context(&sid)
        .await
        .map_err(AppError::lifecycle)?;
    let feedback: Vec<ui::DiffViewFeedback> = all
        .into_iter()
        .filter(|r| matches!(&r.target, FeedbackTargetRef::ImplementationCommit(s) if s.as_str() == rev.commit_sha))
        .map(|r| {
            let (file_status, file_path) =
                diff_feedback_file_meta(&feedback_ctx, FeedbackKind::Impl, r.author_label.as_str());
            ui::DiffViewFeedback {
                feedback_id: r.id,
                author_label: r.author_label.as_str().to_string(),
                feedback_kind: FeedbackKind::Impl.as_str().to_string(),
                verdict: crate::review_state::parse_verdict(&r.body),
                file_status,
                file_path,
                body_html: render_markdown(&r.body),
                created_at: r.created_at,
            }
        })
        .collect();
    Ok(Html(
        ui::commit_diff(&ui::CommitDiffView {
            session_id: session.id,
            commit: rev,
            diff,
            files,
            base_label,
            feedback,
            is_amend,
        })
        .into_string(),
    ))
}

#[derive(Debug, Clone)]
pub enum DiffBaseLabel {
    Parent(Option<String>),
    Override(String),
}

#[derive(serde::Deserialize)]
struct SseQuery {
    #[serde(default)]
    since: Option<i64>,
}

async fn events_stream(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    axum::extract::Query(query): axum::extract::Query<SseQuery>,
    headers: axum::http::HeaderMap,
) -> Result<impl axum::response::IntoResponse, AppError> {
    use axum::response::sse::{Event, KeepAlive, Sse};
    use futures::StreamExt;
    use tokio_stream::wrappers::BroadcastStream;

    let sid = SessionId::from(session_id.clone());
    if sessions::fetch(&state.pool, &sid)
        .await
        .map_err(AppError::sqlx)?
        .is_none()
    {
        return Err(AppError::not_found(format!("no session {session_id}")));
    }
    let supplied_cursor = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
        .or(query.since);
    let initial_cursor = match supplied_cursor {
        Some(cursor) => cursor,
        None => session_max_event_id(&state.pool, &sid).await?,
    };

    let rx = state.lifecycle.subscribe();
    let pool = state.pool.clone();

    let stream = async_stream::stream! {
        let mut cursor = initial_cursor;
        let mut bs = BroadcastStream::new(rx);
        'stream: loop {
            let rows = match session_events_after(&pool, &sid, cursor).await {
                Ok(rows) => rows,
                Err(err) => {
                    tracing::error!(session_id = sid.as_str(), error = ?err, "SSE event query failed");
                    break;
                }
            };
            for ev in rows {
                let fragment = ui::sse_event_fragment(&pool, &sid, &ev).await;
                let id_str = ev.id.to_string();
                let evt: Result<Event, std::convert::Infallible> =
                    Ok(Event::default().id(id_str).data(fragment.into_string()));
                yield evt;
                cursor = ev.id;
            }
            loop {
                match bs.next().await {
                    Some(Ok(received)) if received == sid => break,
                    Some(Ok(_)) => continue,
                    Some(Err(_)) => break,
                    None => break 'stream,
                }
            }
        }
    };

    Ok(Sse::new(Box::pin(stream)
        as std::pin::Pin<
            Box<dyn futures::Stream<Item = Result<Event, std::convert::Infallible>> + Send>,
        >)
    .keep_alive(KeepAlive::default()))
}

async fn home_events_stream(
    State(state): State<AppState>,
    axum::extract::Query(query): axum::extract::Query<SseQuery>,
    headers: axum::http::HeaderMap,
) -> Result<impl axum::response::IntoResponse, AppError> {
    use axum::response::sse::{Event, KeepAlive, Sse};
    use futures::StreamExt;
    use tokio_stream::wrappers::BroadcastStream;

    let supplied_cursor = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
        .or(query.since);
    let initial_cursor = match supplied_cursor {
        Some(cursor) => cursor,
        None => ev_store::max_id(&state.pool)
            .await
            .map_err(AppError::sqlx)?,
    };

    let rx = state.lifecycle.subscribe();
    let stream_state = state.clone();

    let stream = async_stream::stream! {
        let mut cursor = initial_cursor;
        let mut bs = BroadcastStream::new(rx);
        'stream: loop {
            // Merge visibility events and removal events on a shared cursor.
            let visible = match ev_store::active_session_events_after(&stream_state.pool, cursor).await {
                Ok(rows) => rows,
                Err(err) => {
                    tracing::error!(error = ?err, "home SSE visible-events query failed");
                    break;
                }
            };
            let removal = match ev_store::removal_events_after(&stream_state.pool, cursor).await {
                Ok(rows) => rows,
                Err(err) => {
                    tracing::error!(error = ?err, "home SSE removal-events query failed");
                    break;
                }
            };
            let mut merged: Vec<_> = visible.into_iter().chain(removal).collect();
            merged.sort_by_key(|e| e.id);
            merged.dedup_by_key(|e| e.id);
            for ev in merged {
                match home_event_fragment(&stream_state, &ev).await {
                    Ok(fragment) => {
                        let id_str = ev.id.to_string();
                        let evt: Result<Event, std::convert::Infallible> =
                            Ok(Event::default().id(id_str).data(fragment.into_string()));
                        yield evt;
                    }
                    Err(err) => {
                        tracing::warn!(event_id = ev.id, error = ?err.message, "home SSE fragment skipped");
                    }
                }
                cursor = ev.id;
            }
            if bs.next().await.is_none() {
                break 'stream;
            }
        }
    };

    Ok(Sse::new(Box::pin(stream)
        as std::pin::Pin<
            Box<dyn futures::Stream<Item = Result<Event, std::convert::Infallible>> + Send>,
        >)
    .keep_alive(KeepAlive::default()))
}

async fn home_event_fragment(
    state: &AppState,
    ev: &crate::storage::events::Event,
) -> Result<maud::Markup, AppError> {
    let sid = SessionId::from(ev.session_id.clone());
    let session = sessions::fetch(&state.pool, &sid)
        .await
        .map_err(AppError::sqlx)?
        .ok_or_else(|| AppError::not_found(format!("no session {}", ev.session_id)))?;

    // Session-archived removes the row regardless of any timeline payload.
    if ev.kind == "session_archived" {
        return Ok(ui::session_table_row_remove(&ev.session_id));
    }
    if ev.kind == "session_reactivated" {
        return Ok(maud::html! {});
    }

    // Build the table-row update fragment. For "first plan revision" or
    // "reactivated", insert at the top; otherwise replace the row.
    let active_plan_state = if let Some(id) = session.active_plan_id {
        plans::fetch(&state.pool, id)
            .await
            .map_err(AppError::sqlx)?
            .map(|p| p.state)
    } else {
        None
    };
    let finished = if active_plan_state.is_none() {
        let row: Option<(i64,)> = sqlx::query_as(
            "SELECT id FROM plans WHERE session_id = ? AND state = 'finished' \
             ORDER BY finished_at DESC, id DESC LIMIT 1",
        )
        .bind(&session.id)
        .fetch_optional(&state.pool)
        .await
        .map_err(AppError::sqlx)?;
        row.is_some()
    } else {
        false
    };
    let session_row = ui::SessionRow {
        session: session.clone(),
        review_gate: review_gate_for_row(&state, &sid, active_plan_state.as_deref()).await?,
        active_plan_state,
        is_repo_effective: state
            .lifecycle
            .is_repo_effective_session(&sid)
            .await
            .map_err(AppError::lifecycle)?,
        finished,
    };
    let payload: serde_json::Value =
        serde_json::from_str(&ev.payload).unwrap_or(serde_json::Value::Null);
    let prior_presence = home_row_prior_presence(ev, &payload);
    let projection = ui::project_row(prior_presence, ui::RowPresence::Rendered);
    let table_fragment = match projection {
        ui::RowProjection::Insert => ui::session_table_row_insert(&session_row),
        ui::RowProjection::Replace => ui::session_table_row_replace(&session_row),
        ui::RowProjection::Remove => ui::session_table_row_remove(&ev.session_id),
        ui::RowProjection::Noop => maud::html! {},
    };
    let previous_claim_fragment = if ev.kind == "session_claimed"
        && let Some(previous_session_id) =
            payload.get("previous_session_id").and_then(|v| v.as_str())
        && let Some(previous) = sessions::fetch(&state.pool, &SessionId::from(previous_session_id))
            .await
            .map_err(AppError::sqlx)?
    {
        let previous_sid = SessionId::from(previous.id.as_str());
        let previous_active_plan_state = if let Some(id) = previous.active_plan_id {
            plans::fetch(&state.pool, id)
                .await
                .map_err(AppError::sqlx)?
                .map(|p| p.state)
        } else {
            None
        };
        let previous_finished = if previous_active_plan_state.is_none() {
            let row: Option<(i64,)> = sqlx::query_as(
                "SELECT id FROM plans WHERE session_id = ? AND state = 'finished' \
                     ORDER BY finished_at DESC, id DESC LIMIT 1",
            )
            .bind(&previous.id)
            .fetch_optional(&state.pool)
            .await
            .map_err(AppError::sqlx)?;
            row.is_some()
        } else {
            false
        };
        let previous_row = ui::SessionRow {
            review_gate: review_gate_for_row(
                state,
                &previous_sid,
                previous_active_plan_state.as_deref(),
            )
            .await?,
            is_repo_effective: state
                .lifecycle
                .is_repo_effective_session(&previous_sid)
                .await
                .map_err(AppError::lifecycle)?,
            session: previous,
            active_plan_state: previous_active_plan_state,
            finished: previous_finished,
        };
        ui::session_table_row_replace(&previous_row)
    } else {
        maud::html! {}
    };

    // Build the timeline-row OOB. For finished sessions (or any session
    // missing an active plan) the per-event timeline-row build still
    // works as long as the timeline maps include the relevant artifacts.
    let maps = build_timeline_maps(state, &sid).await?;
    let rows = timeline_rows_for_events(&session, std::slice::from_ref(ev), &maps, true);
    let timeline_fragment = rows
        .first()
        .map(|row| ui::timeline_row_oob(row, "home-timeline-feed"))
        .unwrap_or_else(|| maud::html! {});

    Ok(maud::html! {
        (table_fragment)
        (previous_claim_fragment)
        (timeline_fragment)
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HomeRowPayloadProjection {
    Insert,
    Replace,
}

impl HomeRowPayloadProjection {
    fn parse(payload: &serde_json::Value) -> Option<Self> {
        match payload.get("home_row_projection").and_then(|v| v.as_str()) {
            Some("insert") => Some(Self::Insert),
            Some("replace") => Some(Self::Replace),
            _ => None,
        }
    }
}

fn home_row_prior_presence(
    ev: &crate::storage::events::Event,
    payload: &serde_json::Value,
) -> ui::RowPresence {
    match ev.kind.as_str() {
        "session_reactivated" => ui::RowPresence::NotRendered,
        "plan_revision_created"
            if HomeRowPayloadProjection::parse(payload)
                == Some(HomeRowPayloadProjection::Insert) =>
        {
            ui::RowPresence::NotRendered
        }
        _ => ui::RowPresence::Rendered,
    }
}

async fn session_max_event_id(pool: &sqlx::SqlitePool, sid: &SessionId) -> Result<i64, AppError> {
    let max_id: Option<i64> = sqlx::query_scalar("SELECT MAX(id) FROM events WHERE session_id = ?")
        .bind(sid.as_str())
        .fetch_one(pool)
        .await
        .map_err(AppError::sqlx)?;
    Ok(max_id.unwrap_or(0))
}

async fn session_events_after(
    pool: &sqlx::SqlitePool,
    sid: &SessionId,
    cursor: i64,
) -> sqlx::Result<Vec<crate::storage::events::Event>> {
    sqlx::query_as::<_, crate::storage::events::Event>(
        "SELECT * FROM events WHERE session_id = ? AND id > ? ORDER BY id ASC",
    )
    .bind(sid.as_str())
    .bind(cursor)
    .fetch_all(pool)
    .await
}

async fn plan_revision_view(
    State(state): State<AppState>,
    Path((session_id, rev_id)): Path<(String, i64)>,
) -> Result<axum::response::Response, AppError> {
    let sid = SessionId::from(session_id.clone());
    let rev = plan_revisions::fetch_in_session(&state.pool, &session_id, rev_id)
        .await
        .map_err(AppError::sqlx)?
        .ok_or_else(|| AppError::not_found(format!("plan revision {rev_id} not in session")))?;
    let plan = plans::fetch(&state.pool, rev.plan_id)
        .await
        .map_err(AppError::sqlx)?
        .ok_or_else(|| AppError::not_found("plan vanished"))?;
    let prev = plan_revisions::previous_in_plan(&state.pool, plan.id, rev.revision_number)
        .await
        .map_err(AppError::sqlx)?;
    let next: Option<plan_revisions::PlanRevision> =
        sqlx::query_as(
            "SELECT pr.id, s.rowid AS plan_id, pr.revision_number, pr.content_hash, pr.body, pr.created_at \
             FROM plan_revisions pr JOIN sessions s ON s.id = pr.session_id \
             WHERE s.rowid = ? AND pr.revision_number = ?",
        )
            .bind(plan.id)
            .bind(rev.revision_number + 1)
            .fetch_optional(&state.pool)
            .await
            .map_err(AppError::sqlx)?;
    let body_html = render_markdown(&rev.body);
    let feedback = feedback_targeting_plan_revision(&state, &sid, rev.id).await?;
    Ok(Html(
        ui::plan_revision_view(&ui::PlanRevisionView {
            session_id: sid.as_str().to_string(),
            rev,
            prev: prev.as_ref().map(|r| ui::PlanRevisionLink {
                rev_id: r.id,
                revision_number: r.revision_number,
            }),
            next: next.as_ref().map(|r| ui::PlanRevisionLink {
                rev_id: r.id,
                revision_number: r.revision_number,
            }),
            body_html,
            feedback,
        })
        .into_string(),
    )
    .into_response())
}

async fn plan_revision_diff(
    State(state): State<AppState>,
    Path((session_id, rev_id)): Path<(String, i64)>,
) -> Result<axum::response::Response, AppError> {
    let sid = SessionId::from(session_id.clone());
    let cur = plan_revisions::fetch_in_session(&state.pool, &session_id, rev_id)
        .await
        .map_err(AppError::sqlx)?
        .ok_or_else(|| AppError::not_found(format!("plan revision {rev_id} not in session")))?;
    // Revision #1 has nothing to diff against — redirect to the view.
    if cur.revision_number <= 1 {
        return Ok(axum::response::Redirect::to(&format!(
            "/sessions/{session_id}/plan_revisions/{rev_id}"
        ))
        .into_response());
    }
    let prev = plan_revisions::previous_in_plan(&state.pool, cur.plan_id, cur.revision_number)
        .await
        .map_err(AppError::sqlx)?
        .ok_or_else(|| AppError::not_found("previous revision missing"))?;

    let diff = similar::TextDiff::from_lines(&prev.body, &cur.body);
    let mut lines: Vec<ui::DiffLine> = Vec::new();
    for change in diff.iter_all_changes() {
        let kind = match change.tag() {
            similar::ChangeTag::Equal => ui::DiffLineKind::Context,
            similar::ChangeTag::Insert => ui::DiffLineKind::Insert,
            similar::ChangeTag::Delete => ui::DiffLineKind::Delete,
        };
        lines.push(ui::DiffLine {
            kind,
            content: change.to_string(),
        });
    }
    let body_html = render_markdown(&cur.body);
    let feedback = feedback_targeting_plan_revision(&state, &sid, cur.id).await?;
    Ok(Html(
        ui::plan_revision_diff(&ui::PlanRevisionDiffView {
            session_id: sid.as_str().to_string(),
            cur,
            prev_rev_number: prev.revision_number,
            lines,
            body_html,
            feedback,
        })
        .into_string(),
    )
    .into_response())
}

async fn feedback_targeting_plan_revision(
    state: &AppState,
    sid: &SessionId,
    rev_id: i64,
) -> Result<Vec<ui::DiffViewFeedback>, AppError> {
    let all = feedback_store::list_for_session(&state.pool, sid)
        .await
        .map_err(AppError::feedback)?;
    let feedback_ctx = state
        .lifecycle
        .build_feedback_context(sid)
        .await
        .map_err(AppError::lifecycle)?;
    Ok(all
        .into_iter()
        .filter(|r| matches!(&r.target, FeedbackTargetRef::PlanRevision(id) if *id == rev_id))
        .map(|r| {
            let (file_status, file_path) =
                diff_feedback_file_meta(&feedback_ctx, FeedbackKind::Plan, r.author_label.as_str());
            ui::DiffViewFeedback {
                feedback_id: r.id,
                author_label: r.author_label.as_str().to_string(),
                feedback_kind: FeedbackKind::Plan.as_str().to_string(),
                verdict: crate::review_state::parse_verdict(&r.body),
                file_status,
                file_path,
                body_html: render_markdown(&r.body),
                created_at: r.created_at,
            }
        })
        .collect())
}

fn diff_feedback_file_meta(
    ctx: &super::service::FeedbackContext,
    kind: FeedbackKind,
    author_label: &str,
) -> (Option<crate::domain::FeedbackFileStatus>, Option<String>) {
    let files = match kind {
        FeedbackKind::Plan => &ctx.plan_files,
        FeedbackKind::Impl => &ctx.impl_files,
    };
    files
        .iter()
        .find(|s| s.row.author_label == author_label)
        .map(|s| (Some(s.status), Some(s.row.path.clone())))
        .unwrap_or((None, None))
}

fn render_markdown(body: &str) -> String {
    use pulldown_cmark::{Options, Parser, html};
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_FOOTNOTES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    let parser = Parser::new_ext(body, options);
    let mut html_out = String::new();
    html::push_html(&mut html_out, parser);
    ammonia::clean(&html_out)
}

async fn healthz() -> impl IntoResponse {
    "ok"
}

async fn delete_session(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> Result<Response, AppError> {
    let sid = SessionId::from(session_id);
    state
        .lifecycle
        .delete_session(&sid, "system:operator")
        .await
        .map_err(AppError::lifecycle)?;
    Ok(redirect_home())
}

async fn claim_session(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> Result<Response, AppError> {
    let sid = SessionId::from(session_id.clone());
    state
        .lifecycle
        .claim_repo_effective_session(&sid, "system:operator")
        .await
        .map_err(AppError::lifecycle)?;
    Ok(redirect_session(&session_id))
}

async fn finish_session(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> Result<Response, AppError> {
    let sid = SessionId::from(session_id);
    state
        .lifecycle
        .finish_plan(&sid, "system:operator")
        .await
        .map_err(AppError::lifecycle)?;
    Ok(redirect_home())
}

#[derive(serde::Deserialize)]
struct ReviewGateOverrideForm {
    phase: String,
    state: String,
}

async fn review_gate_override(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    axum::extract::Form(form): axum::extract::Form<ReviewGateOverrideForm>,
) -> Result<Response, AppError> {
    let sid = SessionId::from(session_id.clone());
    let phase = ReviewPhase::parse(&form.phase)
        .ok_or_else(|| AppError::bad(format!("invalid review phase `{}`", form.phase)))?;
    let gate_state = ReviewGateState::parse(&form.state)
        .ok_or_else(|| AppError::bad(format!("invalid review gate state `{}`", form.state)))?;
    if matches!(gate_state, ReviewGateState::NeedsReview) {
        return Err(AppError::bad(
            "review gate override must be ready or changes_requested",
        ));
    }
    state
        .lifecycle
        .override_review_gate(&sid, phase, gate_state, "system:operator")
        .await
        .map_err(AppError::lifecycle)?;
    Ok(redirect_session(&session_id))
}

fn redirect_home() -> Response {
    use axum::http::{HeaderValue, StatusCode, header};
    let mut resp = Response::new(axum::body::Body::empty());
    *resp.status_mut() = StatusCode::SEE_OTHER;
    resp.headers_mut()
        .insert(header::LOCATION, HeaderValue::from_static("/"));
    resp
}

fn redirect_session(session_id: &str) -> Response {
    use axum::http::{HeaderValue, StatusCode, header};
    let mut resp = Response::new(axum::body::Body::empty());
    *resp.status_mut() = StatusCode::SEE_OTHER;
    let location = HeaderValue::from_str(&format!("/sessions/{session_id}"))
        .unwrap_or_else(|_| HeaderValue::from_static("/"));
    resp.headers_mut().insert(header::LOCATION, location);
    resp
}

async fn origin_guard(req: Request<axum::body::Body>, next: Next) -> Response {
    let method = req.method().clone();
    if matches!(
        method,
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    ) && !is_loopback(req.headers())
    {
        return (StatusCode::FORBIDDEN, "rejected: non-loopback Origin/Host").into_response();
    }
    next.run(req).await
}

fn is_loopback(headers: &HeaderMap) -> bool {
    if let Some(origin) = headers.get("origin").and_then(|v| v.to_str().ok())
        && !origin_is_loopback(origin)
    {
        return false;
    }
    if let Some(host) = headers.get("host").and_then(|v| v.to_str().ok())
        && !host_is_loopback(host)
    {
        return false;
    }
    true
}

fn origin_is_loopback(origin: &str) -> bool {
    matches!(origin, "null")
        || origin.starts_with("http://localhost:")
        || origin.starts_with("http://127.0.0.1:")
        || origin == "http://localhost"
        || origin == "http://127.0.0.1"
}

fn host_is_loopback(host: &str) -> bool {
    let host_only = host.split(':').next().unwrap_or(host);
    matches!(host_only, "localhost" | "127.0.0.1")
}

struct AppError {
    status: StatusCode,
    message: String,
}

impl AppError {
    fn bad(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: msg.into(),
        }
    }
    fn not_found(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: msg.into(),
        }
    }
    fn sqlx(err: sqlx::Error) -> Self {
        tracing::error!(error = ?err, "sqlx error");
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("database error: {err}"),
        }
    }
    fn feedback(err: feedback_store::Error) -> Self {
        tracing::error!(error = ?err, "feedback storage error");
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("feedback storage: {err}"),
        }
    }
    fn lifecycle(err: super::LifecycleServiceError) -> Self {
        tracing::error!(error = ?err, "lifecycle service error");
        let status = match &err {
            super::LifecycleServiceError::NoSession(_) => StatusCode::NOT_FOUND,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self {
            status,
            message: err.to_string(),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (self.status, self.message).into_response()
    }
}
