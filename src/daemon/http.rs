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
use crate::domain::{FeedbackTargetRef, TargetKind};
use crate::lifecycle::{CommitSha, SessionId};
use crate::storage::{
    agents, events as ev_store, feedback as feedback_store, feedback::FeedbackRecord,
    implementation_revisions as impl_revs, plan_revisions, plans, sessions,
};

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(home))
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
    let mut out = Vec::with_capacity(rows.len());
    for s in rows {
        let sid = SessionId::from(s.id.clone());
        let agents_for = agents::list_for_session(&state.pool, &sid)
            .await
            .map_err(AppError::sqlx)?;
        let recent_agents: Vec<String> = agents_for.iter().map(|a| a.label.clone()).collect();
        let active_plan_state = if let Some(id) = s.active_plan_id {
            plans::fetch(&state.pool, id)
                .await
                .map_err(AppError::sqlx)?
                .map(|p| p.state)
        } else {
            None
        };
        let (plan_count, impl_count) = active_feedback_counts(&state, &sid).await?;
        let current_feedback_files: i64 = count_current_feedback_files(&state, &sid).await?;
        out.push(ui::SessionRow {
            session: s,
            recent_agents,
            active_plan_state,
            plan_feedback_count: plan_count,
            impl_feedback_count: impl_count,
            current_feedback_files,
        });
    }
    Ok(Html(ui::home(&out).into_string()))
}

/// Count how many of this session's `feedback_files` rows currently
/// render as the `current` status. Sums across both plan/ and impl/
/// since each kind is judged against its own expected target inside
/// `build_feedback_context`.
async fn count_current_feedback_files(state: &AppState, sid: &SessionId) -> Result<i64, AppError> {
    let ctx = state
        .lifecycle
        .build_feedback_context(sid)
        .await
        .map_err(AppError::lifecycle)?;
    let n = ctx
        .plan_files
        .iter()
        .chain(ctx.impl_files.iter())
        .filter(|s| s.status == crate::domain::FeedbackFileStatus::Current)
        .count();
    Ok(n as i64)
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
    let agents_for = agents::list_for_session(&state.pool, &sid)
        .await
        .map_err(AppError::sqlx)?;
    let active_plan = if let Some(id) = session.active_plan_id {
        plans::fetch(&state.pool, id)
            .await
            .map_err(AppError::sqlx)?
    } else {
        None
    };
    let plan_revisions = if let Some(p) = &active_plan {
        plan_revisions::list_for_plan(&state.pool, p.id)
            .await
            .map_err(AppError::sqlx)?
    } else {
        Vec::new()
    };
    let latest_body_html = plan_revisions
        .last()
        .map(|r| render_markdown(&r.body))
        .unwrap_or_default();
    let impl_revisions = if let Some(p) = &active_plan {
        impl_revs::list_for_plan(&state.pool, p.id)
            .await
            .map_err(AppError::sqlx)?
    } else {
        Vec::new()
    };

    let active_feedback = feedback_store::list_for_active_plan(&state.pool, &sid)
        .await
        .map_err(AppError::feedback)?;
    let active_target = state
        .lifecycle
        .resolve_active_target(&sid)
        .await
        .map_err(AppError::lifecycle)?;
    let current_commit_sha = active_target.as_ref().and_then(|t| match t.kind {
        TargetKind::ImplementationCommit => Some(t.id.clone()),
        _ => None,
    });
    let (plan_feedback, impl_feedback) = build_feedback_items(
        &active_feedback,
        &plan_revisions,
        current_commit_sha.as_deref(),
    );

    let archived_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM plans WHERE session_id = ? AND state = 'archived'",
    )
    .bind(sid.as_str())
    .fetch_one(&state.pool)
    .await
    .map_err(AppError::sqlx)?;

    // Build the lookup map for timeline rendering. Includes the active
    // plan's rows (always present) plus any historical feedback rows
    // referenced by audit events from older plans in the same session.
    // We just list all of session's feedback once and key by id.
    let all_session_feedback = feedback_store::list_for_session(&state.pool, &sid)
        .await
        .map_err(AppError::feedback)?;
    let feedback_by_id: HashMap<i64, FeedbackRecord> = all_session_feedback
        .into_iter()
        .map(|r| (r.id, r))
        .collect();

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

    // Amend chain classification: collect impl revisions across every
    // plan in the session and classify per-plan, then index by commit_sha.
    let mut amend_by_sha: HashMap<String, crate::daemon::amend::AmendInfo> = HashMap::new();
    let all_plans = plans::list_for_session(&state.pool, &sid)
        .await
        .map_err(AppError::sqlx)?;
    for plan in &all_plans {
        let rows = impl_revs::list_for_plan(&state.pool, plan.id)
            .await
            .map_err(AppError::sqlx)?;
        let info = crate::daemon::amend::classify(&rows);
        for (row, ai) in rows.into_iter().zip(info) {
            amend_by_sha.insert(row.commit_sha, ai);
        }
    }

    // Plan revision id → revision number, for timeline action links.
    let mut plan_rev_number_by_id: HashMap<i64, i64> = HashMap::new();
    for rev in &plan_revisions {
        plan_rev_number_by_id.insert(rev.id, rev.revision_number);
    }
    // Also include archived plans' revisions (they may show up in older
    // timeline events).
    for plan in &all_plans {
        if plan.state == "archived" {
            let revs = plan_revisions::list_for_plan(&state.pool, plan.id)
                .await
                .map_err(AppError::sqlx)?;
            for rev in revs {
                plan_rev_number_by_id.insert(rev.id, rev.revision_number);
            }
        }
    }

    // Feedback id → (target_kind, target_id), used by feedback rows to
    // route to the right artifact anchor.
    let mut feedback_target_by_id: HashMap<i64, (String, String)> = HashMap::new();
    for (id, rec) in feedback_by_id.iter() {
        let (k, t) = match &rec.target {
            FeedbackTargetRef::PlanRevision(rev_id) => {
                ("plan_revision".to_string(), rev_id.to_string())
            }
            FeedbackTargetRef::ImplementationCommit(sha) => (
                "implementation_commit".to_string(),
                sha.as_str().to_string(),
            ),
        };
        feedback_target_by_id.insert(*id, (k, t));
    }

    let _ = (
        agents_for,
        latest_body_html,
        impl_revisions,
        plan_feedback,
        impl_feedback,
        feedback_by_id,
    );

    Ok(Html(
        ui::session_detail(&ui::SessionDetail {
            session,
            active_plan,
            archived_count,
            events,
            plan_feedback_files: ctx.plan_files,
            impl_feedback_files: ctx.impl_files,
            git_logs_head_path,
            plan_feedback_dir_path,
            impl_feedback_dir_path,
            amend_by_sha,
            plan_rev_number_by_id,
            feedback_target_by_id,
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
/// archived-plan history page. `outdated` does not apply here (everything
/// archived is "historical"), so it's always false.
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
                feedback_id: f.id,
                author_label: f.author_label.as_str().to_string(),
                body: f.body.clone(),
                created_at: f.created_at,
                updated_at: f.updated_at,
                target_label,
                outdated: false,
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
    for p in plans_in_session {
        if let Some(r) = impl_revs::fetch_by_sha(&state.pool, p.id, &CommitSha::from(sha.clone()))
            .await
            .map_err(AppError::sqlx)?
        {
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

    let all = feedback_store::list_for_session(&state.pool, &sid)
        .await
        .map_err(AppError::feedback)?;
    let feedback: Vec<ui::DiffViewFeedback> = all
        .into_iter()
        .filter(|r| matches!(&r.target, FeedbackTargetRef::ImplementationCommit(s) if s.as_str() == rev.commit_sha))
        .map(|r| ui::DiffViewFeedback {
            feedback_id: r.id,
            author_label: r.author_label.as_str().to_string(),
            body: r.body,
            created_at: r.created_at,
        })
        .collect();
    Ok(Html(
        ui::commit_diff(&ui::CommitDiffView {
            session_id: session.id,
            commit: rev,
            diff,
            base_label,
            feedback,
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
) -> impl axum::response::IntoResponse {
    use axum::response::sse::{Event, KeepAlive, Sse};
    use futures::StreamExt;
    use tokio_stream::wrappers::BroadcastStream;

    let sid = SessionId::from(session_id.clone());
    let initial_cursor: i64 = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
        .or(query.since)
        .unwrap_or(i64::MAX);

    let rx = state.lifecycle.subscribe();
    let pool = state.pool.clone();

    let stream = async_stream::stream! {
        let mut cursor = initial_cursor;
        let mut bs = BroadcastStream::new(rx);
        loop {
            // Block until any session pings. Re-check `cursor` against the
            // DB on any lag too; the cursor handles catch-up naturally.
            match bs.next().await {
                Some(Ok(received)) if received != sid => continue,
                Some(Ok(_)) | Some(Err(_)) => {} // matched session OR lag fall-through
                None => break,
            }
            let rows = sqlx::query_as::<_, crate::storage::events::Event>(
                "SELECT * FROM events WHERE session_id = ? AND id > ? ORDER BY id ASC",
            )
            .bind(sid.as_str())
            .bind(cursor)
            .fetch_all(&pool)
            .await
            .unwrap_or_default();
            for ev in rows {
                let fragment = ui::sse_event_fragment(&pool, &sid, &ev).await;
                let id_str = ev.id.to_string();
                let evt: Result<Event, std::convert::Infallible> =
                    Ok(Event::default().id(id_str).data(fragment.into_string()));
                yield evt;
                cursor = ev.id;
            }
        }
    };

    Sse::new(Box::pin(stream)
        as std::pin::Pin<
            Box<dyn futures::Stream<Item = Result<Event, std::convert::Infallible>> + Send>,
        >)
    .keep_alive(KeepAlive::default())
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
        sqlx::query_as("SELECT * FROM plan_revisions WHERE plan_id = ? AND revision_number = ?")
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
            prev_rev_number: prev.as_ref().map(|r| r.revision_number),
            next_rev_number: next.as_ref().map(|r| r.revision_number),
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
    Ok(all
        .into_iter()
        .filter(|r| matches!(&r.target, FeedbackTargetRef::PlanRevision(id) if *id == rev_id))
        .map(|r| ui::DiffViewFeedback {
            feedback_id: r.id,
            author_label: r.author_label.as_str().to_string(),
            body: r.body,
            created_at: r.created_at,
        })
        .collect())
}

/// Split active-plan feedback rows into (plan_target, impl_target) buckets
/// for the session-detail UI. Impl-target rows whose `target_id` is not
/// the current latest commit get an `outdated` badge.
fn build_feedback_items(
    feedback: &[FeedbackRecord],
    revisions: &[plan_revisions::PlanRevision],
    current_commit_sha: Option<&str>,
) -> (Vec<ui::FeedbackItem>, Vec<ui::FeedbackItem>) {
    let mut plan = Vec::new();
    let mut implv = Vec::new();
    for f in feedback {
        match &f.target {
            FeedbackTargetRef::PlanRevision(rev_id) => {
                let target_label = revisions
                    .iter()
                    .find(|r| r.id == *rev_id)
                    .map(|r| format!("rev #{}", r.revision_number));
                plan.push(ui::FeedbackItem {
                    feedback_id: f.id,
                    author_label: f.author_label.as_str().to_string(),
                    body: f.body.clone(),
                    created_at: f.created_at,
                    updated_at: f.updated_at,
                    target_label,
                    outdated: false,
                });
            }
            FeedbackTargetRef::ImplementationCommit(sha) => {
                let outdated = matches!(current_commit_sha, Some(cur) if cur != sha.as_str());
                let target_label = Some(format!(
                    "commit {}",
                    sha.as_str().chars().take(12).collect::<String>()
                ));
                implv.push(ui::FeedbackItem {
                    feedback_id: f.id,
                    author_label: f.author_label.as_str().to_string(),
                    body: f.body.clone(),
                    created_at: f.created_at,
                    updated_at: f.updated_at,
                    target_label,
                    outdated,
                });
            }
        }
    }
    (plan, implv)
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

async fn active_feedback_counts(state: &AppState, sid: &SessionId) -> Result<(i64, i64), AppError> {
    let active = feedback_store::list_for_active_plan(&state.pool, sid)
        .await
        .map_err(AppError::feedback)?;
    let mut plan = 0_i64;
    let mut implv = 0_i64;
    for f in &active {
        match f.target.kind() {
            TargetKind::PlanRevision => plan += 1,
            TargetKind::ImplementationCommit => implv += 1,
        }
    }
    Ok((plan, implv))
}

async fn healthz() -> impl IntoResponse {
    "ok"
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
