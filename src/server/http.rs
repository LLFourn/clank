//! HTTP routes backed by the filesystem-truth runtime.

use std::path::PathBuf;
use std::sync::Arc;

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
use super::wait::{WaitArgs, WaitError, wait_for_work};
use crate::lifecycle::SessionId;

pub fn router(state: AppState) -> Router {
    // Phase 1 axum routing order (per .trinity/plans/leptos-frontend.md):
    //   1. /api/*           — UI surface (JSON only).
    //   2. /static/*        — Leptos bundle assets via ServeDir.
    //   3. /events          — SSE.
    //   4. /internal/*      — MCP dispatch + tool catalog.
    //   5. /healthz         — readiness probe.
    //   6. Transitional maud routes — `/sessions/...` paths still served
    //      by the old src/server/ui.rs handlers; deleted in Phase 5.
    //   7. SPA fallback     — anything else returns frontend/dist/index.html.
    let static_service = tower_http::services::ServeDir::new(state.frontend_dist.clone());
    let spa_shell = state.spa_shell.clone();
    let frontend_dist = state.frontend_dist.clone();
    Router::new()
        .route("/healthz", get(healthz))
        // Phase 5: the maud /sessions/:id* handlers are gone. The SPA
        // owns those paths via the fallback; the done action moves
        // under /api as a structured POST.
        .route("/events", get(home_events_stream))
        .route("/sessions/{session_id}/events", get(session_events_stream))
        .route("/internal/tools", get(list_tools))
        .route("/internal/tool_call", post(call_tool))
        .route("/api/wait_for_work", post(api_wait_for_work))
        .route("/api/sessions", get(api_sessions))
        .route("/api/sessions/{session_id}", get(api_session_detail))
        .route("/api/sessions/{session_id}/done", post(api_move_to_done))
        .route(
            "/api/sessions/{session_id}/plan/{sha}",
            get(api_plan_revision),
        )
        .route(
            "/api/sessions/{session_id}/commit/{sha}",
            get(api_commit_diff),
        )
        .route("/api/diff", get(api_diff))
        .nest_service("/static", static_service)
        .fallback(move || serve_spa_shell(spa_shell.clone(), frontend_dist.clone()))
        .with_state(state)
}

/// Serve the cached Leptos shell (read once at boot into `AppState`).
/// Falls back to a 503 with a build hint when the cache is empty (i.e.
/// `frontend_dist/index.html` was missing at startup).
async fn serve_spa_shell(shell: Option<Arc<String>>, frontend_dist: PathBuf) -> Response {
    match shell {
        Some(body) => Html((*body).clone()).into_response(),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            format!(
                "Leptos bundle not found at {}/index.html. Run `trunk build` in frontend/ or set --frontend-dist.",
                frontend_dist.display()
            ),
        )
            .into_response(),
    }
}

async fn api_wait_for_work(
    State(state): State<AppState>,
    axum::Json(args): axum::Json<WaitArgs>,
) -> Result<axum::Json<Value>, AppError> {
    let resp = wait_for_work(&state.runtime, args)
        .await
        .map_err(|e| match e {
            WaitError::InvalidRole(_)
            | WaitError::MissingSessionId
            | WaitError::MissingAuthorLabel
            | WaitError::MissingRepo => AppError {
                status: StatusCode::BAD_REQUEST,
                msg: e.to_string(),
            },
            WaitError::UnknownSession(_) => AppError::not_found(e.to_string()),
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
    let live_stream =
        tokio_stream::wrappers::BroadcastStream::new(rx).filter_map(|r| async move { r.ok() });

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

// ---------------------------------------------------------------------------
// /api/* — UI-only JSON surface. Distinct shapes from mcp_response::* per the
// surface-separation rule in .trinity/plans/leptos-frontend.md. All handlers
// snapshot under the runtime mutex via Runtime::snapshot_*, release the
// lock, then build the response — no disk I/O happens under the mutex.

async fn api_sessions(
    State(state): State<AppState>,
    Query(q): Query<RepoQuery>,
) -> Result<axum::Json<Value>, AppError> {
    let repos = repos_to_render(&state, q.repo).await;
    let mut combined: Vec<Value> = Vec::new();
    for repo in repos {
        let snapshot = state
            .runtime
            .snapshot_repo(&repo)
            .await
            .map_err(AppError::runtime)?;
        let v = crate::ui_response::sessions_index(&snapshot).map_err(AppError::io)?;
        if let Some(arr) = v.as_array() {
            combined.extend(arr.iter().cloned());
        }
    }
    Ok(axum::Json(Value::Array(combined)))
}

async fn api_session_detail(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Query(q): Query<RepoQuery>,
) -> Result<axum::Json<Value>, AppError> {
    let repos = repos_to_render(&state, q.repo).await;
    for repo in repos {
        let snapshot = state
            .runtime
            .snapshot_session(&repo, &SessionId::from(session_id.clone()))
            .await
            .map_err(AppError::runtime)?;
        if let Some(snapshot) = snapshot {
            let v = crate::ui_response::session_page(&snapshot).map_err(AppError::io)?;
            return Ok(axum::Json(v));
        }
    }
    Err(AppError::not_found(format!(
        "session {session_id} not found"
    )))
}

async fn api_plan_revision(
    State(state): State<AppState>,
    Path((session_id, sha)): Path<(String, String)>,
    Query(q): Query<RepoQuery>,
) -> Result<axum::Json<Value>, AppError> {
    let commit_sha = crate::lifecycle::CommitSha::from(sha.clone());
    let repos = repos_to_render(&state, q.repo).await;
    for repo in repos {
        let snapshot = state
            .runtime
            .snapshot_session(&repo, &SessionId::from(session_id.clone()))
            .await
            .map_err(AppError::runtime)?;
        let Some(snapshot) = snapshot else { continue };

        // Session-scoped endpoint: require the SHA to be one of THIS
        // session's plan-touching commits. Without this guard, an
        // arbitrary blob in the repo could be rendered through any
        // session URL, leaking commits across session boundaries.
        let plan_revisions = crate::projection::all_plan_revisions_for(
            &snapshot.session.id,
            &snapshot.commit_order,
            &snapshot.plan_touches,
        );
        let Some(pos) = plan_revisions.iter().position(|c| c == &commit_sha) else {
            return Err(AppError::not_found(format!(
                "commit {sha} is not a plan revision of session {session_id}"
            )));
        };

        let body_raw = crate::git_io::show_blob(&repo, &commit_sha, &snapshot.session.plan_path)
            .await
            .map_err(|e| AppError::internal(format!("git show: {e}")))?;
        let body_html = crate::ui_response::render_markdown(&body_raw);

        let previous_sha = pos
            .checked_sub(1)
            .and_then(|j| plan_revisions.get(j))
            .map(|c| c.as_str().to_string());
        let next_sha = plan_revisions
            .get(pos + 1)
            .map(|c| c.as_str().to_string());

        let feedback = crate::ui_response::feedback_for_target(&snapshot.session, &commit_sha);
        return Ok(axum::Json(json!({
            "repo": snapshot.root.to_string_lossy(),
            "session_id": snapshot.session.id.as_str(),
            "commit_sha": commit_sha.as_str(),
            "body_raw": body_raw,
            "body_html": body_html,
            "plan_intro": snapshot.session.plan_intro.as_str(),
            "plan_intro_parent": snapshot.session.plan_intro_parent.as_ref().map(|s| s.as_str()),
            "previous_sha": previous_sha,
            "next_sha": next_sha,
            "feedback": feedback,
        })));
    }
    Err(AppError::not_found(format!(
        "session {session_id} not found"
    )))
}

async fn api_commit_diff(
    State(state): State<AppState>,
    Path((session_id, sha)): Path<(String, String)>,
    Query(q): Query<RepoQuery>,
) -> Result<axum::Json<Value>, AppError> {
    let commit_sha = crate::lifecycle::CommitSha::from(sha.clone());
    let repos = repos_to_render(&state, q.repo).await;
    for repo in repos {
        let snapshot = state
            .runtime
            .snapshot_session(&repo, &SessionId::from(session_id.clone()))
            .await
            .map_err(AppError::runtime)?;
        let Some(snapshot) = snapshot else { continue };

        // Session-scoped endpoint: refuse to render an arbitrary commit
        // diff unless the SHA is attributed to THIS session (covers both
        // pure-impl and mixed plan+impl commits).
        let belongs_to_session = matches!(
            snapshot.attribution.get(&commit_sha),
            Some(crate::repo_state::AttributionResult::Attributed { session, .. })
                if session == &snapshot.session.id
        );
        if !belongs_to_session {
            return Err(AppError::not_found(format!(
                "commit {sha} is not attributed to session {session_id}"
            )));
        }

        let patch = crate::git_io::show_commit(&repo, &commit_sha)
            .await
            .map_err(|e| AppError::internal(format!("git show: {e}")))?;
        let diff_files = crate::diff_parser::parse_diff(&patch);
        let diff_files_json: Vec<Value> = diff_files
            .iter()
            .map(|f| {
                json!({
                    "path": f.path,
                    "old_path": f.old_path,
                    "additions": f.additions,
                    "deletions": f.deletions,
                    "mode": match f.mode {
                        crate::diff_parser::FileDiffMode::Added => "added",
                        crate::diff_parser::FileDiffMode::Removed => "removed",
                        crate::diff_parser::FileDiffMode::Renamed => "renamed",
                        crate::diff_parser::FileDiffMode::Modified => "modified",
                    },
                    "binary": f.binary,
                    "always_folded": crate::diff_parser::is_always_folded(&f.path),
                    "hunks": f.hunks.iter().map(|h| json!({
                        "header": h.header,
                        "lines": h.lines.iter().map(|l| json!({
                            "kind": match l.kind {
                                crate::diff_parser::DiffLineKind::Insert => "insert",
                                crate::diff_parser::DiffLineKind::Delete => "delete",
                                crate::diff_parser::DiffLineKind::Context => "context",
                                crate::diff_parser::DiffLineKind::Meta => "meta",
                            },
                            "old_lineno": l.old_lineno,
                            "new_lineno": l.new_lineno,
                            "content": l.content,
                        })).collect::<Vec<_>>(),
                    })).collect::<Vec<_>>(),
                })
            })
            .collect();

        let feedback = crate::ui_response::feedback_for_target(&snapshot.session, &commit_sha);
        return Ok(axum::Json(json!({
            "repo": snapshot.root.to_string_lossy(),
            "session_id": snapshot.session.id.as_str(),
            "commit_sha": commit_sha.as_str(),
            "diff_files": diff_files_json,
            "feedback": feedback,
        })));
    }
    Err(AppError::not_found(format!(
        "session {session_id} not found"
    )))
}

#[derive(Deserialize)]
struct DoneBody {
    /// Repo root the session belongs to. Required because session ids
    /// are repo-scoped.
    repo: String,
}

/// `POST /api/sessions/:session_id/done` — move the plan file under
/// `.trinity/plans/done/`. Replaces the Phase-1 form-encoded
/// `/sessions/:id/done` POST. Body is JSON.
async fn api_move_to_done(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    axum::Json(body): axum::Json<DoneBody>,
) -> Result<axum::Json<Value>, AppError> {
    let repo = PathBuf::from(&body.repo);
    let plan_path_rel = state
        .runtime
        .snapshot_session(&repo, &SessionId::from(session_id.clone()))
        .await
        .map_err(AppError::runtime)?
        .map(|snapshot| snapshot.session.plan_path)
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
    let new_plan_path = to.strip_prefix(&repo).unwrap_or(&to).to_path_buf();
    Ok(axum::Json(json!({
        "ok": true,
        "new_plan_path": new_plan_path.to_string_lossy(),
    })))
}

#[derive(Deserialize)]
struct DiffQuery {
    from: String,
    to: String,
    path: String,
    repo: Option<String>,
}

/// `GET /api/diff?from=&to=&path=&repo=` — patch between two SHAs at one
/// file. Used by `<PlanDiff/>` to compare two plan-body revisions.
/// Intentionally not session-scoped (the SPA only renders diffs over
/// `session.plan_path`, but the endpoint is general).
async fn api_diff(
    State(state): State<AppState>,
    Query(q): Query<DiffQuery>,
) -> Result<axum::Json<Value>, AppError> {
    let from = crate::lifecycle::CommitSha::from(q.from);
    let to = crate::lifecycle::CommitSha::from(q.to);
    let path = std::path::PathBuf::from(&q.path);
    let repos = repos_to_render(&state, q.repo).await;
    for repo in repos {
        // Cheap reachability check: the snapshot ensures the repo is
        // actually known to the runtime (saves us from spawning git
        // against a stray query path).
        if state
            .runtime
            .snapshot_repo(&repo)
            .await
            .map_err(AppError::runtime)
            .is_err()
        {
            continue;
        }
        let patch = crate::git_io::diff_two_blobs(&repo, &from, &to, &path)
            .await
            .map_err(|e| AppError::internal(format!("git diff: {e}")))?;
        let diff_files = crate::diff_parser::parse_diff(&patch);
        let diff_files_json = serialize_diff_files(&diff_files);
        return Ok(axum::Json(json!({
            "repo": repo.to_string_lossy(),
            "from": from.as_str(),
            "to": to.as_str(),
            "path": path.to_string_lossy(),
            "diff_files": diff_files_json,
        })));
    }
    Err(AppError::not_found("no matching repo".to_string()))
}

/// Shared diff_files → JSON converter used by `api_commit_diff` and
/// `api_diff`. Returns a Vec<Value> so callers wrap it in their own
/// envelope.
fn serialize_diff_files(files: &[crate::diff_parser::FileDiff]) -> Vec<Value> {
    files
        .iter()
        .map(|f| {
            json!({
                "path": f.path,
                "old_path": f.old_path,
                "additions": f.additions,
                "deletions": f.deletions,
                "mode": match f.mode {
                    crate::diff_parser::FileDiffMode::Added => "added",
                    crate::diff_parser::FileDiffMode::Removed => "removed",
                    crate::diff_parser::FileDiffMode::Renamed => "renamed",
                    crate::diff_parser::FileDiffMode::Modified => "modified",
                },
                "binary": f.binary,
                "always_folded": crate::diff_parser::is_always_folded(&f.path),
                "hunks": f.hunks.iter().map(|h| json!({
                    "header": h.header,
                    "lines": h.lines.iter().map(|l| json!({
                        "kind": match l.kind {
                            crate::diff_parser::DiffLineKind::Insert => "insert",
                            crate::diff_parser::DiffLineKind::Delete => "delete",
                            crate::diff_parser::DiffLineKind::Context => "context",
                            crate::diff_parser::DiffLineKind::Meta => "meta",
                        },
                        "old_lineno": l.old_lineno,
                        "new_lineno": l.new_lineno,
                        "content": l.content,
                    })).collect::<Vec<_>>(),
                })).collect::<Vec<_>>(),
            })
        })
        .collect()
}

#[cfg(test)]
mod wire_tests {
    //! Wire-level tests against the actual axum router. Covers
    //! `POST /api/wait_for_work` (HTTP) and `POST /internal/tool_call`
    //! with `tool: "wait_for_work"` (MCP dispatch). Uses `tower::ServiceExt::oneshot`
    //! to drive the router in-process without binding a port.

    use super::*;
    use crate::runtime::Runtime;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use serde_json::json;
    use std::collections::HashSet;
    use std::path::Path;
    use std::process::Command;
    use std::sync::Arc;
    use tokio::sync::Mutex;
    use tower::ServiceExt;

    fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        run_git(path, &["init", "--quiet", "--initial-branch=main"]);
        run_git(path, &["config", "user.email", "test@test"]);
        run_git(path, &["config", "user.name", "test"]);
        run_git(path, &["config", "commit.gpgsign", "false"]);
        dir
    }

    fn run_git(cwd: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success());
    }

    fn write_file(repo: &Path, rel: &str, body: &str) {
        let abs = repo.join(rel);
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(abs, body).unwrap();
    }

    fn commit(repo: &Path, msg: &str) {
        run_git(repo, &["add", "-A"]);
        run_git(repo, &["commit", "--quiet", "-m", msg]);
    }

    /// Commits a `foo` plan into `dir`, registers it with a fresh runtime,
    /// and returns the runtime alongside an `AppState` that wraps it. The
    /// `Arc<Runtime>` handle is for tests that need to drive
    /// `handle_signal` / `read_repo` directly between setup and request;
    /// the `AppState` feeds straight into `router(...)`.
    async fn prepared_state(dir: &tempfile::TempDir) -> (Arc<Runtime>, AppState) {
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "add foo");
        let runtime = Arc::new(Runtime::new());
        runtime.add_repo(dir.path().to_path_buf()).await.unwrap();
        let state = AppState {
            runtime: Arc::clone(&runtime),
            watchers: Arc::new(Mutex::new(Vec::new())),
            watched_repos: Arc::new(Mutex::new(HashSet::new())),
            frontend_dist: std::path::PathBuf::from("frontend/dist"),
            spa_shell: None,
        };
        (runtime, state)
    }

    async fn router_with_repo(dir: &tempfile::TempDir) -> axum::Router {
        let (_, state) = prepared_state(dir).await;
        router(state)
    }

    async fn body_json(resp: axum::response::Response) -> serde_json::Value {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn body_text(resp: axum::response::Response) -> String {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn http_returns_work_for_immediate_match() {
        let dir = init_repo();
        let app = router_with_repo(&dir).await;
        let body = json!({
            "role": "reviewers",
            "session_id": "foo",
            "author_label": "codex",
            "repo": dir.path().to_string_lossy(),
            "timeout_secs": 1,
        });
        let req = Request::builder()
            .method("POST")
            .uri("/api/wait_for_work")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["work"], "review_plan");
        let locations = v["locations"].as_array().unwrap();
        assert_eq!(locations.len(), 1);
        assert!(locations[0].as_str().unwrap().ends_with("/codex.md"));
    }

    #[tokio::test]
    async fn http_timeout_returns_timed_out_shape() {
        let dir = init_repo();
        let app = router_with_repo(&dir).await;
        // Polling master while only reviewers have work → timeout.
        let body = json!({
            "role": "master",
            "session_id": "foo",
            "author_label": "lloyd",
            "repo": dir.path().to_string_lossy(),
            "timeout_secs": 1,
        });
        let req = Request::builder()
            .method("POST")
            .uri("/api/wait_for_work")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["timed_out"], true);
        assert!(v.get("work").is_none());
        assert!(v.get("locations").is_none());
    }

    #[tokio::test]
    async fn http_400_on_invalid_role() {
        let dir = init_repo();
        let app = router_with_repo(&dir).await;
        let body = json!({
            "role": "reviewer",
            "session_id": "foo",
            "author_label": "codex",
            "repo": dir.path().to_string_lossy(),
            "timeout_secs": 1,
        });
        let req = Request::builder()
            .method("POST")
            .uri("/api/wait_for_work")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let msg = body_text(resp).await;
        assert!(msg.contains("invalid role"), "got: {msg}");
    }

    #[tokio::test]
    async fn http_400_on_missing_repo() {
        let dir = init_repo();
        let app = router_with_repo(&dir).await;
        let body = json!({
            "role": "reviewers",
            "session_id": "foo",
            "author_label": "codex",
            "timeout_secs": 1,
        });
        let req = Request::builder()
            .method("POST")
            .uri("/api/wait_for_work")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn http_400_on_missing_author_label() {
        let dir = init_repo();
        let app = router_with_repo(&dir).await;
        let body = json!({
            "role": "reviewers",
            "session_id": "foo",
            "repo": dir.path().to_string_lossy(),
            "timeout_secs": 1,
        });
        let req = Request::builder()
            .method("POST")
            .uri("/api/wait_for_work")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn http_404_on_unknown_session() {
        let dir = init_repo();
        let app = router_with_repo(&dir).await;
        let body = json!({
            "role": "reviewers",
            "session_id": "does-not-exist",
            "author_label": "codex",
            "repo": dir.path().to_string_lossy(),
            "timeout_secs": 1,
        });
        let req = Request::builder()
            .method("POST")
            .uri("/api/wait_for_work")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn mcp_dispatch_returns_work_with_repo_arg() {
        let dir = init_repo();
        let app = router_with_repo(&dir).await;
        let body = json!({
            "cwd": dir.path().to_string_lossy(),
            "tool": "wait_for_work",
            "arguments": {
                "role": "reviewers",
                "session_id": "foo",
                "author_label": "codex",
                "repo": dir.path().to_string_lossy(),
                "timeout_secs": 1,
            },
        });
        let req = Request::builder()
            .method("POST")
            .uri("/internal/tool_call")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["result"]["work"], "review_plan");
    }

    #[tokio::test]
    async fn mcp_dispatch_falls_back_to_cwd_when_repo_omitted() {
        let dir = init_repo();
        let app = router_with_repo(&dir).await;
        // No `repo` arg — dispatcher should resolve from cwd via git
        // rev-parse on the tempdir.
        let body = json!({
            "cwd": dir.path().to_string_lossy(),
            "tool": "wait_for_work",
            "arguments": {
                "role": "reviewers",
                "session_id": "foo",
                "author_label": "codex",
                "timeout_secs": 1,
            },
        });
        let req = Request::builder()
            .method("POST")
            .uri("/internal/tool_call")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["result"]["work"], "review_plan");
    }

    #[tokio::test]
    async fn mcp_dispatch_400_on_invalid_role() {
        let dir = init_repo();
        let app = router_with_repo(&dir).await;
        let body = json!({
            "cwd": dir.path().to_string_lossy(),
            "tool": "wait_for_work",
            "arguments": {
                "role": "reviewer",
                "session_id": "foo",
                "author_label": "codex",
                "timeout_secs": 1,
            },
        });
        let req = Request::builder()
            .method("POST")
            .uri("/internal/tool_call")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn mcp_dispatch_400_on_missing_author_label() {
        // The schema is now optional on author_label, but the daemon
        // still rejects calls that arrive without one (no shim cache in
        // a direct /internal/tool_call test).
        let dir = init_repo();
        let app = router_with_repo(&dir).await;
        let body = json!({
            "cwd": dir.path().to_string_lossy(),
            "tool": "wait_for_work",
            "arguments": {
                "role": "reviewers",
                "session_id": "foo",
                "timeout_secs": 1,
            },
        });
        let req = Request::builder()
            .method("POST")
            .uri("/internal/tool_call")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let msg = body_text(resp).await;
        assert!(
            msg.contains("author_label"),
            "expected author_label in error, got: {msg}"
        );
    }

    #[tokio::test]
    async fn mcp_dispatch_timeout_returns_envelope_with_timed_out() {
        let dir = init_repo();
        let app = router_with_repo(&dir).await;
        let body = json!({
            "cwd": dir.path().to_string_lossy(),
            "tool": "wait_for_work",
            "arguments": {
                "role": "master",
                "session_id": "foo",
                "author_label": "lloyd",
                "timeout_secs": 1,
            },
        });
        let req = Request::builder()
            .method("POST")
            .uri("/internal/tool_call")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        // MCP envelope wraps the wait response under `result`.
        assert_eq!(v["result"]["timed_out"], true);
        assert!(v["result"].get("work").is_none());
        assert!(v["result"].get("locations").is_none());
    }

    #[tokio::test]
    async fn http_and_mcp_bodies_byte_identical_for_same_input() {
        // Acceptance: HTTP and MCP dispatch produce byte-identical JSON
        // responses for the same inputs (modulo MCP's {result: ...}
        // envelope). Drive both routes against the *same* router (both
        // Router and AppState are Clone) so the underlying state — and
        // therefore the SHA-bearing paths in `locations` — match
        // exactly. Full Value equality, not key-set parity.
        let dir = init_repo();
        let app = router_with_repo(&dir).await;
        let args = json!({
            "role": "reviewers",
            "session_id": "foo",
            "author_label": "codex",
            "repo": dir.path().to_string_lossy(),
            "timeout_secs": 1,
        });

        let http_req = Request::builder()
            .method("POST")
            .uri("/api/wait_for_work")
            .header("content-type", "application/json")
            .body(Body::from(args.to_string()))
            .unwrap();
        let http_resp = app.clone().oneshot(http_req).await.unwrap();
        assert_eq!(http_resp.status(), StatusCode::OK);
        let http_body = body_json(http_resp).await;

        let mcp_body_in = json!({
            "cwd": dir.path().to_string_lossy(),
            "tool": "wait_for_work",
            "arguments": args,
        });
        let mcp_req = Request::builder()
            .method("POST")
            .uri("/internal/tool_call")
            .header("content-type", "application/json")
            .body(Body::from(mcp_body_in.to_string()))
            .unwrap();
        let mcp_resp = app.oneshot(mcp_req).await.unwrap();
        assert_eq!(mcp_resp.status(), StatusCode::OK);
        let mcp_body = body_json(mcp_resp).await;

        // Full equality: every byte of the wait response on the HTTP
        // side must equal the `result` value on the MCP side.
        assert_eq!(
            http_body, mcp_body["result"],
            "HTTP and MCP wait_for_work bodies diverged"
        );
    }

    #[tokio::test]
    async fn http_caller_already_voted_regression_at_wire() {
        // P1 regression coverage at the wire. The guard only fires when
        // the gate is `NeedsReview` with role `Reviewers` — i.e. there
        // is at least one participant who hasn't voted on the current
        // target. A single APPROVE makes the gate `Ready`, which flips
        // the role to Master and short-circuits before the guard runs,
        // so we have to build the same scenario as the integration
        // test in `wait::integration_tests`: two participants, plan
        // revised so the target SHA advances, codex re-votes on the
        // new target, bob is missing.
        let dir = init_repo();
        let (runtime, state) = prepared_state(&dir).await;

        // Both codex + bob approve the intro target so they're recorded
        // as participants of the plan-phase gate.
        let intro = runtime
            .read_repo(dir.path(), |s| {
                s.sessions[&crate::lifecycle::SessionId::from("foo")]
                    .plan_intro
                    .clone()
            })
            .await
            .unwrap();
        for author in ["codex", "bob"] {
            let rel = format!(
                ".trinity/feedback/foo/plan/{}/{}.md",
                intro.as_str(),
                author
            );
            write_file(dir.path(), &rel, "APPROVE\n");
            let parsed = crate::disk_format::parse_feedback_path(&std::path::PathBuf::from(
                format!("foo/plan/{}/{}.md", intro.as_str(), author),
            ))
            .unwrap();
            runtime
                .handle_signal(
                    dir.path(),
                    crate::fs_watcher::FilesystemSignal::FeedbackWritten { parsed },
                    1,
                )
                .await
                .unwrap();
        }

        // Push a new plan revision so the target SHA advances; codex
        // re-approves the new target, bob does not.
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo v2\n");
        commit(dir.path(), "revise foo");
        runtime
            .handle_signal(
                dir.path(),
                crate::fs_watcher::FilesystemSignal::HeadChanged,
                2,
            )
            .await
            .unwrap();
        let revised = runtime
            .read_repo(dir.path(), |s| {
                crate::projection::latest_plan_touching_commit(
                    &s.sessions[&crate::lifecycle::SessionId::from("foo")],
                    s,
                )
                .unwrap()
            })
            .await
            .unwrap();
        let codex_rel = format!(".trinity/feedback/foo/plan/{}/codex.md", revised.as_str());
        write_file(dir.path(), &codex_rel, "APPROVE\n");
        let parsed = crate::disk_format::parse_feedback_path(&std::path::PathBuf::from(format!(
            "foo/plan/{}/codex.md",
            revised.as_str()
        )))
        .unwrap();
        runtime
            .handle_signal(
                dir.path(),
                crate::fs_watcher::FilesystemSignal::FeedbackWritten { parsed },
                3,
            )
            .await
            .unwrap();

        // The session is now in reviewers/plan_needs_rereview with bob
        // as the only missing approval. codex's wait_for_work must
        // hit the caller_already_voted guard and time out; bob's must
        // return work.
        let app = router(state);
        let codex_body = json!({
            "role": "reviewers",
            "session_id": "foo",
            "author_label": "codex",
            "repo": dir.path().to_string_lossy(),
            "timeout_secs": 1,
        });
        let codex_req = Request::builder()
            .method("POST")
            .uri("/api/wait_for_work")
            .header("content-type", "application/json")
            .body(Body::from(codex_body.to_string()))
            .unwrap();
        let resp = app.clone().oneshot(codex_req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(
            v["timed_out"], true,
            "codex already voted on the revised target; guard must skip the wake-up"
        );
        assert!(v.get("work").is_none());

        // Sanity check: bob (who hasn't voted on the revised target)
        // does get work. Without this assertion, a regression that made
        // the guard skip everybody would also pass the codex assertion.
        let bob_body = json!({
            "role": "reviewers",
            "session_id": "foo",
            "author_label": "bob",
            "repo": dir.path().to_string_lossy(),
            "timeout_secs": 1,
        });
        let bob_req = Request::builder()
            .method("POST")
            .uri("/api/wait_for_work")
            .header("content-type", "application/json")
            .body(Body::from(bob_body.to_string()))
            .unwrap();
        let resp = app.oneshot(bob_req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["work"], "review_plan");
        let loc = v["locations"][0].as_str().unwrap();
        assert!(
            loc.ends_with("/bob.md"),
            "expected bob's write path, got {loc}"
        );
    }
}
