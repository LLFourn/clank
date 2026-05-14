//! HTTP routes backed by the filesystem-truth runtime.

use std::path::PathBuf;
use std::time::Duration;

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response, Sse, sse};
use axum::routing::{get, post};
use futures::stream::Stream;
use serde::Deserialize;
use serde_json::{Value, json};

use super::AppState;
use super::mcp;
use super::ui;
use crate::lifecycle::SessionId;
use crate::mcp_response::{get_context_response, list_sessions_response};

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(home))
        .route("/healthz", get(healthz))
        .route("/sessions/{session_id}", get(session_detail))
        .route("/sessions/{session_id}/done", post(move_to_done))
        .route("/events", get(home_events_stream))
        .route("/internal/tools", get(list_tools))
        .route("/internal/tool_call", post(call_tool))
        .with_state(state)
}

#[derive(Deserialize)]
struct RepoQuery {
    repo: Option<String>,
}

async fn home(
    State(state): State<AppState>,
    Query(q): Query<RepoQuery>,
) -> Result<Html<String>, AppError> {
    let repos = repos_to_render(&state, q.repo).await;
    let mut combined: Vec<Value> = Vec::new();
    for repo in repos {
        let v = state
            .runtime
            .read_repo(&repo, |s| list_sessions_response(&repo, s))
            .await
            .map_err(AppError::runtime)?
            .map_err(AppError::io)?;
        if let Some(arr) = v.as_array() {
            combined.extend(arr.iter().cloned());
        }
    }
    Ok(Html(ui::home_page(&Value::Array(combined))))
}

async fn session_detail(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Query(q): Query<RepoQuery>,
) -> Result<Html<String>, AppError> {
    let repos = repos_to_render(&state, q.repo).await;
    for repo in repos {
        let v = state
            .runtime
            .read_repo(&repo, |s| {
                get_context_response(
                    &repo,
                    s,
                    &SessionId::from(session_id.clone()),
                    &crate::lifecycle::AgentLabel::from("web".to_string()),
                )
            })
            .await
            .map_err(AppError::runtime)?
            .map_err(AppError::io)?;
        if let Some(ctx) = v {
            return Ok(Html(ui::session_page(&ctx)));
        }
    }
    Err(AppError::not_found(format!(
        "session {session_id} not found"
    )))
}

#[derive(Deserialize)]
struct DoneArgs {
    /// Repo root the session belongs to. Required because session ids are
    /// repo-scoped.
    repo: String,
}

async fn move_to_done(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    axum::Form(args): axum::Form<DoneArgs>,
) -> Result<Response, AppError> {
    let repo = PathBuf::from(&args.repo);
    let plan_path_rel = state
        .runtime
        .read_repo(&repo, |s| {
            s.sessions
                .get(&SessionId::from(session_id.clone()))
                .map(|sess| sess.plan_path.clone())
        })
        .await
        .map_err(AppError::runtime)?
        .ok_or_else(|| AppError::not_found(format!("session {session_id} not found")))?;

    let from = repo.join(&plan_path_rel);
    let to_dir = repo.join(".trinity/plans/done");
    std::fs::create_dir_all(&to_dir).map_err(AppError::io)?;
    let to = to_dir.join(
        plan_path_rel
            .file_name()
            .ok_or_else(|| AppError::internal("plan_path has no file name"))?,
    );
    std::fs::rename(&from, &to).map_err(AppError::io)?;
    // Don't `git add` — operator commits the move themselves per the plan.
    Ok((
        StatusCode::SEE_OTHER,
        [(axum::http::header::LOCATION, "/")],
    )
        .into_response())
}

async fn home_events_stream(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<sse::Event, std::convert::Infallible>>> {
    use futures::stream::StreamExt;
    use tokio::time::interval;
    use tokio_stream::wrappers::IntervalStream;

    // Poll-based SSE: every 500ms, emit the current ring snapshot.
    // A push-driven version using broadcast channels is a follow-up.
    let runtime = state.runtime.clone();
    let stream = IntervalStream::new(interval(Duration::from_millis(500))).then(move |_| {
        let runtime = runtime.clone();
        async move {
            let events = runtime.live_events_snapshot().await;
            let payload = json!({
                "events": events.iter().map(|e| json!({
                    "ts": e.ts,
                    "repo": e.repo.to_string_lossy(),
                    "session_id": e.session_id.as_ref().map(|s| s.as_str()),
                    "kind": e.kind,
                })).collect::<Vec<_>>()
            });
            Ok(sse::Event::default().data(payload.to_string()))
        }
    });
    Sse::new(stream).keep_alive(sse::KeepAlive::default())
}

async fn healthz() -> &'static str {
    "ok"
}

async fn list_tools() -> axum::Json<Vec<crate::tools::ToolDescriptor>> {
    axum::Json(crate::tools::catalog())
}

async fn call_tool(
    State(state): State<AppState>,
    axum::Json(req): axum::Json<mcp::ToolCallRequest>,
) -> Result<axum::Json<mcp::ToolCallResponse>, AppError> {
    let result = mcp::dispatch(&state, &req).await.map_err(AppError::tool)?;
    Ok(axum::Json(mcp::ToolCallResponse { result }))
}

/// Best-effort: resolve `?repo=<path>` to a list of repo roots to render,
/// or all known repos if `repo` is absent.
async fn repos_to_render(state: &AppState, override_path: Option<String>) -> Vec<PathBuf> {
    if let Some(s) = override_path {
        return vec![PathBuf::from(s)];
    }
    let arc = state.runtime.state();
    let trinity = arc.lock().await;
    trinity.repos.keys().cloned().collect()
}

#[derive(Debug)]
struct AppError {
    status: StatusCode,
    msg: String,
}

impl AppError {
    fn not_found(msg: String) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            msg,
        }
    }
    fn io(err: std::io::Error) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            msg: format!("io: {err}"),
        }
    }
    fn runtime(err: crate::runtime::RuntimeError) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            msg: format!("runtime: {err}"),
        }
    }
    fn internal(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            msg: msg.into(),
        }
    }
    fn tool(err: mcp::ToolError) -> Self {
        let status = match err {
            mcp::ToolError::Invalid(_) => StatusCode::BAD_REQUEST,
            mcp::ToolError::NotFound(_) => StatusCode::NOT_FOUND,
            mcp::ToolError::Forbidden(_) => StatusCode::FORBIDDEN,
            mcp::ToolError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self {
            status,
            msg: err.to_string(),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (self.status, self.msg).into_response()
    }
}
