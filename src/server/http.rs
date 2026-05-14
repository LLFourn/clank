//! HTTP routes backed by the filesystem-truth runtime.

use std::path::PathBuf;

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
use super::wait::{WaitArgs, WaitError, wait_for_work};
use crate::lifecycle::SessionId;
use crate::mcp_response::{get_context_response, list_sessions_response};

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(home))
        .route("/healthz", get(healthz))
        .route("/sessions/{session_id}", get(session_detail))
        .route("/sessions/{session_id}/plan/{sha}", get(plan_revision_view))
        .route("/sessions/{session_id}/commit/{sha}", get(commit_diff_view))
        .route("/sessions/{session_id}/done", post(move_to_done))
        .route("/events", get(home_events_stream))
        .route("/sessions/{session_id}/events", get(session_events_stream))
        .route("/internal/tools", get(list_tools))
        .route("/internal/tool_call", post(call_tool))
        .route("/api/wait_for_work", post(api_wait_for_work))
        .with_state(state)
}

async fn api_wait_for_work(
    State(state): State<AppState>,
    axum::Json(args): axum::Json<WaitArgs>,
) -> Result<axum::Json<Value>, AppError> {
    let resp = wait_for_work(&state.runtime, args).await.map_err(|e| match e {
        WaitError::InvalidRole(_) => AppError {
            status: StatusCode::BAD_REQUEST,
            msg: e.to_string(),
        },
        WaitError::Io(err) => AppError::io(err),
    })?;
    let v = serde_json::to_value(resp)
        .map_err(|e| AppError::internal(format!("serialize wait response: {e}")))?;
    Ok(axum::Json(v))
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
    Sse::new(event_stream(state, None).await).keep_alive(sse::KeepAlive::default())
}

async fn session_events_stream(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> Sse<impl Stream<Item = Result<sse::Event, std::convert::Infallible>>> {
    Sse::new(event_stream(state, Some(session_id)).await).keep_alive(sse::KeepAlive::default())
}

/// Push-driven SSE: forwards new events from the broadcast channel.
///
/// Intentionally does **not** replay the ring on connect. Otherwise every
/// page reload would re-fire chimes + reloads for every historical event,
/// causing a refresh loop. The ring buffer is kept for diagnostic /
/// future-API consumers; SSE only carries live events from the moment of
/// subscribe forward.
///
/// `session_filter` (when `Some`) drops events that aren't for that session.
async fn event_stream(
    state: AppState,
    session_filter: Option<String>,
) -> impl Stream<Item = Result<sse::Event, std::convert::Infallible>> {
    use futures::stream::StreamExt;

    let rx = state.runtime.subscribe_events();
    let live_stream = tokio_stream::wrappers::BroadcastStream::new(rx)
        .filter_map(|r| async move { r.ok() });

    let combined = live_stream.filter_map(move |e| {
        let session_filter = session_filter.clone();
        async move {
            if let Some(sid_filter) = session_filter
                && let Some(ref event_sid) = e.session_id
                && event_sid.as_str() != sid_filter
            {
                return None;
            }
            let payload = json!({
                "ts": e.ts,
                "repo": e.repo.to_string_lossy(),
                "session_id": e.session_id.as_ref().map(|s| s.as_str()),
                "kind": e.kind,
                "payload": e.payload,
            });
            Some(Ok(sse::Event::default().data(payload.to_string())))
        }
    });

    Box::pin(combined)
}

async fn healthz() -> &'static str {
    "ok"
}

async fn plan_revision_view(
    State(state): State<AppState>,
    Path((session_id, sha)): Path<(String, String)>,
    Query(q): Query<RepoQuery>,
) -> Result<Html<String>, AppError> {
    let repos = repos_to_render(&state, q.repo).await;
    for repo in repos {
        let plan_path = state
            .runtime
            .read_repo(&repo, |s| {
                s.sessions
                    .get(&SessionId::from(session_id.clone()))
                    .map(|sess| sess.plan_path.clone())
            })
            .await
            .map_err(AppError::runtime)?;
        let Some(plan_path) = plan_path else {
            continue;
        };
        let commit_sha = crate::lifecycle::CommitSha::from(sha.clone());
        let body = crate::git_io::show_blob(&repo, &commit_sha, &plan_path)
            .await
            .map_err(|e| AppError::internal(format!("git show: {e}")))?;
        return Ok(Html(ui::plan_revision_page(&session_id, &sha, &body)));
    }
    Err(AppError::not_found(format!(
        "session {session_id} not found"
    )))
}

async fn commit_diff_view(
    State(state): State<AppState>,
    Path((session_id, sha)): Path<(String, String)>,
    Query(q): Query<RepoQuery>,
) -> Result<Html<String>, AppError> {
    let repos = repos_to_render(&state, q.repo).await;
    for repo in repos {
        let exists = state
            .runtime
            .read_repo(&repo, |s| {
                s.sessions
                    .contains_key(&SessionId::from(session_id.clone()))
            })
            .await
            .map_err(AppError::runtime)?;
        if !exists {
            continue;
        }
        let commit_sha = crate::lifecycle::CommitSha::from(sha.clone());
        let patch = crate::git_io::show_commit(&repo, &commit_sha)
            .await
            .map_err(|e| AppError::internal(format!("git show: {e}")))?;
        return Ok(Html(ui::commit_diff_page(&session_id, &sha, &patch)));
    }
    Err(AppError::not_found(format!(
        "session {session_id} not found"
    )))
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
