//! HTTP routes backed by the filesystem-truth runtime.

use std::path::PathBuf;

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::http::header;
use axum::response::{Html, IntoResponse, Response, Sse, sse};
use axum::routing::{get, post};
use futures::stream::Stream;
use serde::Deserialize;

use super::AppState;
use super::mcp;
use super::state::Bundle;
use super::wait::{WaitArgs, WaitError, wait_for_work};

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/events", get(events_route))
        .route("/internal/tools", get(list_tools))
        .route("/internal/tool_call", post(call_tool))
        .route("/api/wait_for_work", post(api_wait_for_work))
        .route("/api/plans", get(api_plans))
        .route("/api/plan/{repo}/{stem_md}", get(api_plan_detail))
        .route(
            "/api/plan/{repo}/{stem_md}/finish_preview",
            get(api_finish_preview),
        )
        .route(
            "/api/plan/{repo}/{stem_md}/rewrite_preview",
            get(api_rewrite_preview),
        )
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
        .route(
            "/api/repos/{basename}/rewrite_preview_all",
            get(api_rewrite_preview_all),
        )
        .route("/static/{*path}", get(serve_static_asset))
        .fallback(get(serve_spa_shell))
        .with_state(state)
}

/// Resolve `/static/<path>` against the daemon's `Bundle`. Embedded
/// mode reads from the binary's `include_dir!` table; Disk mode
/// re-reads from the filesystem on every request so `trunk watch`
/// reload loops work without restarting the daemon.
async fn serve_static_asset(State(state): State<AppState>, Path(path): Path<String>) -> Response {
    match &state.bundle {
        Bundle::Embedded(dir) => match dir.get_file(&path) {
            Some(file) => {
                let mime = mime_for(&path);
                ([(header::CONTENT_TYPE, mime)], file.contents()).into_response()
            }
            None => StatusCode::NOT_FOUND.into_response(),
        },
        Bundle::Disk(dir) => {
            let full = dir.join(&path);
            match tokio::fs::read(&full).await {
                Ok(bytes) => {
                    let mime = mime_for(&path);
                    ([(header::CONTENT_TYPE, mime)], bytes).into_response()
                }
                Err(_) => StatusCode::NOT_FOUND.into_response(),
            }
        }
    }
}

/// SPA fallback: serve `index.html` for any unmatched route so the
/// Leptos router handles in-app navigation client-side.
async fn serve_spa_shell(State(state): State<AppState>) -> Response {
    let bytes = match &state.bundle {
        Bundle::Embedded(dir) => dir.get_file("index.html").map(|f| f.contents().to_vec()),
        Bundle::Disk(dir) => tokio::fs::read(dir.join("index.html")).await.ok(),
    };
    match bytes {
        Some(body) => Html(body).into_response(),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Leptos bundle missing index.html. The embedded bundle is built by \
             `build.rs`; if you set --frontend-dist, run `trunk build` in frontend/.",
        )
            .into_response(),
    }
}

/// MIME type for a few extensions the SPA actually serves. `axum`'s
/// default static-file pipeline (via `tower-http`) is heavier-weight
/// than we need; this table covers the trunk output set.
fn mime_for(path: &str) -> &'static str {
    if path.ends_with(".wasm") {
        "application/wasm"
    } else if path.ends_with(".js") {
        "application/javascript; charset=utf-8"
    } else if path.ends_with(".css") {
        "text/css; charset=utf-8"
    } else if path.ends_with(".html") {
        "text/html; charset=utf-8"
    } else if path.ends_with(".json") {
        "application/json"
    } else if path.ends_with(".svg") {
        "image/svg+xml"
    } else if path.ends_with(".png") {
        "image/png"
    } else if path.ends_with(".ico") {
        "image/x-icon"
    } else if path.ends_with(".woff2") {
        "font/woff2"
    } else {
        "application/octet-stream"
    }
}

async fn api_wait_for_work(
    State(state): State<AppState>,
    axum::Json(args): axum::Json<WaitArgs>,
) -> Result<axum::Json<trinity_core::api::WaitForWorkResponse>, AppError> {
    let resp = wait_for_work(&state.runtime, args)
        .await
        .map_err(|e| match e {
            WaitError::InvalidRole(_)
            | WaitError::MissingPlanId
            | WaitError::MissingAuthorLabel
            | WaitError::InvalidAuthorLabel(_)
            | WaitError::InvalidPlanId(_) => AppError {
                status: StatusCode::BAD_REQUEST,
                msg: e.to_string(),
            },
            WaitError::UnknownRepo(_)
            | WaitError::UnknownPlan(_)
            | WaitError::PlanConflict { .. } => AppError::not_found(e.to_string()),
            WaitError::Io(err) => AppError::io(err),
        })?;
    Ok(axum::Json(resp))
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
        use crate::repo_state::LiveEvent as DaemonLiveEvent;
        let wire_event = match e {
            DaemonLiveEvent::Repo(re) => {
                trinity_core::api::LiveEvent::Repo(trinity_core::api::RepoEvent {
                    ts: re.ts,
                    payload: re.payload,
                })
            }
            DaemonLiveEvent::Plan(pe) => {
                trinity_core::api::LiveEvent::Plan(trinity_core::api::PlanEvent {
                    ts: pe.ts,
                    plan_id: pe.plan_id.to_string(),
                    lifecycle: pe.lifecycle,
                    payload: pe.payload,
                })
            }
        };
        let payload = serde_json::to_string(&wire_event)
            .unwrap_or_else(|_| String::from("{\"error\":\"sse_serialize\"}"));
        Ok(sse::Event::default().data(payload))
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
    if let Ok(basename) = crate::lifecycle::RepoBasename::parse(&raw)
        && let Some(root) = trinity.repo_basenames.get(&basename)
    {
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
    fn bad_request(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
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
    let basename = crate::lifecycle::RepoBasename::parse(repo_basename)
        .map_err(|e| AppError::bad_request(format!("invalid repo basename: {e}")))?;
    let stem = stem_md
        .strip_suffix(".md")
        .ok_or_else(|| AppError::bad_request(format!("stem must end in .md: {stem_md}")))?;
    let plan_key = crate::lifecycle::PlanKey::parse(stem)
        .map_err(|e| AppError::bad_request(format!("invalid plan stem: {e}")))?;
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
) -> Result<axum::Json<trinity_core::api::ListPlansResponse>, AppError> {
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
    let v = crate::responses::plans_index_across(&snapshots).map_err(AppError::io)?;
    Ok(axum::Json(v))
}

/// 404 the request if the plan in this single-plan snapshot is
/// hidden (active + plan file missing from the working tree). See
/// `Plan::is_visible`. Every plan-scoped route calls this before
/// doing further work so the response shape is uniform.
fn enforce_plan_visible(
    snapshot: &crate::repo_state::RepoState,
    repo_basename: &str,
    stem_md: &str,
) -> Result<(), AppError> {
    let plan = snapshot
        .plans
        .values()
        .next()
        .expect("single_plan invariant");
    let status = crate::responses::compute_plan_worktree_status_parts(
        &snapshot.root,
        &plan.plan_path,
        &plan.body_hash,
    )
    .map_err(AppError::io)?;
    if !plan.is_visible(status) {
        return Err(AppError::not_found(format!(
            "plan {repo_basename}/{stem_md} is hidden: plan file missing from working tree"
        )));
    }
    Ok(())
}

async fn api_plan_detail(
    State(state): State<AppState>,
    Path((repo_basename, stem_md)): Path<(String, String)>,
) -> Result<axum::Json<trinity_core::api::PlanDetailResponse>, AppError> {
    let (repo, plan_key) = resolve_plan_id_segments(&state, &repo_basename, &stem_md).await?;
    let snapshot = state
        .runtime
        .snapshot_session(&repo, &plan_key)
        .await
        .map_err(AppError::runtime)?
        .ok_or_else(|| AppError::not_found(format!("plan {repo_basename}/{stem_md} not found")))?;
    enforce_plan_visible(&snapshot, &repo_basename, &stem_md)?;
    let v = crate::responses::plan_page(&snapshot)
        .map_err(AppError::io)?
        .expect("plan_page returns Some after enforce_plan_visible");
    Ok(axum::Json(v))
}

async fn api_finish_preview(
    State(state): State<AppState>,
    Path((repo_basename, stem_md)): Path<(String, String)>,
) -> Result<axum::Json<trinity_core::api::FinishPreviewResponse>, AppError> {
    let (repo, plan_key) = resolve_plan_id_segments(&state, &repo_basename, &stem_md).await?;
    let snapshot = state
        .runtime
        .snapshot_session(&repo, &plan_key)
        .await
        .map_err(AppError::runtime)?
        .ok_or_else(|| AppError::not_found(format!("plan {repo_basename}/{stem_md} not found")))?;
    enforce_plan_visible(&snapshot, &repo_basename, &stem_md)?;

    let plan = snapshot
        .plans
        .values()
        .next()
        .expect("single_plan invariant");
    let plan_id = format!("{repo_basename}/{stem_md}");
    let worktree_status = crate::responses::compute_plan_worktree_status_parts(
        &snapshot.root,
        &plan.plan_path,
        &plan.body_hash,
    )
    .map_err(AppError::io)?;
    let latest_reviewable_sha = crate::projection::latest_reviewable_commit_for(plan);
    let latest_event = latest_reviewable_sha
        .as_ref()
        .and_then(|sha| plan.event_for(sha));
    let gate = latest_event.and_then(|e| e.gate());
    let gate_state = gate
        .map(|g| g.state)
        .unwrap_or(trinity_core::vocab::CommitGateState::Unreviewed);
    let is_finished = plan.is_frozen();

    let sealed_approvals = match gate {
        Some(g) if g.state == trinity_core::vocab::CommitGateState::Approved => {
            let target_sha = latest_reviewable_sha
                .as_ref()
                .expect("Approved gate implies a reviewable sha");
            g.feedback
                .iter()
                .filter(|(_, fb)| fb.verdict == trinity_core::Verdict::Approve)
                .map(|(author, fb)| trinity_core::api::SealedApproval {
                    author: author.clone(),
                    source_path: crate::disk_format::feedback_path_wire(
                        &plan.id,
                        target_sha.as_str(),
                        author,
                    ),
                    body_hash: crate::lifecycle::content_hash(&fb.body),
                })
                .collect()
        }
        _ => Vec::new(),
    };

    let readiness = compute_finalize_readiness(
        is_finished,
        latest_reviewable_sha.as_ref(),
        gate_state,
        worktree_status,
    );

    Ok(axum::Json(trinity_core::api::FinishPreviewResponse {
        plan_id,
        plan_path: plan.plan_path.clone(),
        readiness,
        gate_state,
        latest_reviewable_sha,
        plan_worktree_status: worktree_status,
        is_finished,
        sealed_approvals,
    }))
}

#[derive(Deserialize)]
struct RewritePreviewQuery {
    #[serde(default)]
    include_finalize: bool,
}

/// All-plans variant: defaults `include_finalize` to `true`
/// because scrubbing every `.trinity/` path is the natural meaning
/// of `--all` — finalize snapshots are part of "every Trinity
/// trace." Distinct query type from `RewritePreviewQuery` so the
/// `#[serde(default)]` on a bool doesn't silently mean false here.
#[derive(Deserialize)]
struct RewritePreviewAllQuery {
    #[serde(default = "default_include_finalize_all")]
    include_finalize: bool,
}

fn default_include_finalize_all() -> bool {
    true
}

async fn api_rewrite_preview(
    State(state): State<AppState>,
    Path((repo_basename, stem_md)): Path<(String, String)>,
    Query(q): Query<RewritePreviewQuery>,
) -> Result<axum::Json<trinity_core::api::RewritePreviewResponse>, AppError> {
    let (repo, plan_key) = resolve_plan_id_segments(&state, &repo_basename, &stem_md).await?;
    let snapshot = state
        .runtime
        .snapshot_session(&repo, &plan_key)
        .await
        .map_err(AppError::runtime)?
        .ok_or_else(|| AppError::not_found(format!("plan {repo_basename}/{stem_md} not found")))?;
    enforce_plan_visible(&snapshot, &repo_basename, &stem_md)?;
    let plan = snapshot
        .plans
        .values()
        .next()
        .expect("single_plan invariant");
    let plan_id = format!("{repo_basename}/{stem_md}");
    let plan_stem = plan_key.clone();

    let head_sha = snapshot
        .head
        .clone()
        .ok_or_else(|| AppError::not_found(format!("repo {repo_basename} has no HEAD")))?;
    let intro_sha = Some(plan.plan_intro.clone());

    let metas = crate::git_io::first_parent_commits_to(&repo, &head_sha)
        .await
        .map_err(|e| AppError::internal(format!("git first-parent walk: {e}")))?;
    let start = metas
        .iter()
        .position(|m| m.sha == plan.plan_intro)
        .ok_or_else(|| {
            AppError::internal(format!(
                "plan intro {} not in first-parent walk from HEAD",
                plan.plan_intro.as_str()
            ))
        })?;
    let range = &metas[start..];

    // Map each in-range sha to its timeline-event kind for this
    // plan, when present. `MultiPlan` events ARE in this plan's
    // timeline (they touched it) but a cross-plan commit must
    // still be flagged `foreign: true` — `--squash` refuses on it,
    // `--purge` rewrites it. Treat PlanOnly/CodeOnly/Mixed/Finalize
    // as "native to this plan"; everything else (MultiPlan, absent)
    // is foreign.
    use trinity_core::model::PlanTimelineEvent;
    let native_shas: std::collections::BTreeSet<crate::lifecycle::CommitSha> = plan
        .timeline
        .iter()
        .filter_map(|e| match e {
            PlanTimelineEvent::PlanOnly { sha, .. }
            | PlanTimelineEvent::CodeOnly { sha, .. }
            | PlanTimelineEvent::Mixed { sha, .. }
            | PlanTimelineEvent::Finalize { sha, .. } => Some(sha.clone()),
            PlanTimelineEvent::MultiPlan { .. } => None,
        })
        .collect();

    let mut linear = true;
    let mut commits = Vec::with_capacity(range.len());
    for meta in range {
        let parent_count = crate::git_io::commit_parent_count(&repo, &meta.sha)
            .await
            .map_err(|e| AppError::internal(format!("git show parents: {e}")))?;
        if parent_count > 1 {
            linear = false;
        }
        let changes = crate::git_io::diff_tree_changes(&repo, &meta.sha)
            .await
            .map_err(|e| AppError::internal(format!("git diff-tree: {e}")))?;
        let attributed_to_this_plan = native_shas.contains(&meta.sha);
        let (disposition, strip_paths) = classify_rewrite(&changes, &plan_key, q.include_finalize);
        commits.push(trinity_core::api::RewriteCommit {
            sha: meta.sha.clone(),
            subject: meta.subject.clone(),
            disposition,
            foreign: !attributed_to_this_plan,
            strip_paths,
        });
    }

    Ok(axum::Json(trinity_core::api::RewritePreviewResponse {
        plan_id,
        plan_stem,
        intro_sha,
        head_sha,
        linear,
        commits,
    }))
}

async fn api_rewrite_preview_all(
    State(state): State<AppState>,
    Path(repo_basename): Path<String>,
    Query(q): Query<RewritePreviewAllQuery>,
) -> Result<axum::Json<trinity_core::api::PurgeAllPreviewResponse>, AppError> {
    let basename = crate::lifecycle::RepoBasename::parse(&repo_basename)
        .map_err(|e| AppError::bad_request(format!("invalid repo basename: {e}")))?;
    let repo_root = {
        let trinity_arc = state.runtime.state();
        let trinity = trinity_arc.lock().await;
        trinity
            .repo_basenames
            .get(&basename)
            .cloned()
            .ok_or_else(|| AppError::not_found(format!("unknown repo basename: {repo_basename}")))?
    };

    let head_sha = crate::git_io::rev_parse_head(&repo_root)
        .await
        .map_err(|e| AppError::internal(format!("git rev-parse HEAD: {e}")))?
        .ok_or_else(|| AppError::not_found(format!("repo {repo_basename} has no HEAD")))?;

    let metas = crate::git_io::first_parent_commits_to(&repo_root, &head_sha)
        .await
        .map_err(|e| AppError::internal(format!("git first-parent walk: {e}")))?;

    let mut linear = true;
    let mut intro_sha: Option<crate::lifecycle::CommitSha> = None;
    let mut commits = Vec::with_capacity(metas.len());
    let mut plans_seen: std::collections::BTreeSet<crate::lifecycle::PlanKey> =
        std::collections::BTreeSet::new();
    for meta in metas {
        let parent_count = crate::git_io::commit_parent_count(&repo_root, &meta.sha)
            .await
            .map_err(|e| AppError::internal(format!("git show parents: {e}")))?;
        if parent_count > 1 {
            linear = false;
        }
        let changes = crate::git_io::diff_tree_changes(&repo_root, &meta.sha)
            .await
            .map_err(|e| AppError::internal(format!("git diff-tree: {e}")))?;
        for touch in &changes.plan_touches {
            plans_seen.insert(touch.session.clone());
        }
        for fc in &changes.finalize_changes {
            plans_seen.insert(fc.plan_key.clone());
        }
        let (disposition, strip_paths) = classify_rewrite_all(&changes, q.include_finalize);
        let touched_any_trinity = !changes.trinity_paths.is_empty();
        if intro_sha.is_none() && touched_any_trinity {
            intro_sha = Some(meta.sha.clone());
        }
        commits.push(trinity_core::api::RewriteCommit {
            sha: meta.sha.clone(),
            subject: meta.subject.clone(),
            disposition,
            // No per-plan attribution for the all-plans case.
            foreign: false,
            strip_paths,
        });
    }

    Ok(axum::Json(trinity_core::api::PurgeAllPreviewResponse {
        repo: repo_basename.clone(),
        head_sha,
        linear,
        intro_sha,
        plans_touched: plans_seen.into_iter().collect(),
        commits,
    }))
}

/// All-plans variant of `classify_rewrite`. Uses
/// `CommitChanges.trinity_paths` as the source of truth for
/// "this plan's strippable paths" — captures plan files,
/// finalize files, AND non-plan Trinity metadata like
/// `.trinity/.gitignore` and `.trinity/stubs/*`.
fn classify_rewrite_all(
    changes: &crate::attribution::CommitChanges,
    include_finalize: bool,
) -> (trinity_core::api::RewriteDisposition, Vec<String>) {
    use trinity_core::api::RewriteDisposition;

    // Apply the include_finalize filter at the strip-path level:
    // when finalize is excluded, drop entries under
    // `.trinity/finished/` from the strip set. The disposition
    // still uses the full set — a commit that only touched
    // finalize files is still touching `.trinity/`, just
    // strippable depends on the flag.
    let trinity_paths_filtered: Vec<String> = changes
        .trinity_paths
        .iter()
        .filter(|p| include_finalize || !p.starts_with(".trinity/finished/"))
        .cloned()
        .collect();

    let touched_any_trinity = !changes.trinity_paths.is_empty();
    let touched_other_paths = changes.has_non_plan_code_changes;
    // Edge case: a commit that ONLY touched `.trinity/finished/`
    // when `include_finalize=false` has no strippable paths but
    // still touched `.trinity/`. Treat as KeepVerbatim — there's
    // nothing to strip, and dropping the commit would mean losing
    // the finalize snapshot.
    let has_strippable = !trinity_paths_filtered.is_empty();

    let disposition = if !touched_any_trinity || !has_strippable {
        RewriteDisposition::KeepVerbatim
    } else if touched_other_paths {
        RewriteDisposition::Rewrite
    } else {
        RewriteDisposition::Drop
    };

    let strip_paths = if matches!(disposition, RewriteDisposition::Rewrite) {
        trinity_paths_filtered
    } else {
        Vec::new()
    };

    (disposition, strip_paths)
}

/// Classify a single commit's disposition for the rewrite engine,
/// derived purely from its `CommitChanges` and whether finalize-tree
/// entries are in the strip set. The fold's per-commit `CommitChanges`
/// is the data layer; classification is the typed projection.
fn classify_rewrite(
    changes: &crate::attribution::CommitChanges,
    plan_key: &crate::lifecycle::PlanKey,
    include_finalize: bool,
) -> (trinity_core::api::RewriteDisposition, Vec<String>) {
    use trinity_core::api::RewriteDisposition;

    let this_plan_touches: Vec<&crate::attribution::PlanTouch> = changes
        .plan_touches
        .iter()
        .filter(|t| &t.session == plan_key)
        .collect();
    let other_plan_touches = changes.plan_touches.iter().any(|t| &t.session != plan_key);

    let this_finalize: Vec<&crate::attribution::FinalizeChange> = changes
        .finalize_changes
        .iter()
        .filter(|f| &f.plan_key == plan_key)
        .collect();
    let other_finalize = changes
        .finalize_changes
        .iter()
        .any(|f| &f.plan_key != plan_key);

    let touched_this_plan_strippable =
        !this_plan_touches.is_empty() || (include_finalize && !this_finalize.is_empty());
    let touched_other_paths = changes.has_non_plan_code_changes
        || other_plan_touches
        || other_finalize
        || (!include_finalize && !this_finalize.is_empty());

    let mut strip_paths = Vec::new();
    if touched_this_plan_strippable {
        for touch in &this_plan_touches {
            if let Some(p) = &touch.new_path {
                strip_paths.push(p.to_string_lossy().into_owned());
            }
        }
        if include_finalize {
            for fc in &this_finalize {
                if matches!(
                    fc.kind,
                    crate::attribution::FinalizeChangeKind::Upsert { .. }
                ) {
                    strip_paths.push(format!(
                        ".trinity/finished/{}/{}",
                        plan_key.as_str(),
                        fc.file_name
                    ));
                }
            }
        }
        strip_paths.sort();
        strip_paths.dedup();
    }

    let disposition = if !touched_this_plan_strippable {
        RewriteDisposition::KeepVerbatim
    } else if touched_other_paths {
        RewriteDisposition::Rewrite
    } else {
        RewriteDisposition::Drop
    };

    // strip_paths is moot for Drop (the commit goes away) and
    // KeepVerbatim (we don't change the tree). Only Rewrite uses it.
    if !matches!(disposition, RewriteDisposition::Rewrite) {
        strip_paths.clear();
    }

    (disposition, strip_paths)
}

/// Project the typed finalize decision. Single source of truth for
/// "can the CLI proceed?" — the CLI dispatches on this and nothing
/// else.
fn compute_finalize_readiness(
    is_finished: bool,
    latest_reviewable_sha: Option<&crate::lifecycle::CommitSha>,
    gate_state: trinity_core::vocab::CommitGateState,
    worktree_status: trinity_core::vocab::PlanWorktreeStatus,
) -> trinity_core::api::FinalizeReadiness {
    use trinity_core::api::{FinalizeBlockReason, FinalizeReadiness};
    use trinity_core::vocab::{CommitGateState, PlanWorktreeStatus};

    if is_finished {
        return FinalizeReadiness::AlreadyFinished;
    }
    let mut reasons = Vec::new();
    if latest_reviewable_sha.is_none() {
        reasons.push(FinalizeBlockReason::NoReviewableCommit);
    }
    if gate_state != CommitGateState::Approved {
        reasons.push(FinalizeBlockReason::GateNotApproved { state: gate_state });
    }
    match worktree_status {
        PlanWorktreeStatus::PlanFileMissing => {
            reasons.push(FinalizeBlockReason::PlanFileMissing);
        }
        PlanWorktreeStatus::BodyDirty => {
            reasons.push(FinalizeBlockReason::PlanFileDirty);
        }
        PlanWorktreeStatus::Clean => {}
    }
    if reasons.is_empty() {
        FinalizeReadiness::Ready
    } else {
        FinalizeReadiness::Blocked { reasons }
    }
}

async fn api_plan_revision(
    State(state): State<AppState>,
    Path((repo_basename, stem_md, sha)): Path<(String, String, String)>,
) -> Result<axum::Json<trinity_core::api::PlanRevisionResponse>, AppError> {
    let (repo, plan_key) = resolve_plan_id_segments(&state, &repo_basename, &stem_md).await?;
    let commit_sha = crate::lifecycle::CommitSha::parse(&sha)
        .map_err(|e| AppError::bad_request(format!("invalid sha: {e}")))?;
    let snapshot = state
        .runtime
        .snapshot_session(&repo, &plan_key)
        .await
        .map_err(AppError::runtime)?
        .ok_or_else(|| AppError::not_found(format!("plan {repo_basename}/{stem_md} not found")))?;
    enforce_plan_visible(&snapshot, &repo_basename, &stem_md)?;

    let plan = snapshot
        .plans
        .values()
        .next()
        .expect("single_plan invariant");
    let plan_revisions = crate::projection::all_plan_revisions(plan, &snapshot);
    let Some(pos) = plan_revisions.iter().position(|c| c == &commit_sha) else {
        return Err(AppError::not_found(format!(
            "commit {sha} is not a plan revision of {repo_basename}/{stem_md}"
        )));
    };
    let path_at_sha = crate::projection::plan_path_at(plan, &commit_sha)
        .ok_or_else(|| AppError::internal("plan path resolution failed for known revision"))?;
    let body_raw = crate::git_io::show_blob(&repo, &commit_sha, &path_at_sha)
        .await
        .map_err(|e| AppError::internal(format!("git show: {e}")))?;
    Ok(axum::Json(crate::responses::build_plan_revision_response(
        &snapshot,
        plan,
        format!("{repo_basename}/{stem_md}"),
        &commit_sha,
        body_raw,
        &plan_revisions,
        pos,
    )))
}

async fn api_commit_diff(
    State(state): State<AppState>,
    Path((repo_basename, stem_md, sha)): Path<(String, String, String)>,
) -> Result<axum::Json<trinity_core::api::CommitDetailResponse>, AppError> {
    use trinity_core::vocab::CommitKind;

    let (repo, plan_key) = resolve_plan_id_segments(&state, &repo_basename, &stem_md).await?;
    let commit_sha = crate::lifecycle::CommitSha::parse(&sha)
        .map_err(|e| AppError::bad_request(format!("invalid sha: {e}")))?;
    let snapshot = state
        .runtime
        .snapshot_session(&repo, &plan_key)
        .await
        .map_err(AppError::runtime)?
        .ok_or_else(|| AppError::not_found(format!("plan {repo_basename}/{stem_md} not found")))?;
    enforce_plan_visible(&snapshot, &repo_basename, &stem_md)?;

    let plan = snapshot
        .plans
        .values()
        .next()
        .expect("single_plan invariant");
    let event = plan.event_for(&commit_sha).ok_or_else(|| {
        AppError::not_found(format!(
            "commit {sha} is not attributed to {repo_basename}/{stem_md}"
        ))
    })?;
    if matches!(event.kind(), CommitKind::Unattributed) {
        return Err(AppError::not_found(format!(
            "commit {sha} is unattributed for {repo_basename}/{stem_md}"
        )));
    }
    let stem = plan.id.as_str().to_string();

    let patch = crate::git_io::show_commit(&repo, &commit_sha)
        .await
        .map_err(|e| AppError::internal(format!("git show: {e}")))?;
    let diff_files = crate::diff_parser::parse_diff(&patch);

    let (subject, message_body) = crate::git_io::commit_message(&repo, &commit_sha)
        .await
        .map_err(|e| AppError::internal(format!("git show -s: {e}")))?;

    let finalize_files = if matches!(event.kind(), CommitKind::Finalize) {
        crate::git_io::read_finalize_snapshot(&repo, &commit_sha, &stem)
            .await
            .map_err(|e| AppError::internal(format!("read finalize snapshot: {e}")))?
    } else {
        Vec::new()
    };

    Ok(axum::Json(crate::responses::build_commit_detail_response(
        crate::responses::CommitDetailInputs {
            snapshot: &snapshot,
            plan,
            plan_id_str: format!("{repo_basename}/{stem_md}"),
            commit_sha: &commit_sha,
            event,
            subject,
            message_body,
            diff_files,
            finalize_files,
        },
    )))
}

/// `GET /api/plan/{repo}/{stem_md}/diff/{from}/{to}` — patch between
/// two SHAs of this plan's file. Used by `<PlanDiff/>` to compare two
/// plan-body revisions.
async fn api_diff(
    State(state): State<AppState>,
    Path((repo_basename, stem_md, from, to)): Path<(String, String, String, String)>,
) -> Result<axum::Json<trinity_core::api::DiffResponse>, AppError> {
    let (repo, plan_key) = resolve_plan_id_segments(&state, &repo_basename, &stem_md).await?;
    let from_sha = crate::lifecycle::CommitSha::parse(&from)
        .map_err(|e| AppError::bad_request(format!("invalid from sha: {e}")))?;
    let to_sha = crate::lifecycle::CommitSha::parse(&to)
        .map_err(|e| AppError::bad_request(format!("invalid to sha: {e}")))?;
    let snapshot = state
        .runtime
        .snapshot_session(&repo, &plan_key)
        .await
        .map_err(AppError::runtime)?
        .ok_or_else(|| AppError::not_found(format!("plan {repo_basename}/{stem_md} not found")))?;
    enforce_plan_visible(&snapshot, &repo_basename, &stem_md)?;

    let plan = snapshot
        .plans
        .values()
        .next()
        .expect("single_plan invariant");
    let plan_revisions = crate::projection::all_plan_revisions(plan, &snapshot);
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
    let from_path = crate::projection::plan_path_at(plan, &from_sha)
        .ok_or_else(|| AppError::not_found(format!("commit {from_sha} not in plan history")))?;
    let to_path = crate::projection::plan_path_at(plan, &to_sha)
        .ok_or_else(|| AppError::not_found(format!("commit {to_sha} not in plan history")))?;
    let patch = crate::git_io::diff_two_blobs(&repo, &from_sha, &from_path, &to_sha, &to_path)
        .await
        .map_err(|e| AppError::internal(format!("git diff: {e}")))?;
    let diff_files = crate::diff_parser::parse_diff(&patch);
    Ok(axum::Json(crate::responses::build_diff_response(
        &from_sha, &to_sha, &from_path, &to_path, diff_files,
    )))
}

/// `GET /api/repos` — list every watched repo with its basename,
/// canonical path, plan count, and last activity timestamp. Returns
/// `{repos: [...]}` sorted by `last_activity_ts` desc.
async fn api_repos_list(
    State(state): State<AppState>,
) -> Result<axum::Json<trinity_core::api::RepoListResponse>, AppError> {
    let trinity_arc = state.runtime.state();
    let trinity = trinity_arc.lock().await;
    Ok(axum::Json(crate::responses::build_repo_list_response(
        &trinity,
    )))
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
) -> Result<axum::Json<trinity_core::api::DeleteRepoOutcome>, AppError> {
    let basename_key = crate::lifecycle::RepoBasename::parse(&basename)
        .map_err(|e| AppError::bad_request(format!("invalid basename: {e}")))?;
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

    Ok(axum::Json(trinity_core::api::DeleteRepoOutcome {
        ok: registry_write_error.is_none(),
        basename,
        plan_count,
        registry_write_error,
    }))
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
            // Tests never hit the SPA-serving routes; point at a
            // bogus disk path so the variant is constructed without
            // requiring an actual bundle on disk.
            bundle: Bundle::Disk(std::path::PathBuf::from("/dev/null")),
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
        assert_eq!(v["kind"], "write_feedback");
        // Path is now on the variant payload, not a separate
        // locations array.
        assert!(v["path"].as_str().unwrap().ends_with("/codex.md"));
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
    async fn http_4xx_on_invalid_role() {
        // The wire's `WaitArgs.role` is the typed `WaitingRole`
        // enum (Phase 4 conversion). Unknown role strings fail at
        // axum's `Json<WaitArgs>` body deserializer, which returns
        // 422 Unprocessable Entity — the correct status for
        // "well-formed JSON, invalid value."
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
        assert!(
            resp.status().is_client_error(),
            "expected 4xx, got {}",
            resp.status()
        );
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
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
        assert_eq!(v["result"]["kind"], "write_feedback");
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
        assert_eq!(v["result"]["kind"], "write_feedback");
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
        // Acceptance: HTTP and MCP dispatch produce equivalent JSON
        // responses for the same inputs (modulo MCP's {result: ...}
        // envelope). Drive both routes against the *same* router (both
        // Router and AppState are Clone) so the underlying state
        // matches.
        //
        // Note: the WFW opportunistic-content cache means the first
        // request gets `plan_file.content` populated, the second does
        // not. Strip that field from both bodies before comparing —
        // the test pins the non-opportunistic shape parity, not the
        // wait-only enrichment.
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
        let mut http_body = body_json(http_resp).await;

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
        let mut mcp_body = body_json(mcp_resp).await;

        strip_opportunistic_content(&mut http_body);
        strip_opportunistic_content(&mut mcp_body["result"]);

        assert_eq!(
            http_body, mcp_body["result"],
            "HTTP and MCP wait_for_work bodies diverged outside opportunistic content"
        );
    }

    /// Drop opportunistic inline content (the wait-only enrichment) so
    /// HTTP and MCP responses can be compared on their projection-only
    /// fields.
    fn strip_opportunistic_content(v: &mut serde_json::Value) {
        if let Some(obj) = v.as_object_mut() {
            if let Some(pf) = obj.get_mut("plan_file")
                && let Some(pf_obj) = pf.as_object_mut()
            {
                pf_obj.remove("content");
            }
            if let Some(reviews) = obj.get_mut("reviews")
                && let Some(arr) = reviews.as_array_mut()
            {
                for r in arr {
                    if let Some(r_obj) = r.as_object_mut() {
                        r_obj.remove("content");
                    }
                }
            }
        }
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
                s.plans[&crate::lifecycle::PlanKey::parse("foo").unwrap()]
                    .plan_intro
                    .clone()
            })
            .await
            .unwrap();
        for author in ["codex", "bob"] {
            let rel = format!(".trinity/feedback/foo/{}/{}.md", intro.as_str(), author);
            write_file(dir.path(), &rel, "APPROVE\n");
            let parsed = crate::disk_format::parse_feedback_path(&std::path::PathBuf::from(
                format!("foo/{}/{}.md", intro.as_str(), author),
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
                let plan = &s.plans[&crate::lifecycle::PlanKey::parse("foo").unwrap()];
                crate::projection::all_plan_revisions(plan, s)
                    .into_iter()
                    .next_back()
                    .unwrap()
            })
            .await
            .unwrap();
        let codex_rel = format!(".trinity/feedback/foo/{}/codex.md", revised.as_str());
        write_file(dir.path(), &codex_rel, "APPROVE\n");
        let parsed = crate::disk_format::parse_feedback_path(&std::path::PathBuf::from(format!(
            "foo/{}/codex.md",
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
        assert_eq!(v["kind"], "write_feedback");
        let loc = v["path"].as_str().unwrap();
        assert!(
            loc.ends_with("/bob.md"),
            "expected bob's write path, got {loc}"
        );
    }

    #[tokio::test]
    async fn finish_preview_unreviewed_blocks_with_gate_not_approved() {
        let dir = init_repo();
        let app = router_with_repo(&dir).await;
        let url = format!("/api/plan/{}/finish_preview", plan_id_for(&dir, "foo"));
        let req = Request::builder()
            .method("GET")
            .uri(&url)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["gate_state"], "unreviewed");
        assert_eq!(v["is_finished"], false);
        assert_eq!(v["sealed_approvals"].as_array().unwrap().len(), 0);
        assert_eq!(v["plan_path"], ".trinity/plans/foo.md");
        assert!(v["latest_reviewable_sha"].as_str().is_some());
        assert_eq!(v["readiness"]["kind"], "blocked");
        let reasons = v["readiness"]["reasons"].as_array().unwrap();
        assert!(reasons.iter().any(|r| r["kind"] == "gate_not_approved"));
    }

    #[tokio::test]
    async fn finish_preview_approved_returns_sealed_approvals_with_hashes() {
        let dir = init_repo();
        let (runtime, state) = prepared_state(&dir).await;
        let app = router(state);

        let intro: crate::lifecycle::CommitSha = runtime
            .read_repo(dir.path(), |s| {
                s.plans[&crate::lifecycle::PlanKey::parse("foo").unwrap()]
                    .plan_intro
                    .clone()
            })
            .await
            .unwrap();
        let codex_body = "APPROVE\n\nLooks good.\n";
        let codex_fb = format!(".trinity/feedback/foo/{}/codex.md", intro.as_str());
        write_file(dir.path(), &codex_fb, codex_body);
        runtime
            .handle_signal(
                dir.path(),
                crate::fs_watcher::FilesystemSignal::FeedbackWritten {
                    parsed: crate::disk_format::parse_feedback_path(&std::path::PathBuf::from(
                        format!("foo/{}/codex.md", intro.as_str()),
                    ))
                    .unwrap(),
                },
                1,
            )
            .await
            .unwrap();

        let url = format!("/api/plan/{}/finish_preview", plan_id_for(&dir, "foo"));
        let req = Request::builder()
            .method("GET")
            .uri(&url)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["gate_state"], "approved");
        assert_eq!(v["is_finished"], false);
        assert_eq!(v["readiness"]["kind"], "ready");
        let approvals = v["sealed_approvals"].as_array().unwrap();
        assert_eq!(approvals.len(), 1);
        assert_eq!(approvals[0]["author"], "codex");
        assert_eq!(approvals[0]["source_path"], codex_fb);
        let expected_hash = crate::lifecycle::content_hash(codex_body);
        assert_eq!(approvals[0]["body_hash"], expected_hash.as_str());
    }

    #[tokio::test]
    async fn finish_preview_request_changes_returns_empty_approvals() {
        let dir = init_repo();
        let (runtime, state) = prepared_state(&dir).await;
        let app = router(state);
        let intro: crate::lifecycle::CommitSha = runtime
            .read_repo(dir.path(), |s| {
                s.plans[&crate::lifecycle::PlanKey::parse("foo").unwrap()]
                    .plan_intro
                    .clone()
            })
            .await
            .unwrap();
        write_file(
            dir.path(),
            &format!(".trinity/feedback/foo/{}/codex.md", intro.as_str()),
            "REQUEST_CHANGES\n\nNo.\n",
        );
        runtime
            .handle_signal(
                dir.path(),
                crate::fs_watcher::FilesystemSignal::FeedbackWritten {
                    parsed: crate::disk_format::parse_feedback_path(&std::path::PathBuf::from(
                        format!("foo/{}/codex.md", intro.as_str()),
                    ))
                    .unwrap(),
                },
                1,
            )
            .await
            .unwrap();

        let url = format!("/api/plan/{}/finish_preview", plan_id_for(&dir, "foo"));
        let req = Request::builder()
            .method("GET")
            .uri(&url)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["gate_state"], "changes_requested");
        assert_eq!(v["sealed_approvals"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn rewrite_preview_drops_plan_only_commit() {
        let dir = init_repo();
        let (_runtime, state) = prepared_state(&dir).await;
        let app = router(state);
        let url = format!("/api/plan/{}/rewrite_preview", plan_id_for(&dir, "foo"));
        let req = Request::builder()
            .method("GET")
            .uri(&url)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["plan_stem"], "foo");
        assert_eq!(v["linear"], true);
        let commits = v["commits"].as_array().unwrap();
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0]["disposition"], "drop");
        assert_eq!(commits[0]["foreign"], false);
        assert!(commits[0]["strip_paths"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn rewrite_preview_keeps_pure_code_commit_verbatim() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "add foo");
        write_file(dir.path(), "src/main.rs", "fn main() {}\n");
        commit(dir.path(), "code commit");
        let runtime = Arc::new(Runtime::new());
        runtime.add_repo(dir.path().to_path_buf()).await.unwrap();
        let state = AppState {
            runtime: Arc::clone(&runtime),
            watchers: Arc::new(Mutex::new(std::collections::BTreeMap::new())),
            bundle: Bundle::Disk(std::path::PathBuf::from("/dev/null")),
            repos_path: std::path::PathBuf::from("/dev/null"),
        };
        let app = router(state);
        let url = format!("/api/plan/{}/rewrite_preview", plan_id_for(&dir, "foo"));
        let req = Request::builder()
            .method("GET")
            .uri(&url)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        let commits = v["commits"].as_array().unwrap();
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0]["disposition"], "drop", "plan intro commit");
        assert_eq!(commits[1]["disposition"], "keep_verbatim", "code commit");
        assert_eq!(
            commits[1]["foreign"], false,
            "code commit is attributed via walk-back"
        );
    }

    #[tokio::test]
    async fn rewrite_preview_rewrites_mixed_commit() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "add foo");
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo v2\n");
        write_file(dir.path(), "src/lib.rs", "// code\n");
        commit(dir.path(), "mixed: revise foo + code");
        let runtime = Arc::new(Runtime::new());
        runtime.add_repo(dir.path().to_path_buf()).await.unwrap();
        let state = AppState {
            runtime: Arc::clone(&runtime),
            watchers: Arc::new(Mutex::new(std::collections::BTreeMap::new())),
            bundle: Bundle::Disk(std::path::PathBuf::from("/dev/null")),
            repos_path: std::path::PathBuf::from("/dev/null"),
        };
        let app = router(state);
        let url = format!("/api/plan/{}/rewrite_preview", plan_id_for(&dir, "foo"));
        let req = Request::builder()
            .method("GET")
            .uri(&url)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        let commits = v["commits"].as_array().unwrap();
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[1]["disposition"], "rewrite");
        let strip = commits[1]["strip_paths"].as_array().unwrap();
        assert_eq!(strip.len(), 1);
        assert_eq!(strip[0], ".trinity/plans/foo.md");
    }

    #[tokio::test]
    async fn rewrite_preview_marks_multi_plan_commit_foreign() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "add foo");
        // One commit touching BOTH foo and bar — this is the
        // MultiPlan / cross-plan case. It will appear in foo's
        // timeline as a MultiPlan event but must be `foreign: true`.
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo v2\n");
        write_file(dir.path(), ".trinity/plans/bar.md", "# bar\n");
        commit(dir.path(), "cross-plan: foo + bar");
        let runtime = Arc::new(Runtime::new());
        runtime.add_repo(dir.path().to_path_buf()).await.unwrap();
        let state = AppState {
            runtime: Arc::clone(&runtime),
            watchers: Arc::new(Mutex::new(std::collections::BTreeMap::new())),
            bundle: Bundle::Disk(std::path::PathBuf::from("/dev/null")),
            repos_path: std::path::PathBuf::from("/dev/null"),
        };
        let app = router(state);
        let url = format!("/api/plan/{}/rewrite_preview", plan_id_for(&dir, "foo"));
        let req = Request::builder()
            .method("GET")
            .uri(&url)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        let commits = v["commits"].as_array().unwrap();
        assert_eq!(commits.len(), 2);
        assert_eq!(
            commits[1]["foreign"], true,
            "cross-plan commit must be foreign"
        );
    }

    #[tokio::test]
    async fn rewrite_preview_marks_other_plan_commit_foreign() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "add foo");
        write_file(dir.path(), ".trinity/plans/bar.md", "# bar\n");
        commit(dir.path(), "add bar");
        let runtime = Arc::new(Runtime::new());
        runtime.add_repo(dir.path().to_path_buf()).await.unwrap();
        let state = AppState {
            runtime: Arc::clone(&runtime),
            watchers: Arc::new(Mutex::new(std::collections::BTreeMap::new())),
            bundle: Bundle::Disk(std::path::PathBuf::from("/dev/null")),
            repos_path: std::path::PathBuf::from("/dev/null"),
        };
        let app = router(state);
        let url = format!("/api/plan/{}/rewrite_preview", plan_id_for(&dir, "foo"));
        let req = Request::builder()
            .method("GET")
            .uri(&url)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        let commits = v["commits"].as_array().unwrap();
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0]["foreign"], false);
        assert_eq!(commits[1]["disposition"], "keep_verbatim");
        assert_eq!(commits[1]["foreign"], true, "bar commit is foreign to foo");
    }

    fn repo_basename(dir: &tempfile::TempDir) -> String {
        dir.path()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_string()
    }

    #[tokio::test]
    async fn rewrite_preview_all_drops_plan_only_keeps_pure_code() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "add foo");
        write_file(dir.path(), "src/main.rs", "fn main() {}\n");
        commit(dir.path(), "code");
        let runtime = Arc::new(Runtime::new());
        runtime.add_repo(dir.path().to_path_buf()).await.unwrap();
        let state = AppState {
            runtime,
            watchers: Arc::new(Mutex::new(std::collections::BTreeMap::new())),
            bundle: Bundle::Disk(std::path::PathBuf::from("/dev/null")),
            repos_path: std::path::PathBuf::from("/dev/null"),
        };
        let app = router(state);
        let basename = repo_basename(&dir);
        let url = format!("/api/repos/{basename}/rewrite_preview_all");
        let req = Request::builder()
            .method("GET")
            .uri(&url)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["repo"], basename);
        assert_eq!(v["linear"], true);
        assert!(v["intro_sha"].as_str().is_some());
        let commits = v["commits"].as_array().unwrap();
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0]["disposition"], "drop");
        assert_eq!(commits[1]["disposition"], "keep_verbatim");
        assert_eq!(v["plans_touched"].as_array().unwrap().len(), 1);
        assert_eq!(v["plans_touched"][0], "foo");
    }

    #[tokio::test]
    async fn rewrite_preview_all_rewrites_non_plan_trinity_path() {
        // The codex-flagged case: a commit touching code + a
        // non-plan Trinity path like `.trinity/.gitignore` must
        // appear as Rewrite with that path in strip_paths.
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "add foo");
        write_file(dir.path(), ".trinity/.gitignore", "feedback/\ncache/\n");
        write_file(dir.path(), "src/main.rs", "fn main() {}\n");
        commit(dir.path(), "mixed: gitignore + code");
        let runtime = Arc::new(Runtime::new());
        runtime.add_repo(dir.path().to_path_buf()).await.unwrap();
        let state = AppState {
            runtime,
            watchers: Arc::new(Mutex::new(std::collections::BTreeMap::new())),
            bundle: Bundle::Disk(std::path::PathBuf::from("/dev/null")),
            repos_path: std::path::PathBuf::from("/dev/null"),
        };
        let app = router(state);
        let basename = repo_basename(&dir);
        let url = format!("/api/repos/{basename}/rewrite_preview_all");
        let req = Request::builder()
            .method("GET")
            .uri(&url)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        let commits = v["commits"].as_array().unwrap();
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[1]["disposition"], "rewrite");
        let strip: Vec<&str> = commits[1]["strip_paths"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(
            strip.contains(&".trinity/.gitignore"),
            "non-plan Trinity path missing from strip_paths: {strip:?}"
        );
    }

    #[tokio::test]
    async fn rewrite_preview_all_empty_repo_returns_no_intro() {
        let dir = init_repo();
        write_file(dir.path(), "README.md", "seed\n");
        commit(dir.path(), "seed");
        let runtime = Arc::new(Runtime::new());
        runtime.add_repo(dir.path().to_path_buf()).await.unwrap();
        let state = AppState {
            runtime,
            watchers: Arc::new(Mutex::new(std::collections::BTreeMap::new())),
            bundle: Bundle::Disk(std::path::PathBuf::from("/dev/null")),
            repos_path: std::path::PathBuf::from("/dev/null"),
        };
        let app = router(state);
        let basename = repo_basename(&dir);
        let url = format!("/api/repos/{basename}/rewrite_preview_all");
        let req = Request::builder()
            .method("GET")
            .uri(&url)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert!(
            v["intro_sha"].is_null(),
            "no .trinity/ history → intro_sha=None"
        );
        assert_eq!(v["plans_touched"].as_array().unwrap().len(), 0);
        // The seed commit is still in `commits` but classified KeepVerbatim.
        let commits = v["commits"].as_array().unwrap();
        for c in commits {
            assert_eq!(c["disposition"], "keep_verbatim");
        }
    }

    #[tokio::test]
    async fn rewrite_preview_all_finalize_toggle_changes_disposition() {
        // Reproduces the codex-flagged contract: include_finalize
        // must default to true on the all-plans endpoint, and
        // explicitly setting it to false changes a finalize-only
        // commit's disposition.
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "add foo");
        // A finalize commit (only touches `.trinity/finished/`).
        write_file(
            dir.path(),
            ".trinity/finished/foo/codex.md",
            "APPROVE\n\nlgtm\n",
        );
        commit(dir.path(), "Finalize foo");
        let runtime = Arc::new(Runtime::new());
        runtime.add_repo(dir.path().to_path_buf()).await.unwrap();
        let state = AppState {
            runtime,
            watchers: Arc::new(Mutex::new(std::collections::BTreeMap::new())),
            bundle: Bundle::Disk(std::path::PathBuf::from("/dev/null")),
            repos_path: std::path::PathBuf::from("/dev/null"),
        };
        let app = router(state);
        let basename = repo_basename(&dir);

        // Default (no query string): include_finalize defaults to TRUE
        // for the all-plans endpoint. The finalize commit is Drop.
        let req = Request::builder()
            .method("GET")
            .uri(format!("/api/repos/{basename}/rewrite_preview_all"))
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        let commits = v["commits"].as_array().unwrap();
        assert_eq!(commits.len(), 2);
        assert_eq!(
            commits[1]["disposition"], "drop",
            "finalize commit Drops when include_finalize defaults true"
        );

        // Explicit include_finalize=false: finalize commit is
        // KeepVerbatim (the finalize files survive in the new tree).
        let req = Request::builder()
            .method("GET")
            .uri(format!(
                "/api/repos/{basename}/rewrite_preview_all?include_finalize=false"
            ))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        let commits = v["commits"].as_array().unwrap();
        assert_eq!(
            commits[1]["disposition"], "keep_verbatim",
            "finalize commit kept verbatim when include_finalize=false"
        );
    }

    #[tokio::test]
    async fn rewrite_preview_all_unknown_repo_404s() {
        let dir = init_repo();
        let app = router_with_repo(&dir).await;
        let req = Request::builder()
            .method("GET")
            .uri("/api/repos/nonexistent/rewrite_preview_all")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn finish_preview_unknown_plan_404s() {
        let dir = init_repo();
        let app = router_with_repo(&dir).await;
        let basename = dir.path().file_name().unwrap().to_str().unwrap();
        let req = Request::builder()
            .method("GET")
            .uri(format!(
                "/api/plan/{}/nonexistent.md/finish_preview",
                basename
            ))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
