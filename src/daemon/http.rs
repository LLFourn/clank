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
use crate::storage::{
    agents, events as ev_store, implementation_revisions as impl_revs, plan_revisions, plans,
};

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(home))
        .route("/healthz", get(healthz))
        .route("/plans/{id}", get(plan_detail))
        .route("/plans/{id}/commits/{sha}", get(commit_diff))
        .route(
            "/plans/{id}/feedback/{event_id}/stage",
            post(curator::stage_feedback),
        )
        .route(
            "/plans/{id}/feedback/{event_id}/unstage",
            post(curator::unstage_feedback),
        )
        .route(
            "/plans/{id}/feedback/{event_id}/edit",
            post(curator::edit_feedback),
        )
        .route(
            "/plans/{id}/feedback/{event_id}/delete",
            post(curator::delete_feedback),
        )
        .route("/plans/{id}/deliver", post(curator::deliver))
        .route("/plans/{id}/comment", post(curator::comment))
        .route("/plans/{id}/approve", post(curator::approve_plan))
        .route("/plans/{id}/mark-done", post(curator::mark_done))
        .route("/plans/{id}/archive", post(curator::archive))
        .route("/plans/{id}/rename", post(curator::rename))
        .route("/plans/{id}/evict-master", post(curator::evict_master))
        .route(
            "/plans/{id}/register-head",
            post(curator::register_head_as_impl),
        )
        .route("/internal/tools", get(internal_api::list_tools))
        .route("/internal/tool_call", post(internal_api::call_tool))
        .layer(middleware::from_fn(origin_guard))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn home(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    let plans = plans::list_active(&state.pool)
        .await
        .map_err(AppError::sqlx)?;
    let mut rows = Vec::with_capacity(plans.len());
    for plan in plans {
        let agents_for_plan = agents::list_for_plan(&state.pool, &plan.id)
            .await
            .map_err(AppError::sqlx)?;
        let master_label = agents_for_plan
            .iter()
            .find(|a| a.role == "master")
            .map(|a| a.label.clone());
        let reviewer_labels: Vec<String> = agents_for_plan
            .iter()
            .filter(|a| a.role == "reviewer")
            .map(|a| a.label.clone())
            .collect();
        let latest = plan_revisions::latest(&state.pool, &plan.id)
            .await
            .map_err(AppError::sqlx)?;
        let pending_plan_count = count_pending(&state, &plan.id, "plan_revision").await?;
        let pending_impl_count = count_pending(&state, &plan.id, "implementation_commit").await?;

        rows.push(ui::HomeRow {
            plan,
            master_label,
            reviewer_labels,
            latest_plan_revision_at: latest.as_ref().map(|r| r.created_at),
            pending_plan_count,
            pending_impl_count,
        });
    }
    Ok(Html(ui::home(&rows).into_string()))
}

async fn plan_detail(
    State(state): State<AppState>,
    Path(plan_id): Path<String>,
) -> Result<Html<String>, AppError> {
    let plan = plans::fetch(&state.pool, &plan_id)
        .await
        .map_err(AppError::sqlx)?
        .ok_or_else(|| AppError::not_found(format!("no plan {plan_id}")))?;
    let agents_for_plan = agents::list_for_plan(&state.pool, &plan_id)
        .await
        .map_err(AppError::sqlx)?;
    let revisions = plan_revisions::list(&state.pool, &plan_id)
        .await
        .map_err(AppError::sqlx)?;
    let latest_body = revisions.last().map(|r| r.body.as_str()).unwrap_or("");
    let latest_body_html = render_markdown(latest_body);

    let raw_plan_feedback: Vec<ev_store::Event> = sqlx::query_as(
        "SELECT * FROM events WHERE plan_id = ? AND kind = 'feedback_added' \
         AND target_kind = 'plan_revision' AND status IN ('pending', 'staged') ORDER BY id ASC",
    )
    .bind(&plan_id)
    .fetch_all(&state.pool)
    .await
    .map_err(AppError::sqlx)?;
    let (plan_pending, plan_staged) = build_feedback_lists_for_plan(&raw_plan_feedback, &revisions);

    let impl_revisions = impl_revs::list(&state.pool, &plan_id)
        .await
        .map_err(AppError::sqlx)?;
    let raw_impl_feedback: Vec<ev_store::Event> = sqlx::query_as(
        "SELECT * FROM events WHERE plan_id = ? AND kind = 'feedback_added' \
         AND target_kind = 'implementation_commit' AND status IN ('pending', 'staged') ORDER BY id ASC",
    )
    .bind(&plan_id)
    .fetch_all(&state.pool)
    .await
    .map_err(AppError::sqlx)?;
    let current_commit_sha = plan.current_implementation_id.and_then(|id| {
        impl_revisions
            .iter()
            .find(|r| r.id == id)
            .map(|r| r.commit_sha.clone())
    });
    let (impl_pending, impl_staged) =
        build_feedback_lists_for_impl(&raw_impl_feedback, current_commit_sha.as_deref());
    drop(current_commit_sha);

    let timeline_events = ev_store::for_plan(&state.pool, &plan_id)
        .await
        .map_err(AppError::sqlx)?;
    let post_impl_drift_count = timeline_events
        .iter()
        .filter(|ev| ev.kind == "plan_file_changed_after_implementation")
        .count() as i64;
    let timeline = timeline_events
        .iter()
        .map(|ev| ui::TimelineItem::from_event(ev, &revisions))
        .collect();

    Ok(Html(
        ui::plan_detail(&ui::PlanDetail {
            plan,
            agents: agents_for_plan,
            revisions,
            latest_body_html,
            plan_feedback_pending: plan_pending,
            plan_feedback_staged: plan_staged,
            impl_revisions,
            impl_feedback_pending: impl_pending,
            impl_feedback_staged: impl_staged,
            timeline,
            post_impl_drift_count,
        })
        .into_string(),
    ))
}

async fn commit_diff(
    State(state): State<AppState>,
    Path((plan_id, sha)): Path<(String, String)>,
) -> Result<Html<String>, AppError> {
    let plan = plans::fetch(&state.pool, &plan_id)
        .await
        .map_err(AppError::sqlx)?
        .ok_or_else(|| AppError::not_found(format!("no plan {plan_id}")))?;
    let rev = impl_revs::fetch_by_sha(&state.pool, &plan_id, &sha)
        .await
        .map_err(AppError::sqlx)?
        .ok_or_else(|| AppError::not_found(format!("commit {sha} not registered for this plan")))?;
    let repo = std::path::Path::new(&plan.repo_root).to_path_buf();
    let diff = match crate::daemon::git::diff_text(
        &repo,
        rev.parent_sha.as_deref(),
        &rev.commit_sha,
    )
    .await
    {
        Ok(d) => d,
        Err(e) => format!("(failed to compute diff: {e})"),
    };

    let raw_feedback: Vec<ev_store::Event> = sqlx::query_as(
        "SELECT * FROM events WHERE plan_id = ? AND kind = 'feedback_added' \
         AND target_kind = 'implementation_commit' AND target_id = ? ORDER BY id ASC",
    )
    .bind(&plan_id)
    .bind(&rev.commit_sha)
    .fetch_all(&state.pool)
    .await
    .map_err(AppError::sqlx)?;
    let feedback = raw_feedback
        .iter()
        .map(impl_feedback_for_diff_view)
        .collect();

    Ok(Html(
        ui::commit_diff(&ui::CommitDiffView {
            plan_id: plan.id,
            commit: rev,
            diff,
            feedback,
        })
        .into_string(),
    ))
}

fn build_feedback_lists_for_plan(
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

fn build_feedback_lists_for_impl(
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
        let outdated = match (ev.target_id.as_deref(), current_sha) {
            (Some(target), Some(current)) => target != current,
            _ => false,
        };
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

fn impl_feedback_for_diff_view(ev: &ev_store::Event) -> ui::DiffViewFeedback {
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

async fn count_pending(
    state: &AppState,
    plan_id: &str,
    target_kind: &str,
) -> Result<i64, AppError> {
    let row: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM events WHERE plan_id = ? AND kind = 'feedback_added' \
         AND target_kind = ? AND status IN ('pending', 'staged')",
    )
    .bind(plan_id)
    .bind(target_kind)
    .fetch_one(&state.pool)
    .await
    .map_err(AppError::sqlx)?;
    Ok(row.0)
}

async fn healthz() -> impl IntoResponse {
    "ok"
}

/// Reject mutating requests whose Origin/Host header isn't a localhost loopback.
async fn origin_guard(req: Request<axum::body::Body>, next: Next) -> Response {
    let method = req.method().clone();
    if matches!(
        method,
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    ) && !is_loopback_request(req.headers())
    {
        return (StatusCode::FORBIDDEN, "rejected: non-loopback Origin/Host").into_response();
    }
    next.run(req).await
}

fn is_loopback_request(headers: &HeaderMap) -> bool {
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
