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
        .route("/sessions/{session_id}/comment", post(curator::comment))
        .route("/sessions/{session_id}/archive", post(curator::archive))
        .route("/sessions/{session_id}/rename", post(curator::rename))
        .route(
            "/sessions/{session_id}/register-head",
            post(curator::register_head_as_impl),
        )
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
        out.push(ui::SessionRow {
            session: s,
            recent_agents,
            active_plan_state,
            plan_feedback_count: plan_count,
            impl_feedback_count: impl_count,
        });
    }
    Ok(Html(ui::home(&out).into_string()))
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
    let current_commit_sha = impl_revisions.last().map(|r| r.commit_sha.clone());
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

    let timeline_events = ev_store::for_session(&state.pool, &sid)
        .await
        .map_err(AppError::sqlx)?;
    let timeline = timeline_events
        .iter()
        .map(|ev| ui::TimelineItem::from_event(ev, &plan_revisions, &feedback_by_id))
        .collect();

    Ok(Html(
        ui::session_detail(&ui::SessionDetail {
            session,
            agents: agents_for,
            active_plan,
            plan_revisions,
            latest_body_html,
            impl_revisions,
            plan_feedback,
            impl_feedback,
            archived_count,
            timeline,
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

async fn commit_diff(
    State(state): State<AppState>,
    Path((session_id, sha)): Path<(String, String)>,
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
    let diff = match super::git::diff_text(repo, rev.parent_sha.as_deref(), &rev.commit_sha).await {
        Ok(d) => d,
        Err(e) => format!("(failed to compute diff: {e})"),
    };
    // Feedback for this commit lives in the feedback table. Filter all
    // session feedback to rows whose target is the commit_sha (could be
    // under any plan_id — commits are session-scoped in spirit).
    let all = feedback_store::list_for_session(&state.pool, &sid)
        .await
        .map_err(AppError::feedback)?;
    let feedback: Vec<ui::DiffViewFeedback> = all
        .into_iter()
        .filter(|r| matches!(&r.target, FeedbackTargetRef::ImplementationCommit(s) if s.as_str() == rev.commit_sha))
        .map(|r| ui::DiffViewFeedback {
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
            feedback,
        })
        .into_string(),
    ))
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
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (self.status, self.message).into_response()
    }
}
