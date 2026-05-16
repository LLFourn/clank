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

pub fn router(state: AppState) -> Router {
    let static_service = tower_http::services::ServeDir::new(state.frontend_dist.clone());
    let spa_shell = state.spa_shell.clone();
    let frontend_dist = state.frontend_dist.clone();
    Router::new()
        .route("/healthz", get(healthz))
        .route("/events", get(events_route))
        .route("/internal/tools", get(list_tools))
        .route("/internal/tool_call", post(call_tool))
        .route("/api/wait_for_work", post(api_wait_for_work))
        .route("/api/plans", get(api_plans))
        .route("/api/plan/{repo}/{stem_md}", get(api_plan_detail))
        .route("/api/plan/{repo}/{stem_md}/done", post(api_move_to_done))
        .route(
            "/api/plan/{repo}/{stem_md}/revision/{sha}",
            get(api_plan_revision),
        )
        .route(
            "/api/plan/{repo}/{stem_md}/commit/{sha}",
            get(api_commit_diff),
        )
        .route("/api/plan/{repo}/{stem_md}/diff/{from}/{to}", get(api_diff))
        .route("/api/repos", get(api_repos_list))
        .route(
            "/api/repos/{basename}",
            axum::routing::delete(api_repos_delete),
        )
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
            | WaitError::MissingPlanId
            | WaitError::MissingAuthorLabel
            | WaitError::InvalidPlanId(_) => AppError {
                status: StatusCode::BAD_REQUEST,
                msg: e.to_string(),
            },
            WaitError::UnknownRepo(_)
            | WaitError::UnknownPlan(_)
            | WaitError::PlanConflict { .. } => AppError::not_found(e.to_string()),
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

async fn events_route(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<sse::Event, std::convert::Infallible>>> {
    Sse::new(event_stream(state).await).keep_alive(sse::KeepAlive::default())
}

/// Push-driven SSE: forwards new events from the broadcast channel.
///
/// Intentionally does **not** replay the ring on connect. Otherwise every
/// page reload would re-fire chimes + reloads for every historical event,
/// causing a refresh loop. The ring buffer is kept for diagnostic /
/// future-API consumers; SSE only carries live events from the moment of
/// subscribe forward.
async fn event_stream(
    state: AppState,
) -> impl Stream<Item = Result<sse::Event, std::convert::Infallible>> {
    use futures::stream::StreamExt;

    let rx = state.runtime.subscribe_events();
    let live_stream =
        tokio_stream::wrappers::BroadcastStream::new(rx).filter_map(|r| async move { r.ok() });

    let combined = live_stream.map(|e| {
        let plan_id_str = e.plan_id.as_ref().map(|p| p.to_string());
        let slug = e.plan_id.as_ref().map(|p| p.key().as_str().to_string());
        let state_str = e.state.as_ref().map(|s| s.as_str());
        let payload = json!({
            "ts": e.ts,
            "repo": e.repo.to_string_lossy(),
            "plan_id": plan_id_str,
            "slug": slug,
            "state": state_str,
            "kind": e.kind,
            "payload": e.payload,
        });
        Ok(sse::Event::default().data(payload.to_string()))
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

/// Resolve `?repo=<basename-or-absolute-path>` to the list of repo roots
/// the request should render against. Accepts a basename (looked up via
/// `Trinity.repo_basenames`) or an absolute path (canonicalized via
/// `dunce`). Returns all watched repos when `repo` is absent. Returns
/// `AppError::not_found` when the filter doesn't match anything, mirroring
/// the MCP `list_plans` behavior so the two surfaces agree on error
/// semantics.
async fn repos_to_render(
    state: &AppState,
    override_path: Option<String>,
) -> Result<Vec<PathBuf>, AppError> {
    let arc = state.runtime.state();
    let trinity = arc.lock().await;
    let Some(raw) = override_path else {
        return Ok(trinity.repos.keys().cloned().collect());
    };
    let basename = crate::lifecycle::RepoBasename::from(raw.as_str());
    if let Some(root) = trinity.repo_basenames.get(&basename) {
        return Ok(vec![root.clone()]);
    }
    let raw_path = PathBuf::from(&raw);
    let canonical = dunce::canonicalize(&raw_path).unwrap_or(raw_path);
    if trinity.repos.contains_key(&canonical) {
        return Ok(vec![canonical]);
    }
    Err(AppError::not_found(format!("unknown repo filter: {raw}")))
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

/// Resolve `{repo}/{stem_md}` into `(RepoRoot, PlanKey)` via
/// `Trinity.repo_basenames`. Strips `.md` from `stem_md`; returns 404
/// if the basename isn't watched.
async fn resolve_plan_id_segments(
    state: &AppState,
    repo_basename: &str,
    stem_md: &str,
) -> Result<(PathBuf, crate::lifecycle::PlanKey), AppError> {
    let basename = crate::lifecycle::RepoBasename::from(repo_basename);
    let stem = stem_md
        .strip_suffix(".md")
        .ok_or_else(|| AppError::not_found(format!("stem must end in .md: {stem_md}")))?;
    if stem.is_empty() || stem.contains('/') {
        return Err(AppError::not_found(format!("invalid stem: {stem_md}")));
    }
    let plan_key = crate::lifecycle::PlanKey::from(stem.to_string());
    let trinity_arc = state.runtime.state();
    let trinity = trinity_arc.lock().await;
    let repo_root = trinity
        .repo_basenames
        .get(&basename)
        .cloned()
        .ok_or_else(|| AppError::not_found(format!("unknown repo basename: {repo_basename}")))?;
    Ok((repo_root, plan_key))
}

async fn api_plans(
    State(state): State<AppState>,
    Query(q): Query<RepoQuery>,
) -> Result<axum::Json<Value>, AppError> {
    let repos = repos_to_render(&state, q.repo).await?;
    let mut snapshots = Vec::with_capacity(repos.len());
    for repo in repos {
        snapshots.push(
            state
                .runtime
                .snapshot_repo(&repo)
                .await
                .map_err(AppError::runtime)?,
        );
    }
    let v = crate::ui_response::plans_index_across(&snapshots).map_err(AppError::io)?;
    Ok(axum::Json(v))
}

async fn api_plan_detail(
    State(state): State<AppState>,
    Path((repo_basename, stem_md)): Path<(String, String)>,
) -> Result<axum::Json<Value>, AppError> {
    let (repo, plan_key) = resolve_plan_id_segments(&state, &repo_basename, &stem_md).await?;
    let snapshot = state
        .runtime
        .snapshot_session(&repo, &plan_key)
        .await
        .map_err(AppError::runtime)?
        .ok_or_else(|| AppError::not_found(format!("plan {repo_basename}/{stem_md} not found")))?;
    let v = crate::ui_response::plan_page(&snapshot).map_err(AppError::io)?;
    Ok(axum::Json(v))
}

async fn api_plan_revision(
    State(state): State<AppState>,
    Path((repo_basename, stem_md, sha)): Path<(String, String, String)>,
) -> Result<axum::Json<Value>, AppError> {
    let (repo, plan_key) = resolve_plan_id_segments(&state, &repo_basename, &stem_md).await?;
    let commit_sha = crate::lifecycle::CommitSha::from(sha.clone());
    let snapshot = state
        .runtime
        .snapshot_session(&repo, &plan_key)
        .await
        .map_err(AppError::runtime)?
        .ok_or_else(|| AppError::not_found(format!("plan {repo_basename}/{stem_md} not found")))?;

    let plan_revisions = crate::projection::all_plan_revisions_for(
        &snapshot.plan.id,
        &snapshot.commit_order,
        &snapshot.plan_touches,
    );
    let Some(pos) = plan_revisions.iter().position(|c| c == &commit_sha) else {
        return Err(AppError::not_found(format!(
            "commit {sha} is not a plan revision of {repo_basename}/{stem_md}"
        )));
    };

    let path_at_sha = crate::projection::plan_path_at(
        &snapshot.plan.id,
        &commit_sha,
        &snapshot.commit_order,
        &snapshot.plan_touches,
    )
    .ok_or_else(|| AppError::internal("plan path resolution failed for known revision"))?;
    let body_raw = crate::git_io::show_blob(&repo, &commit_sha, &path_at_sha)
        .await
        .map_err(|e| AppError::internal(format!("git show: {e}")))?;
    let body_html = crate::ui_response::render_markdown(&body_raw);

    let previous_sha = pos
        .checked_sub(1)
        .and_then(|j| plan_revisions.get(j))
        .map(|c| c.as_str().to_string());
    let next_sha = plan_revisions.get(pos + 1).map(|c| c.as_str().to_string());

    let feedback = crate::ui_response::feedback_for_target(&snapshot.plan, &commit_sha);
    Ok(axum::Json(json!({
        "repo": snapshot.root.to_string_lossy(),
        "plan_id": format!("{repo_basename}/{stem_md}"),
        "slug": snapshot.plan.id.as_str(),
        "commit_sha": commit_sha.as_str(),
        "body_raw": body_raw,
        "body_html": body_html,
        "plan_intro": snapshot.plan.plan_intro.as_str(),
        "plan_intro_parent": snapshot.plan.plan_intro_parent.as_ref().map(|s| s.as_str()),
        "previous_sha": previous_sha,
        "next_sha": next_sha,
        "feedback": feedback,
    })))
}

async fn api_commit_diff(
    State(state): State<AppState>,
    Path((repo_basename, stem_md, sha)): Path<(String, String, String)>,
) -> Result<axum::Json<Value>, AppError> {
    let (repo, plan_key) = resolve_plan_id_segments(&state, &repo_basename, &stem_md).await?;
    let commit_sha = crate::lifecycle::CommitSha::from(sha.clone());
    let snapshot = state
        .runtime
        .snapshot_session(&repo, &plan_key)
        .await
        .map_err(AppError::runtime)?
        .ok_or_else(|| AppError::not_found(format!("plan {repo_basename}/{stem_md} not found")))?;

    let belongs_to_plan = matches!(
        snapshot.attribution.get(&commit_sha),
        Some(crate::repo_state::AttributionResult::Attributed { session, .. })
            if session == &snapshot.plan.id
    );
    if !belongs_to_plan {
        return Err(AppError::not_found(format!(
            "commit {sha} is not attributed to {repo_basename}/{stem_md}"
        )));
    }

    let patch = crate::git_io::show_commit(&repo, &commit_sha)
        .await
        .map_err(|e| AppError::internal(format!("git show: {e}")))?;
    let diff_files = crate::diff_parser::parse_diff(&patch);
    let diff_files_json = serialize_diff_files(&diff_files);

    let (subject, message_body) = crate::git_io::commit_message(&repo, &commit_sha)
        .await
        .map_err(|e| AppError::internal(format!("git show -s: {e}")))?;

    let feedback = crate::ui_response::feedback_for_target(&snapshot.plan, &commit_sha);
    Ok(axum::Json(json!({
        "repo": snapshot.root.to_string_lossy(),
        "plan_id": format!("{repo_basename}/{stem_md}"),
        "slug": snapshot.plan.id.as_str(),
        "commit_sha": commit_sha.as_str(),
        "subject": subject,
        "message_body": message_body,
        "diff_files": diff_files_json,
        "feedback": feedback,
    })))
}

/// `POST /api/plan/{repo}/{stem_md}/done` — move the plan file under
/// `.trinity/plans/done/`. No body.
async fn api_move_to_done(
    State(state): State<AppState>,
    Path((repo_basename, stem_md)): Path<(String, String)>,
) -> Result<axum::Json<Value>, AppError> {
    let (repo, plan_key) = resolve_plan_id_segments(&state, &repo_basename, &stem_md).await?;
    let plan_path_rel = state
        .runtime
        .snapshot_session(&repo, &plan_key)
        .await
        .map_err(AppError::runtime)?
        .map(|snapshot| snapshot.plan.plan_path)
        .ok_or_else(|| AppError::not_found(format!("plan {repo_basename}/{stem_md} not found")))?;

    let from = repo.join(plan_path_rel.as_path());
    let to_dir = repo.join(".trinity/plans/done");
    std::fs::create_dir_all(&to_dir).map_err(AppError::io)?;
    let to = to_dir.join(
        plan_path_rel
            .as_path()
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

/// `GET /api/plan/{repo}/{stem_md}/diff/{from}/{to}` — patch between
/// two SHAs of this plan's file. Used by `<PlanDiff/>` to compare two
/// plan-body revisions.
async fn api_diff(
    State(state): State<AppState>,
    Path((repo_basename, stem_md, from, to)): Path<(String, String, String, String)>,
) -> Result<axum::Json<Value>, AppError> {
    let (repo, plan_key) = resolve_plan_id_segments(&state, &repo_basename, &stem_md).await?;
    let from_sha = crate::lifecycle::CommitSha::from(from);
    let to_sha = crate::lifecycle::CommitSha::from(to);
    let snapshot = state
        .runtime
        .snapshot_session(&repo, &plan_key)
        .await
        .map_err(AppError::runtime)?
        .ok_or_else(|| AppError::not_found(format!("plan {repo_basename}/{stem_md} not found")))?;

    let plan_revisions = crate::projection::all_plan_revisions_for(
        &snapshot.plan.id,
        &snapshot.commit_order,
        &snapshot.plan_touches,
    );
    if !plan_revisions.contains(&from_sha) {
        return Err(AppError::not_found(format!(
            "commit {from_sha} is not a plan revision of {repo_basename}/{stem_md}"
        )));
    }
    if !plan_revisions.contains(&to_sha) {
        return Err(AppError::not_found(format!(
            "commit {to_sha} is not a plan revision of {repo_basename}/{stem_md}"
        )));
    }

    let from_path = crate::projection::plan_path_at(
        &snapshot.plan.id,
        &from_sha,
        &snapshot.commit_order,
        &snapshot.plan_touches,
    )
    .ok_or_else(|| AppError::not_found(format!("commit {from_sha} not in plan history")))?;
    let to_path = crate::projection::plan_path_at(
        &snapshot.plan.id,
        &to_sha,
        &snapshot.commit_order,
        &snapshot.plan_touches,
    )
    .ok_or_else(|| AppError::not_found(format!("commit {to_sha} not in plan history")))?;
    let patch = crate::git_io::diff_two_blobs(&repo, &from_sha, &from_path, &to_sha, &to_path)
        .await
        .map_err(|e| AppError::internal(format!("git diff: {e}")))?;
    let diff_files = crate::diff_parser::parse_diff(&patch);
    let diff_files_json = serialize_diff_files(&diff_files);
    Ok(axum::Json(json!({
        "repo": repo.to_string_lossy(),
        "plan_id": format!("{repo_basename}/{stem_md}"),
        "from": from_sha.as_str(),
        "to": to_sha.as_str(),
        "from_path": from_path.to_string_lossy(),
        "to_path": to_path.to_string_lossy(),
        "diff_files": diff_files_json,
    })))
}

/// `GET /api/repos` — list every watched repo with its basename,
/// canonical path, plan count, and last activity timestamp. Returns
/// `{repos: [...]}` sorted by `last_activity_ts` desc.
async fn api_repos_list(State(state): State<AppState>) -> Result<axum::Json<Value>, AppError> {
    let trinity_arc = state.runtime.state();
    let trinity = trinity_arc.lock().await;
    let mut repos: Vec<(i64, Value)> = Vec::with_capacity(trinity.repo_basenames.len());
    for (basename, root) in &trinity.repo_basenames {
        let Some(repo_state) = trinity.repos.get(root) else {
            continue;
        };
        let mut last_ts: i64 = 0;
        for plan in repo_state.plans.values() {
            let ts = crate::projection::last_activity_ts_for(
                &plan.id,
                &plan.plan_intro,
                &plan.commits,
                &repo_state.commit_order,
                &repo_state.plan_touches,
                &repo_state.attribution,
                &repo_state.commit_meta,
            );
            last_ts = last_ts.max(ts);
        }
        repos.push((
            last_ts,
            json!({
                "basename": basename.as_str(),
                "root": root.to_string_lossy(),
                "plan_count": repo_state.plans.len(),
                "last_activity_ts": last_ts,
            }),
        ));
    }
    repos.sort_by_key(|r| std::cmp::Reverse(r.0));
    let repos: Vec<Value> = repos.into_iter().map(|(_, v)| v).collect();
    Ok(axum::Json(json!({ "repos": repos })))
}

/// `DELETE /api/repos/{basename}` — unwatch a repo:
///
/// 1. Resolve `basename` to a canonical root via `Trinity.repo_basenames`.
/// 2. Abort and drop the watcher handle for that root from
///    `AppState.watchers`.
/// 3. Call `Runtime::remove_repo` to drop the entries from
///    `Trinity.repos` + `Trinity.repo_basenames` and emit a
///    `repo_unwatched` LiveEvent.
/// 4. Rewrite `AppState.repos_path` without this line.
///
/// Returns 404 when the basename isn't watched.
async fn api_repos_delete(
    State(state): State<AppState>,
    Path(basename): Path<String>,
) -> Result<axum::Json<Value>, AppError> {
    let basename_key = crate::lifecycle::RepoBasename::from(basename.as_str());
    let canonical = {
        let trinity_arc = state.runtime.state();
        let trinity = trinity_arc.lock().await;
        trinity.repo_basenames.get(&basename_key).cloned()
    };
    let Some(canonical) = canonical else {
        return Err(AppError::not_found(format!(
            "unknown repo basename: {basename}"
        )));
    };

    if let Some(handle) = state.watchers.lock().await.remove(&canonical) {
        handle.abort();
    }

    let outcome = state.runtime.remove_repo(canonical.clone()).await;

    let registry_write_error = match mcp::remove_repo_from_registry(&state.repos_path, &canonical) {
        Ok(()) => None,
        Err(err) => {
            tracing::warn!(
                repo = %canonical.display(),
                registry = %state.repos_path.display(),
                error = ?err,
                "failed to remove repo from registry file"
            );
            Some(format!(
                "in-memory state removed, but registry file at {} could not be rewritten: {err}. \
                 The repo will reappear on daemon restart until the file is fixed.",
                state.repos_path.display()
            ))
        }
    };

    let plan_count = match outcome {
        crate::runtime::RemoveOutcome::Removed { plan_count } => plan_count,
        // We held the read of `repo_basenames` over a brief lock-drop
        // before the remove call; concurrent removal is rare but harmless
        // here. Treat as success with zero plans (the runtime already
        // emitted no event in that branch, which is the right shape).
        crate::runtime::RemoveOutcome::NotPresent => 0,
    };

    Ok(axum::Json(json!({
        "ok": registry_write_error.is_none(),
        "basename": basename,
        "removed_plan_count": plan_count,
        "registry_write_error": registry_write_error,
    })))
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
            watchers: Arc::new(Mutex::new(std::collections::BTreeMap::new())),
            frontend_dist: std::path::PathBuf::from("frontend/dist"),
            spa_shell: None,
            repos_path: std::path::PathBuf::from("/dev/null"),
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

    /// `<basename>/<slug>.md` derived from the tempdir.
    fn plan_id_for(dir: &tempfile::TempDir, slug: &str) -> String {
        let basename = dir.path().file_name().unwrap().to_str().unwrap();
        format!("{basename}/{slug}.md")
    }

    #[tokio::test]
    async fn http_returns_work_for_immediate_match() {
        let dir = init_repo();
        let app = router_with_repo(&dir).await;
        let body = json!({
            "role": "reviewers",
            "plan_id": plan_id_for(&dir, "foo"),
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
            "plan_id": plan_id_for(&dir, "foo"),
            "author_label": "lloyd",
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
            "plan_id": plan_id_for(&dir, "foo"),
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
        let msg = body_text(resp).await;
        assert!(msg.contains("invalid role"), "got: {msg}");
    }

    #[tokio::test]
    async fn http_404_on_unknown_repo() {
        let dir = init_repo();
        let app = router_with_repo(&dir).await;
        // Repo basename `no-such-repo` isn't watched — daemon returns
        // 404 unknown_repo. (Replaces the previous missing-repo test;
        // `repo` is no longer a separate field, it lives inside `plan_id`.)
        let body = json!({
            "role": "reviewers",
            "plan_id": "no-such-repo/foo.md",
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
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn http_400_on_missing_author_label() {
        let dir = init_repo();
        let app = router_with_repo(&dir).await;
        let body = json!({
            "role": "reviewers",
            "plan_id": plan_id_for(&dir, "foo"),
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
            "plan_id": plan_id_for(&dir, "does-not-exist"),
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
                "plan_id": plan_id_for(&dir, "foo"),
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
                "plan_id": plan_id_for(&dir, "foo"),
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
                "plan_id": plan_id_for(&dir, "foo"),
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
                "plan_id": plan_id_for(&dir, "foo"),
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
                "plan_id": plan_id_for(&dir, "foo"),
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
            "plan_id": plan_id_for(&dir, "foo"),
            "author_label": "codex",
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
                s.plans[&crate::lifecycle::PlanKey::from("foo")]
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
                    &s.plans[&crate::lifecycle::PlanKey::from("foo")],
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
            "plan_id": plan_id_for(&dir, "foo"),
            "author_label": "codex",
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
            "plan_id": plan_id_for(&dir, "foo"),
            "author_label": "bob",
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
