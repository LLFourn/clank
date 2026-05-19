//! MCP tool dispatch over the runtime.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::AppState;
use super::wait::{WaitArgs, WaitError, wait_for_work as run_wait_for_work};
use crate::lifecycle::{AgentLabel, PlanId, PlanKey, RepoBasename};
use crate::responses::{list_plans_response, work_context_response};

#[derive(Debug, Deserialize)]
pub struct ToolCallRequest {
    pub cwd: PathBuf,
    pub tool: String,
    pub arguments: Value,
}

#[derive(Debug, Serialize)]
pub struct ToolCallResponse {
    pub result: Value,
}

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("invalid: {0}")]
    Invalid(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("forbidden: {0}")]
    Forbidden(String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

pub async fn dispatch(state: &AppState, req: &ToolCallRequest) -> Result<Value, ToolError> {
    match req.tool.as_str() {
        "echo_cwd" => Ok(json!({"cwd": req.cwd})),
        "list_plans" => list_plans(state, req).await,
        "watch_repo" => watch_repo(state, req).await,
        "start_plan" => start_plan(state, req).await,
        "work_context" => work_context(state, req).await,
        "set_active_work" => set_active_work(state, req).await,
        "clear_active_work" => clear_active_work(state, req).await,
        "wait_for_work" => wait_for_work(state, req).await,
        other => Err(ToolError::NotFound(format!("unknown tool: {other}"))),
    }
}

async fn wait_for_work(state: &AppState, req: &ToolCallRequest) -> Result<Value, ToolError> {
    let mut args: WaitArgs = serde_json::from_value(req.arguments.clone())
        .map_err(|e| ToolError::Invalid(format!("args: {e}")))?;

    // Phase 2.9: plan_id is optional. `resolve_plan_id` normalizes
    // blank → absent, so we can hand it whatever the caller sent
    // (including `""`) without a pre-check here. NoActives →
    // timeout-family shape `{timed_out: true, no_active_plans: true}`
    // to match the long-poll vocabulary. Ambiguous → structured
    // error.
    match resolve_plan_id(
        state,
        Some(args.plan_id.as_str()),
        args.repo.as_deref(),
        args.author_label.as_deref(),
        &req.cwd,
    )
    .await?
    {
        PlanIdResolution::Resolved(id) => {
            args.plan_id = id.to_string();
        }
        PlanIdResolution::NoActives { repo } => {
            let timeout =
                trinity_core::api::WaitForWorkResponse::Timeout(trinity_core::api::WaitTimeout {
                    timed_out: true,
                    no_active_plans: true,
                    repo: Some(repo.to_string_lossy().into_owned()),
                });
            return serde_json::to_value(timeout)
                .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)));
        }
        PlanIdResolution::Ambiguous { candidates } => {
            return mcp_error(trinity_core::api::McpErrorPayload::AmbiguousPlan {
                message: "multiple active plans; pass plan_id explicitly".into(),
                candidates,
            });
        }
    }

    let resp = run_wait_for_work(&state.runtime, args)
        .await
        .map_err(map_wait_error)?;
    serde_json::to_value(resp).map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))
}

fn map_wait_error(e: WaitError) -> ToolError {
    match e {
        WaitError::InvalidRole(_)
        | WaitError::MissingPlanId
        | WaitError::MissingAuthorLabel
        | WaitError::InvalidAuthorLabel(_)
        | WaitError::InvalidPlanId(_) => ToolError::Invalid(e.to_string()),
        WaitError::UnknownRepo(_) | WaitError::UnknownPlan(_) | WaitError::PlanConflict { .. } => {
            ToolError::NotFound(e.to_string())
        }
        WaitError::Io(_) => ToolError::Internal(anyhow::anyhow!(e)),
    }
}

#[derive(Debug, Deserialize, Default)]
struct ListPlansArgs {
    /// Optional filter: basename or absolute repo path. Absent = all
    /// watched repos.
    repo: Option<String>,
}

async fn list_plans(state: &AppState, req: &ToolCallRequest) -> Result<Value, ToolError> {
    let args: ListPlansArgs = if req.arguments.is_null() {
        ListPlansArgs::default()
    } else {
        serde_json::from_value(req.arguments.clone())
            .map_err(|e| ToolError::Invalid(format!("args: {e}")))?
    };
    let repo_filter = match args.repo.as_deref() {
        None => None,
        Some(s) => Some(resolve_repo_filter(state, s, &req.cwd).await?),
    };
    let snapshots = if let Some(repo) = repo_filter {
        ensure_registered_or_reject(state, &repo).await?;
        let snap = state
            .runtime
            .snapshot_repo(&repo)
            .await
            .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
        vec![snap]
    } else {
        // No filter — fall back to caller's cwd-repo. (Multi-repo
        // aggregation isn't useful through MCP; HTTP /api/plans returns
        // the cross-repo view.)
        let repo = resolve_repo(&req.cwd).await?;
        ensure_registered_or_reject(state, &repo).await?;
        let snap = state
            .runtime
            .snapshot_repo(&repo)
            .await
            .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
        vec![snap]
    };
    // Single-repo only for MCP today; combine when we add multi-repo
    // aggregation. For now the first snapshot's response is the answer.
    let snap = snapshots.into_iter().next().unwrap();
    let response =
        list_plans_response(&snap).map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
    serde_json::to_value(response).map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))
}

/// Resolve a `repo` arg (filter form): accept either a basename
/// (looks it up in `Trinity.repo_basenames`) or an absolute path
/// (canonicalize via `dunce`).
async fn resolve_repo_filter(
    state: &AppState,
    raw: &str,
    cwd: &Path,
) -> Result<PathBuf, ToolError> {
    if raw.contains('/') || raw.starts_with('~') {
        // Looks like a path — fall back to old path-resolution.
        return resolve_repo_with_override(Some(raw), cwd).await;
    }
    let name = RepoBasename::parse(raw)
        .map_err(|e| ToolError::Invalid(format!("invalid repo basename: {e}")))?;
    let trinity = state.runtime.state();
    let trinity = trinity.lock().await;
    trinity
        .repo_basenames
        .get(&name)
        .cloned()
        .ok_or_else(|| ToolError::NotFound(format!("unknown repo basename: {raw}")))
}

#[derive(Debug, Deserialize)]
struct StartPlanArgs {
    slug: String,
    #[allow(dead_code)]
    label: String,
}

async fn start_plan(state: &AppState, req: &ToolCallRequest) -> Result<Value, ToolError> {
    let args: StartPlanArgs = serde_json::from_value(req.arguments.clone())
        .map_err(|e| ToolError::Invalid(format!("args: {e}")))?;
    let plan_key =
        PlanKey::parse(&args.slug).map_err(|e| ToolError::Invalid(format!("invalid slug: {e}")))?;
    let repo = resolve_repo(&req.cwd).await?;
    let basename = RepoBasename::from_repo_root(&repo).ok_or_else(|| {
        ToolError::Invalid(format!(
            "repo path has no usable basename: {}",
            repo.display()
        ))
    })?;
    let plan_path = PathBuf::from(format!(".trinity/plans/{}.md", args.slug));

    // Atomically register this repo (or report a basename collision)
    // BEFORE any disk mutation. `ensure_registered_or_reject` is the only
    // place where the collision is detected under a single lock-held
    // critical section in `Runtime::add_repo`; doing it first means two
    // concurrent `start_plan` calls from basename-twins both lose disk
    // mutations on the loser side (the first wins registration; the
    // second returns Forbidden without ever touching disk).
    ensure_registered_or_reject(state, &repo).await?;
    ensure_gitignore(&repo)?;
    persist_repo_in_registry(&state.repos_path, &repo)
        .map_err(|e| ToolError::Internal(anyhow::anyhow!("register repo: {e}")))?;
    ensure_repo_watcher(state, repo.clone()).await;

    {
        let trinity = state.runtime.state();
        let trinity = trinity.lock().await;
        if let Some(repo_state) = trinity.repos.get(&repo) {
            if let Some(existing) = repo_state.plans.get(&plan_key) {
                return Err(ToolError::Forbidden(format!(
                    "plan stem `{}` already exists at {}",
                    plan_key.as_str(),
                    existing.plan_path
                )));
            }
            if repo_state.plan_conflicts.contains_key(&plan_key) {
                return Err(ToolError::Forbidden(format!(
                    "plan stem `{}` is in a conflict state; resolve before starting a new plan",
                    plan_key.as_str()
                )));
            }
        }
    }

    let plan_abs = repo.join(&plan_path);
    if let Some(parent) = plan_abs.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
    }
    if !plan_abs.exists() {
        std::fs::write(&plan_abs, "").map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
    }
    let committed = state
        .runtime
        .snapshot_session(&repo, &plan_key)
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?
        .is_some();
    let plan_id = PlanId::new(basename, plan_key.clone());
    let plan_path_display = plan_path.display();
    let next_step = if committed {
        format!(
            "plan already committed at {plan_path_display}; edit and commit again to record a new revision"
        )
    } else {
        format!(
            "edit {plan_path_display} then run: git add {plan_path_display} && git commit -m 'Start plan: {}'",
            plan_key.as_str()
        )
    };
    let response = trinity_core::api::StartPlanResponse {
        plan_id: plan_id.to_string(),
        repo: repo.to_string_lossy().into_owned(),
        canonical_path: plan_abs.to_string_lossy().into_owned(),
        slug: plan_key.as_str().to_string(),
        committed,
        next_step,
    };
    serde_json::to_value(response).map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))
}

/// Serialize a typed [`McpErrorPayload`] into the dispatch result.
/// Used by every `Err(structured-shape)`-style return in this module
/// — kept as a helper so the json-to-Value step is one line per call
/// site instead of repeated inline.
fn mcp_error(payload: trinity_core::api::McpErrorPayload) -> Result<Value, ToolError> {
    serde_json::to_value(payload).map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))
}

#[derive(Debug, Deserialize)]
struct WorkContextArgs {
    #[serde(default)]
    plan_id: Option<String>,
    #[serde(default)]
    repo: Option<String>,
    author_label: Option<String>,
}

async fn work_context(state: &AppState, req: &ToolCallRequest) -> Result<Value, ToolError> {
    let args: WorkContextArgs = serde_json::from_value(req.arguments.clone())
        .map_err(|e| ToolError::Invalid(format!("args: {e}")))?;
    let author_raw = args
        .author_label
        .as_deref()
        .unwrap_or("anonymous")
        .to_string();
    let author = AgentLabel::parse(&author_raw)
        .map_err(|e| ToolError::Invalid(format!("invalid author_label: {e}")))?;

    // Plan-id inference: explicit > active-work selection >
    // repo+single-active-visible > cwd+single-active-visible.
    // Zero actives → no_active_plan error. Multiple → ambiguous_plan
    // with candidate list.
    let plan_id = match resolve_plan_id(
        state,
        args.plan_id.as_deref(),
        args.repo.as_deref(),
        args.author_label.as_deref(),
        &req.cwd,
    )
    .await?
    {
        PlanIdResolution::Resolved(id) => id,
        PlanIdResolution::NoActives { repo } => {
            return mcp_error(trinity_core::api::McpErrorPayload::NoActivePlan {
                repo: repo.to_string_lossy().into_owned(),
                message: format!("no active plans in {}", repo.display()),
            });
        }
        PlanIdResolution::Ambiguous { candidates } => {
            return mcp_error(trinity_core::api::McpErrorPayload::AmbiguousPlan {
                message: "multiple active plans; pass plan_id explicitly".into(),
                candidates,
            });
        }
    };

    // Resolve repo via basename index, then look up plan within it.
    let lookup = {
        let trinity = state.runtime.state();
        let trinity = trinity.lock().await;
        let Some(repo_root) = trinity.repo_basenames.get(plan_id.repo()) else {
            return mcp_error(trinity_core::api::McpErrorPayload::UnknownRepo {
                basename: plan_id.repo().as_str().to_string(),
            });
        };
        let Some(repo_state) = trinity.repos.get(repo_root) else {
            return Err(ToolError::Internal(anyhow::anyhow!(
                "repo_basenames out of sync with repos: {}",
                repo_root.display()
            )));
        };
        if let Some(paths) = repo_state.plan_conflicts.get(plan_id.key()) {
            return mcp_error(trinity_core::api::McpErrorPayload::PlanConflict {
                slug: plan_id.key().as_str().to_string(),
                paths: paths
                    .iter()
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect(),
            });
        }
        if !repo_state.plans.contains_key(plan_id.key()) {
            return mcp_error(trinity_core::api::McpErrorPayload::PlanNotCommitted {
                plan_id: plan_id.to_string(),
                slug: plan_id.key().as_str().to_string(),
                next_step: format!(
                    "commit the plan file: git add .trinity/plans/{slug}.md && git commit -m 'Start plan: {slug}'",
                    slug = plan_id.key().as_str()
                ),
            });
        }
        repo_root.clone()
    };

    let snapshot = state
        .runtime
        .snapshot_session(&lookup, plan_id.key())
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?
        .ok_or_else(|| {
            ToolError::NotFound(format!(
                "plan vanished between resolve and snapshot: {}",
                plan_id
            ))
        })?;
    let response = work_context_response(&snapshot, &author)
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?
        .ok_or_else(|| {
            // Plan is hidden: its file is missing from the working
            // tree and it isn't frozen. Trinity treats it as
            // nonexistent until the operator restores or commits
            // the deletion (see Plan::is_visible).
            ToolError::NotFound(format!(
                "plan {} is hidden: plan file missing from working tree",
                plan_id
            ))
        })?;
    serde_json::to_value(response).map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))
}

#[derive(Debug, Deserialize)]
struct WatchRepoArgs {
    #[serde(default)]
    path: Option<String>,
}

async fn watch_repo(state: &AppState, req: &ToolCallRequest) -> Result<Value, ToolError> {
    let args: WatchRepoArgs = if req.arguments.is_null() {
        WatchRepoArgs { path: None }
    } else {
        serde_json::from_value(req.arguments.clone())
            .map_err(|e| ToolError::Invalid(format!("args: {e}")))?
    };
    // Path semantics:
    //   absent      → caller's cwd via `git rev-parse --show-toplevel`
    //   absolute    → use as-is, canonicalize
    //   relative    → resolve against cwd, canonicalize
    let repo = match args
        .path
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        None => resolve_repo(&req.cwd).await?,
        Some(raw) => {
            let raw_path = PathBuf::from(raw);
            let abs = if raw_path.is_absolute() {
                raw_path
            } else {
                req.cwd.join(raw_path)
            };
            dunce::canonicalize(&abs).unwrap_or(abs)
        }
    };
    let basename = RepoBasename::from_repo_root(&repo).ok_or_else(|| {
        ToolError::Invalid(format!(
            "repo path has no usable basename: {}",
            repo.display()
        ))
    })?;

    // Check whether this repo was already in the watched set so we
    // can report it back as `already_watching` vs `registered`. The
    // brief race between this check and `add_repo_if_unknown` is
    // cosmetic for the response shape; the registration itself is
    // idempotent.
    let already_known = {
        let trinity = state.runtime.state();
        let trinity = trinity.lock().await;
        trinity.repos.contains_key(&repo)
    };

    match state
        .runtime
        .add_repo_if_unknown(repo.clone())
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?
    {
        crate::runtime::RegisterOutcome::Registered => {}
        crate::runtime::RegisterOutcome::ShadowedByOther { claimed_by } => {
            return Err(ToolError::Forbidden(format!(
                "repo basename `{}` is already watched at {}; rename one to disambiguate",
                basename.as_str(),
                claimed_by.display()
            )));
        }
    }

    let response = trinity_core::api::WatchRepoResponse {
        repo: repo.to_string_lossy().into_owned(),
        basename: basename.as_str().to_string(),
        status: if already_known {
            trinity_core::api::WatchRepoStatus::AlreadyWatching
        } else {
            trinity_core::api::WatchRepoStatus::Registered
        },
    };
    serde_json::to_value(response).map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))
}

#[derive(Debug, Deserialize)]
struct SetActiveWorkArgs {
    plan_id: String,
    #[serde(default)]
    repo: Option<String>,
    author_label: Option<String>,
}

async fn set_active_work(state: &AppState, req: &ToolCallRequest) -> Result<Value, ToolError> {
    let args: SetActiveWorkArgs = serde_json::from_value(req.arguments.clone())
        .map_err(|e| ToolError::Invalid(format!("args: {e}")))?;
    let author_raw = args
        .author_label
        .as_deref()
        .ok_or_else(|| ToolError::Invalid("author_label is required".into()))?;
    let author = AgentLabel::parse(author_raw)
        .map_err(|e| ToolError::Invalid(format!("invalid author_label: {e}")))?;
    let plan_id = PlanId::parse(&args.plan_id)
        .map_err(|e| ToolError::Invalid(format!("invalid plan_id: {e}")))?;

    // If `repo` arg is supplied, validate it resolves to the same
    // basename as plan_id's repo segment.
    if let Some(repo_arg) = args
        .repo
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let resolved = resolve_repo_filter(state, repo_arg, &req.cwd).await?;
        let resolved_basename = RepoBasename::from_repo_root(&resolved).ok_or_else(|| {
            ToolError::Internal(anyhow::anyhow!(
                "resolved repo has no usable basename: {}",
                resolved.display()
            ))
        })?;
        if resolved_basename != *plan_id.repo() {
            return Err(ToolError::Invalid(format!(
                "repo arg `{}` resolves to basename `{}` but plan_id basename is `{}`",
                repo_arg,
                resolved_basename.as_str(),
                plan_id.repo().as_str()
            )));
        }
    }

    // Snapshot the candidate under lock; release; disk-read; decide.
    let snapshot = {
        let trinity = state.runtime.state();
        let trinity = trinity.lock().await;
        let Some(repo_root) = trinity.repo_basenames.get(plan_id.repo()) else {
            return mcp_error(trinity_core::api::McpErrorPayload::UnknownRepo {
                basename: plan_id.repo().as_str().to_string(),
            });
        };
        let Some(repo_state) = trinity.repos.get(repo_root) else {
            return Err(ToolError::Internal(anyhow::anyhow!(
                "repo_basenames out of sync with repos: {}",
                repo_root.display()
            )));
        };
        let Some(plan) = repo_state.plans.get(plan_id.key()) else {
            return mcp_error(trinity_core::api::McpErrorPayload::PlanNotCommitted {
                plan_id: plan_id.to_string(),
                slug: plan_id.key().as_str().to_string(),
                next_step: format!(
                    "commit the plan file: git add .trinity/plans/{slug}.md && git commit -m 'Start plan: {slug}'",
                    slug = plan_id.key().as_str()
                ),
            });
        };
        if plan.is_frozen() {
            return Err(ToolError::Invalid(format!(
                "plan {} is frozen; cannot set as active work",
                plan_id
            )));
        }
        (
            repo_root.clone(),
            plan.plan_path.clone(),
            plan.body_hash.clone(),
        )
    };
    let (repo_root, plan_path, body_hash) = snapshot;
    let status =
        crate::responses::compute_plan_worktree_status_parts(&repo_root, &plan_path, &body_hash)
            .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
    if matches!(
        status,
        crate::repo_state::PlanWorktreeStatus::PlanFileMissing
    ) {
        return Err(ToolError::NotFound(format!(
            "plan {} is hidden: plan file missing from working tree",
            plan_id
        )));
    }

    state
        .runtime
        .set_active_work(plan_id.repo().clone(), author, plan_id.key().clone())
        .await;
    let response = trinity_core::api::SetActiveWorkResponse {
        ok: true,
        plan_id: plan_id.to_string(),
    };
    serde_json::to_value(response).map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))
}

#[derive(Debug, Deserialize)]
struct ClearActiveWorkArgs {
    #[serde(default)]
    repo: Option<String>,
    author_label: Option<String>,
}

async fn clear_active_work(state: &AppState, req: &ToolCallRequest) -> Result<Value, ToolError> {
    let args: ClearActiveWorkArgs = serde_json::from_value(req.arguments.clone())
        .map_err(|e| ToolError::Invalid(format!("args: {e}")))?;
    let author_raw = args
        .author_label
        .as_deref()
        .ok_or_else(|| ToolError::Invalid("author_label is required".into()))?;
    let author = AgentLabel::parse(author_raw)
        .map_err(|e| ToolError::Invalid(format!("invalid author_label: {e}")))?;
    let repo_path = match args
        .repo
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(raw) => resolve_repo_filter(state, raw, &req.cwd).await?,
        None => resolve_repo(&req.cwd).await?,
    };
    let basename = RepoBasename::from_repo_root(&repo_path).ok_or_else(|| {
        ToolError::Internal(anyhow::anyhow!(
            "repo path has no usable basename: {}",
            repo_path.display()
        ))
    })?;
    state.runtime.clear_active_work(&basename, &author).await;
    let response = trinity_core::api::ClearActiveWorkResponse { ok: true };
    serde_json::to_value(response).map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))
}

async fn resolve_repo(cwd: &Path) -> Result<PathBuf, ToolError> {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!("git spawn: {e}")))?;
    if !output.status.success() {
        return Err(ToolError::Invalid(format!(
            "not in a git repository: {}",
            cwd.display()
        )));
    }
    let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(PathBuf::from(s))
}

/// Resolve the target repo: if `override_path` is `Some`, canonicalize that
/// (dunce, no `~` expansion). Otherwise fall back to `git rev-parse
/// --show-toplevel`.
async fn resolve_repo_with_override(
    override_path: Option<&str>,
    cwd: &Path,
) -> Result<PathBuf, ToolError> {
    match override_path {
        Some(p) => {
            let raw = PathBuf::from(p);
            Ok(dunce::canonicalize(&raw).unwrap_or(raw))
        }
        None => resolve_repo(cwd).await,
    }
}

/// Result of `resolve_plan_id`. Callers translate `NoActives` /
/// `Ambiguous` into surface-specific shapes (long-poll timeout vs
/// structured error). `Resolved` is the happy path — every other
/// caller in this file maps it straight to the existing lookup
/// machinery.
pub enum PlanIdResolution {
    Resolved(PlanId),
    NoActives {
        repo: PathBuf,
    },
    Ambiguous {
        candidates: Vec<trinity_core::api::PlanCandidate>,
    },
}

/// Snapshot of one plan candidate. Used by `resolve_plan_id` to
/// carry just-enough state out of the runtime mutex so the disk
/// read for `PlanWorktreeStatus` happens outside the lock.
struct CandidateSnapshot {
    plan_key: PlanKey,
    plan_path: String,
    body_hash: crate::lifecycle::ContentHash,
    lifecycle: trinity_core::vocab::PlanLifecycle,
}

/// Inference rules:
///
/// 1. Explicit `plan_id` always wins. Parsed and returned as-is.
/// 2. Otherwise resolve a repo via `repo_override` or the caller's
///    cwd.
/// 3. If `author_label` is present and a `set_active_work` selection
///    exists for `(basename, author_label)`, validate it (must be
///    non-frozen and have its worktree file present). Valid → resolve
///    to the selected plan. Invalid → drop the selection and fall
///    through to step 4.
/// 4. Count *active + visible* plans in the repo. A plan is active
///    when `!is_frozen()`; visible when its worktree file is not
///    missing (matches `Plan::is_visible` semantics).
///    - 1 candidate → return it.
///    - 0 candidates → `NoActives { repo }`.
///    - 2+ candidates → `Ambiguous` with the candidate list.
///
/// Lock-boundary discipline: candidate metadata is snapshotted under
/// the runtime mutex into `CandidateSnapshot` values, then the lock
/// is released before any `compute_plan_worktree_status_parts` call.
/// Stale selection drops happen behind a brief second lock-acquire.
pub async fn resolve_plan_id(
    state: &AppState,
    plan_id_override: Option<&str>,
    repo_override: Option<&str>,
    author_label: Option<&str>,
    cwd: &Path,
) -> Result<PlanIdResolution, ToolError> {
    // Normalize blank-string overrides to absent so callers can pass
    // `args.plan_id.as_deref()` (Option<&str>) without having to
    // pre-check the empty case. wait_for_work and work_context used to
    // diverge on this — one trimmed, the other passed raw — and a
    // schema-strict client sending `{"plan_id":""}` would hit
    // `invalid_plan_id` on one and inference on the other.
    if let Some(raw) = plan_id_override.map(str::trim).filter(|s| !s.is_empty()) {
        let id =
            PlanId::parse(raw).map_err(|e| ToolError::Invalid(format!("invalid plan_id: {e}")))?;
        return Ok(PlanIdResolution::Resolved(id));
    }
    let repo_override = repo_override.map(str::trim).filter(|s| !s.is_empty());

    let repo_path = match repo_override {
        Some(raw) => resolve_repo_filter(state, raw, cwd).await?,
        None => resolve_repo(cwd).await?,
    };
    let basename = RepoBasename::from_repo_root(&repo_path).ok_or_else(|| {
        ToolError::Internal(anyhow::anyhow!(
            "repo path has no usable basename: {}",
            repo_path.display()
        ))
    })?;
    let author = author_label
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| AgentLabel::parse(s).ok());

    // Snapshot the repo's candidate plans + any active-work selection
    // under the runtime mutex, then drop the lock before disk reads.
    let (selection_snapshot, candidates) = {
        let trinity = state.runtime.state();
        let trinity = trinity.lock().await;
        let Some(repo_state) = trinity.repos.get(&repo_path) else {
            return Ok(PlanIdResolution::NoActives { repo: repo_path });
        };
        let selection_snapshot = author
            .as_ref()
            .and_then(|a| {
                trinity
                    .active_selections
                    .get(&(basename.clone(), a.clone()))
            })
            .and_then(|key| repo_state.plans.get(key))
            .map(|plan| CandidateSnapshot {
                plan_key: plan.id.clone(),
                plan_path: plan.plan_path.clone(),
                body_hash: plan.body_hash.clone(),
                lifecycle: plan.lifecycle(),
            });
        let candidates: Vec<CandidateSnapshot> = repo_state
            .plans
            .values()
            .filter(|p| !p.is_frozen())
            .map(|p| CandidateSnapshot {
                plan_key: p.id.clone(),
                plan_path: p.plan_path.clone(),
                body_hash: p.body_hash.clone(),
                lifecycle: p.lifecycle(),
            })
            .collect();
        (selection_snapshot, candidates)
    };

    // Consult the active-work selection first. Validation requires a
    // disk read but the snapshot is already in hand.
    if let Some(selection) = selection_snapshot {
        let status = crate::responses::compute_plan_worktree_status_parts(
            &repo_path,
            &selection.plan_path,
            &selection.body_hash,
        )
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
        let is_visible = selection.lifecycle == trinity_core::vocab::PlanLifecycle::Active
            && status != crate::repo_state::PlanWorktreeStatus::PlanFileMissing;
        if is_visible {
            return Ok(PlanIdResolution::Resolved(PlanId::new(
                basename,
                selection.plan_key,
            )));
        }
        // Stale: drop it under a brief lock so a parallel reader
        // doesn't keep observing the invalid selection.
        if let Some(a) = author.as_ref() {
            let trinity = state.runtime.state();
            let mut trinity = trinity.lock().await;
            trinity
                .active_selections
                .remove(&(basename.clone(), a.clone()));
        }
        // Fall through to normal counting.
    }

    // Active-visible filter on the snapshot candidates. Each
    // surviving candidate requires its own disk read.
    let mut visible: Vec<CandidateSnapshot> = Vec::with_capacity(candidates.len());
    for c in candidates {
        let status = crate::responses::compute_plan_worktree_status_parts(
            &repo_path,
            &c.plan_path,
            &c.body_hash,
        )
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
        if status != crate::repo_state::PlanWorktreeStatus::PlanFileMissing {
            visible.push(c);
        }
    }

    match visible.len() {
        0 => Ok(PlanIdResolution::NoActives { repo: repo_path }),
        1 => Ok(PlanIdResolution::Resolved(PlanId::new(
            basename,
            visible.into_iter().next().unwrap().plan_key,
        ))),
        _ => {
            let candidates = visible
                .into_iter()
                .map(|c| trinity_core::api::PlanCandidate {
                    plan_id: PlanId::new(basename.clone(), c.plan_key).to_string(),
                    current_path: c.plan_path,
                    lifecycle: c.lifecycle,
                })
                .collect();
            Ok(PlanIdResolution::Ambiguous { candidates })
        }
    }
}

/// Register `repo` with the runtime if it isn't already known, and
/// reject the caller if its basename is shadowed by another canonical
/// path. Used by `start_plan` and `list_plans` so callers can't operate
/// on a repo that the daemon won't actually serve.
async fn ensure_registered_or_reject(state: &AppState, repo: &Path) -> Result<(), ToolError> {
    use crate::runtime::RegisterOutcome;
    match state.runtime.add_repo_if_unknown(repo.to_path_buf()).await {
        Ok(RegisterOutcome::Registered) => Ok(()),
        Ok(RegisterOutcome::ShadowedByOther { claimed_by }) => Err(ToolError::Forbidden(format!(
            "repo basename collides with already-watched {}; rename the directory to disambiguate",
            claimed_by.display()
        ))),
        Err(err) => Err(ToolError::Internal(
            anyhow::Error::new(err).context("add_repo_if_unknown failed"),
        )),
    }
}

/// Spawn a notify watcher for `repo` if one isn't already running.
/// Tracks watched handles by canonical repo root in `AppState.watchers`
/// so `delete_repo` can target and abort a specific watcher.
async fn ensure_repo_watcher(state: &AppState, repo: PathBuf) {
    let canonical = dunce::canonicalize(&repo).unwrap_or_else(|_| repo.clone());
    {
        let watchers = state.watchers.lock().await;
        if watchers.contains_key(&canonical) {
            return;
        }
    }
    match super::notify_bridge::start(state.runtime.clone(), repo.clone()).await {
        Ok(handle) => {
            state.watchers.lock().await.insert(canonical, handle);
        }
        Err(err) => {
            tracing::warn!(repo = %repo.display(), error = ?err, "watcher start failed for new repo");
        }
    }
}

/// Append `repo` to the registry file if not already present. The
/// registry path is the daemon's configured `--repos` / `$TRINITY_REPOS`
/// value (stashed in `AppState.repos_path` at startup), NOT the
/// hardcoded `~/.trinity/repos` — that would lose registrations for
/// non-default deployments.
pub(crate) fn persist_repo_in_registry(registry: &Path, repo: &Path) -> std::io::Result<()> {
    if let Some(parent) = registry.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let existing = std::fs::read_to_string(registry).unwrap_or_default();
    let already_present = existing
        .lines()
        .map(str::trim)
        .any(|l| !l.is_empty() && std::path::Path::new(l) == repo);
    if already_present {
        return Ok(());
    }
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(registry)?;
    writeln!(f, "{}", repo.display())?;
    Ok(())
}

/// Remove every line that resolves to `repo` from the registry file.
/// Re-writes atomically (rename-from-temp) so a partial write can't
/// leave the file truncated.
pub(crate) fn remove_repo_from_registry(registry: &Path, repo: &Path) -> std::io::Result<()> {
    if !registry.exists() {
        return Ok(());
    }
    let existing = std::fs::read_to_string(registry)?;
    // `repo` is the daemon's canonical root (from `Trinity.repo_basenames`).
    // The registry file is human-edited and may carry equivalent
    // non-canonical forms (`~/src/foo`, `./src/foo`, paths with
    // intermediate `..`, symlinks resolved differently). Compare each
    // line by its canonical form when canonicalization succeeds; fall
    // back to a byte compare so an entry pointing at a path that no
    // longer exists on disk can still be removed.
    let mut kept: Vec<&str> = Vec::new();
    let mut changed = false;
    for line in existing.lines() {
        let trimmed = line.trim();
        let matches = if trimmed.is_empty() {
            false
        } else {
            let raw = std::path::Path::new(trimmed);
            match dunce::canonicalize(raw) {
                Ok(canonical) => canonical == repo,
                Err(_) => raw == repo,
            }
        };
        if matches {
            changed = true;
            continue;
        }
        kept.push(line);
    }
    if !changed {
        return Ok(());
    }
    let mut body = kept.join("\n");
    if !body.is_empty() {
        body.push('\n');
    }
    let tmp = registry.with_extension("repos.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, registry)?;
    Ok(())
}

fn ensure_gitignore(repo: &Path) -> Result<(), ToolError> {
    let path = repo.join(".gitignore");
    let body = if path.exists() {
        std::fs::read_to_string(&path).map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?
    } else {
        String::new()
    };
    if body.lines().any(|l| l.trim() == ".trinity/") {
        return Err(ToolError::Invalid(
            "`.gitignore` wholesale-ignores `.trinity/`; scope down to feedback + cache before running start_plan"
                .into(),
        ));
    }
    let mut additions = Vec::new();
    for line in [".trinity/feedback/", ".trinity/cache/"] {
        if !body.lines().any(|l| l.trim() == line) {
            additions.push(line);
        }
    }
    if !additions.is_empty() {
        let mut new_body = body;
        if !new_body.is_empty() && !new_body.ends_with('\n') {
            new_body.push('\n');
        }
        for line in additions {
            new_body.push_str(line);
            new_body.push('\n');
        }
        std::fs::write(&path, new_body).map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
    }
    Ok(())
}
