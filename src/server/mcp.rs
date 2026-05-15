//! MCP tool dispatch over the runtime.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::AppState;
use super::wait::{WaitArgs, WaitError, wait_for_work as run_wait_for_work};
use crate::lifecycle::{AgentLabel, PlanId, PlanKey, RepoBasename};
use crate::mcp_response::{get_context_response, list_plans_response};

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
        "start_plan" => start_plan(state, req).await,
        "get_context" => get_context(state, req).await,
        "wait_for_work" => wait_for_work(state, req).await,
        other => Err(ToolError::NotFound(format!("unknown tool: {other}"))),
    }
}

async fn wait_for_work(state: &AppState, req: &ToolCallRequest) -> Result<Value, ToolError> {
    let args: WaitArgs = serde_json::from_value(req.arguments.clone())
        .map_err(|e| ToolError::Invalid(format!("args: {e}")))?;
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
    list_plans_response(&snap).map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))
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
    let name = RepoBasename::from(raw);
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
    if args.slug.is_empty() || args.slug.contains('/') {
        return Err(ToolError::Invalid(format!(
            "slug must be non-empty and contain no `/`; got `{}`",
            args.slug
        )));
    }
    let repo = resolve_repo(&req.cwd).await?;
    let basename = RepoBasename::from_repo_root(&repo).ok_or_else(|| {
        ToolError::Invalid(format!(
            "repo path has no usable basename: {}",
            repo.display()
        ))
    })?;
    let plan_key = PlanKey::from(args.slug.clone());
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
    persist_repo_in_registry(&repo)
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
                    existing.plan_path.display()
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
    Ok(json!({
        "plan_id": plan_id.to_string(),
        "repo": repo.to_string_lossy(),
        "canonical_path": plan_abs.to_string_lossy(),
        "slug": plan_key.as_str(),
        "committed": committed,
        "next_step": next_step,
    }))
}

#[derive(Debug, Deserialize)]
struct GetContextArgs {
    plan_id: String,
    author_label: Option<String>,
}

async fn get_context(state: &AppState, req: &ToolCallRequest) -> Result<Value, ToolError> {
    let args: GetContextArgs = serde_json::from_value(req.arguments.clone())
        .map_err(|e| ToolError::Invalid(format!("args: {e}")))?;
    let plan_id = PlanId::parse(&args.plan_id)
        .map_err(|e| ToolError::Invalid(format!("invalid plan_id: {e}")))?;
    let author = AgentLabel::from(args.author_label.unwrap_or_else(|| "anonymous".to_string()));

    // Resolve repo via basename index, then look up plan within it.
    let lookup = {
        let trinity = state.runtime.state();
        let trinity = trinity.lock().await;
        let Some(repo_root) = trinity.repo_basenames.get(plan_id.repo()) else {
            return Ok(json!({
                "error": "unknown_repo",
                "basename": plan_id.repo().as_str(),
            }));
        };
        let Some(repo_state) = trinity.repos.get(repo_root) else {
            return Err(ToolError::Internal(anyhow::anyhow!(
                "repo_basenames out of sync with repos: {}",
                repo_root.display()
            )));
        };
        if let Some(paths) = repo_state.plan_conflicts.get(plan_id.key()) {
            return Ok(json!({
                "error": "plan_conflict",
                "slug": plan_id.key().as_str(),
                "paths": paths.iter().map(|p| p.to_string_lossy()).collect::<Vec<_>>(),
            }));
        }
        if !repo_state.plans.contains_key(plan_id.key()) {
            return Ok(json!({
                "error": "plan_not_committed",
                "plan_id": plan_id.to_string(),
                "slug": plan_id.key().as_str(),
                "next_step": format!(
                    "commit the plan file: git add .trinity/plans/{slug}.md && git commit -m 'Start plan: {slug}'",
                    slug = plan_id.key().as_str()
                ),
            }));
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
    get_context_response(&snapshot, &author).map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))
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
/// Tracks watched repos in `AppState.watched_repos` to dedupe.
async fn ensure_repo_watcher(state: &AppState, repo: PathBuf) {
    {
        let watched = state.watched_repos.lock().await;
        if watched.contains(&repo) {
            return;
        }
    }
    match super::notify_bridge::start(state.runtime.clone(), repo.clone()).await {
        Ok(handle) => {
            state.watchers.lock().await.push(handle);
            state.watched_repos.lock().await.insert(repo);
        }
        Err(err) => {
            tracing::warn!(repo = %repo.display(), error = ?err, "watcher start failed for new repo");
        }
    }
}

/// Append `repo` to `~/.trinity/repos` if not already present.
fn persist_repo_in_registry(repo: &Path) -> std::io::Result<()> {
    let Some(home) = std::env::var_os("HOME") else {
        return Ok(());
    };
    let registry = PathBuf::from(home).join(".trinity/repos");
    if let Some(parent) = registry.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let existing = std::fs::read_to_string(&registry).unwrap_or_default();
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
        .open(&registry)?;
    writeln!(f, "{}", repo.display())?;
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
