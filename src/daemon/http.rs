//! HTTP routes (session-centric).

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
use crate::lifecycle::{CommitSha, SessionId};
use crate::storage::{
    agents, events as ev_store, implementation_revisions as impl_revs, plan_revisions, plans,
    sessions,
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
            "/sessions/{session_id}/feedback/{event_id}/stage",
            post(curator::stage_feedback),
        )
        .route(
            "/sessions/{session_id}/feedback/{event_id}/unstage",
            post(curator::unstage_feedback),
        )
        .route(
            "/sessions/{session_id}/feedback/{event_id}/edit",
            post(curator::edit_feedback),
        )
        .route(
            "/sessions/{session_id}/feedback/{event_id}/delete",
            post(curator::delete_feedback),
        )
        .route("/sessions/{session_id}/deliver", post(curator::deliver))
        .route("/sessions/{session_id}/comment", post(curator::comment))
        .route("/sessions/{session_id}/archive", post(curator::archive))
        .route("/sessions/{session_id}/rename", post(curator::rename))
        .route(
            "/sessions/{session_id}/evict-master",
            post(curator::evict_master),
        )
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
        let master_label = agents_for
            .iter()
            .find(|a| a.role == "master")
            .map(|a| a.label.clone());
        let reviewer_labels: Vec<String> = agents_for
            .iter()
            .filter(|a| a.role == "reviewer")
            .map(|a| a.label.clone())
            .collect();
        let active_plan_state = if let Some(id) = s.active_plan_id {
            plans::fetch(&state.pool, id)
                .await
                .map_err(AppError::sqlx)?
                .map(|p| p.state)
        } else {
            None
        };
        let (pending_plan, pending_impl) = pending_counts(&state, &sid).await?;
        out.push(ui::SessionRow {
            session: s,
            master_label,
            reviewer_labels,
            active_plan_state,
            pending_plan_count: pending_plan,
            pending_impl_count: pending_impl,
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

    // Feedback queries are scoped to the *active* plan_id, not just the
    // session. Without the plan_id filter, leftover pending/staged feedback
    // from an archived earlier cycle would be visible against a brand-new
    // active plan. (The apply layer also withdraws those events on archive,
    // but the filter is the load-bearing defense.)
    let active_plan_id_for_query: Option<i64> = active_plan.as_ref().map(|p| p.id);
    let raw_plan_feedback: Vec<ev_store::Event> = if let Some(pid) = active_plan_id_for_query {
        sqlx::query_as(
            "SELECT * FROM events WHERE session_id = ? AND plan_id = ? AND kind = 'feedback_added' \
             AND target_kind = 'plan_revision' AND status IN ('pending', 'staged') ORDER BY id ASC",
        )
        .bind(sid.as_str())
        .bind(pid)
        .fetch_all(&state.pool)
        .await
        .map_err(AppError::sqlx)?
    } else {
        Vec::new()
    };
    let (plan_pending, plan_staged) = build_feedback_for_plan(&raw_plan_feedback, &plan_revisions);

    let raw_impl_feedback: Vec<ev_store::Event> = if let Some(pid) = active_plan_id_for_query {
        sqlx::query_as(
            "SELECT * FROM events WHERE session_id = ? AND plan_id = ? AND kind = 'feedback_added' \
             AND target_kind = 'implementation_commit' AND status IN ('pending', 'staged') ORDER BY id ASC",
        )
        .bind(sid.as_str())
        .bind(pid)
        .fetch_all(&state.pool)
        .await
        .map_err(AppError::sqlx)?
    } else {
        Vec::new()
    };
    let current_commit_sha = impl_revisions.last().map(|r| r.commit_sha.clone());
    let (impl_pending, impl_staged) =
        build_feedback_for_impl(&raw_impl_feedback, current_commit_sha.as_deref());

    let archived_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM plans WHERE session_id = ? AND state = 'archived'",
    )
    .bind(sid.as_str())
    .fetch_one(&state.pool)
    .await
    .map_err(AppError::sqlx)?;

    let timeline_events = ev_store::for_session(&state.pool, &sid)
        .await
        .map_err(AppError::sqlx)?;
    let timeline = timeline_events
        .iter()
        .map(|ev| ui::TimelineItem::from_event(ev, &plan_revisions))
        .collect();

    Ok(Html(
        ui::session_detail(&ui::SessionDetail {
            session,
            agents: agents_for,
            active_plan,
            plan_revisions,
            latest_body_html,
            impl_revisions,
            plan_feedback_pending: plan_pending,
            plan_feedback_staged: plan_staged,
            impl_feedback_pending: impl_pending,
            impl_feedback_staged: impl_staged,
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
    Ok(Html(
        ui::history_plan_detail(
            &session,
            &plan,
            &revisions,
            &latest_body_html,
            &impl_revisions,
        )
        .into_string(),
    ))
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
    // Walk plans for this session to find a matching commit_sha.
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
    let raw_feedback: Vec<ev_store::Event> = sqlx::query_as(
        "SELECT * FROM events WHERE session_id = ? AND kind = 'feedback_added' \
         AND target_kind = 'implementation_commit' AND target_id = ? ORDER BY id ASC",
    )
    .bind(sid.as_str())
    .bind(&rev.commit_sha)
    .fetch_all(&state.pool)
    .await
    .map_err(AppError::sqlx)?;
    let feedback = raw_feedback.iter().map(impl_fb).collect();
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

fn build_feedback_for_plan(
    events: &[ev_store::Event],
    revisions: &[plan_revisions::PlanRevision],
) -> (Vec<ui::FeedbackItem>, Vec<ui::FeedbackItem>) {
    let mut pending = Vec::new();
    let mut staged = Vec::new();
    for ev in events {
        let payload: serde_json::Value =
            serde_json::from_str(&ev.payload).unwrap_or(serde_json::Value::Null);
        let text = payload
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let target_label = ev
            .target_id
            .as_deref()
            .and_then(|id| id.parse::<i64>().ok())
            .and_then(|id| revisions.iter().find(|r| r.id == id))
            .map(|r| format!("rev #{}", r.revision_number));
        let item = ui::FeedbackItem {
            event_id: ev.id,
            actor: ev.actor.clone(),
            text,
            status: ev.status.clone().unwrap_or_default(),
            created_at: ev.ts,
            target_label,
            outdated: false,
        };
        match ev.status.as_deref() {
            Some("staged") => staged.push(item),
            _ => pending.push(item),
        }
    }
    (pending, staged)
}

fn build_feedback_for_impl(
    events: &[ev_store::Event],
    current_sha: Option<&str>,
) -> (Vec<ui::FeedbackItem>, Vec<ui::FeedbackItem>) {
    let mut pending = Vec::new();
    let mut staged = Vec::new();
    for ev in events {
        let payload: serde_json::Value =
            serde_json::from_str(&ev.payload).unwrap_or(serde_json::Value::Null);
        let text = payload
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let outdated = matches!(
            (ev.target_id.as_deref(), current_sha),
            (Some(t), Some(c)) if t != c
        );
        let target_label = ev
            .target_id
            .as_deref()
            .map(|sha| format!("commit {}", sha.chars().take(12).collect::<String>()));
        let item = ui::FeedbackItem {
            event_id: ev.id,
            actor: ev.actor.clone(),
            text,
            status: ev.status.clone().unwrap_or_default(),
            created_at: ev.ts,
            target_label,
            outdated,
        };
        match ev.status.as_deref() {
            Some("staged") => staged.push(item),
            _ => pending.push(item),
        }
    }
    (pending, staged)
}

fn impl_fb(ev: &ev_store::Event) -> ui::DiffViewFeedback {
    let payload: serde_json::Value =
        serde_json::from_str(&ev.payload).unwrap_or(serde_json::Value::Null);
    ui::DiffViewFeedback {
        actor: ev.actor.clone(),
        text: payload
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        status: ev.status.clone().unwrap_or_default(),
        created_at: ev.ts,
    }
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

async fn pending_counts(state: &AppState, sid: &SessionId) -> Result<(i64, i64), AppError> {
    // Counts are scoped to the active plan_id, mirroring the feedback display
    // queries. With no active plan there's nothing the curator can stage or
    // deliver, so the counts are zero.
    let active: Option<i64> =
        sqlx::query_scalar("SELECT active_plan_id FROM sessions WHERE id = ?")
            .bind(sid.as_str())
            .fetch_one(&state.pool)
            .await
            .map_err(AppError::sqlx)?;
    let Some(pid) = active else { return Ok((0, 0)) };
    let plan: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM events WHERE session_id = ? AND plan_id = ? AND kind = 'feedback_added' \
         AND target_kind = 'plan_revision' AND status IN ('pending', 'staged')",
    )
    .bind(sid.as_str())
    .bind(pid)
    .fetch_one(&state.pool)
    .await
    .map_err(AppError::sqlx)?;
    let impl_: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM events WHERE session_id = ? AND plan_id = ? AND kind = 'feedback_added' \
         AND target_kind = 'implementation_commit' AND status IN ('pending', 'staged')",
    )
    .bind(sid.as_str())
    .bind(pid)
    .fetch_one(&state.pool)
    .await
    .map_err(AppError::sqlx)?;
    Ok((plan.0, impl_.0))
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
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (self.status, self.message).into_response()
    }
}
