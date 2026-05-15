//! MCP tool dispatch over the runtime.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::AppState;
use super::wait::{WaitArgs, WaitError, wait_for_work as run_wait_for_work};
use crate::lifecycle::{AgentLabel, SessionId};
use crate::mcp_response::{get_context_response, list_sessions_response};

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
        "list_sessions" => list_sessions(state, req).await,
        "start_plan" => start_plan(state, req).await,
        "get_context" => get_context(state, req).await,
        "wait_for_work" => wait_for_work(state, req).await,
        other => Err(ToolError::NotFound(format!("unknown tool: {other}"))),
    }
}

async fn wait_for_work(state: &AppState, req: &ToolCallRequest) -> Result<Value, ToolError> {
    let mut args: WaitArgs = serde_json::from_value(req.arguments.clone())
        .map_err(|e| ToolError::Invalid(format!("args: {e}")))?;
    // MCP path: if the caller didn't pass `repo`, fall back to the shim cwd.
    if args.repo.is_none() {
        let repo = resolve_repo(&req.cwd).await?;
        args.repo = Some(repo.to_string_lossy().into_owned());
    }
    let resp = run_wait_for_work(&state.runtime, args)
        .await
        .map_err(map_wait_error)?;
    serde_json::to_value(resp).map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))
}

fn map_wait_error(e: WaitError) -> ToolError {
    match e {
        WaitError::InvalidRole(_)
        | WaitError::MissingSessionId
        | WaitError::MissingAuthorLabel
        | WaitError::MissingRepo => ToolError::Invalid(e.to_string()),
        WaitError::UnknownSession(_) => ToolError::NotFound(e.to_string()),
        WaitError::Io(_) => ToolError::Internal(anyhow::anyhow!(e)),
    }
}

#[derive(Debug, Deserialize, Default)]
struct ListSessionsArgs {
    /// Optional absolute repo path. Falls back to req.cwd if absent.
    repo: Option<String>,
}

async fn list_sessions(state: &AppState, req: &ToolCallRequest) -> Result<Value, ToolError> {
    let args: ListSessionsArgs = if req.arguments.is_null() {
        ListSessionsArgs::default()
    } else {
        serde_json::from_value(req.arguments.clone())
            .map_err(|e| ToolError::Invalid(format!("args: {e}")))?
    };
    let repo = resolve_repo_with_override(args.repo.as_deref(), &req.cwd).await?;
    state.runtime.add_repo_if_unknown(repo.clone()).await;
    let snapshot = state
        .runtime
        .snapshot_repo(&repo)
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
    let v =
        list_sessions_response(&snapshot).map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
    Ok(json!({"sessions": v}))
}

#[derive(Debug, Deserialize)]
struct StartPlanArgs {
    session_id: String,
    #[allow(dead_code)]
    label: String,
}

async fn start_plan(state: &AppState, req: &ToolCallRequest) -> Result<Value, ToolError> {
    let args: StartPlanArgs = serde_json::from_value(req.arguments.clone())
        .map_err(|e| ToolError::Invalid(format!("args: {e}")))?;
    let repo = resolve_repo(&req.cwd).await?;
    ensure_gitignore(&repo)?;
    persist_repo_in_registry(&repo)
        .map_err(|e| ToolError::Internal(anyhow::anyhow!("register repo: {e}")))?;
    let plan_rel = PathBuf::from(".trinity/plans").join(format!("{}.md", args.session_id));
    let plan_abs = repo.join(&plan_rel);
    if let Some(parent) = plan_abs.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
    }
    if !plan_abs.exists() {
        std::fs::write(&plan_abs, "").map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
    }
    state.runtime.add_repo_if_unknown(repo.clone()).await;
    ensure_repo_watcher(state, repo.clone()).await;
    let committed = state
        .runtime
        .snapshot_session(&repo, &SessionId::from(args.session_id.clone()))
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?
        .is_some();
    let next_step = if committed {
        format!(
            "plan already committed at {}; edit and commit again to record a new revision",
            plan_rel.display()
        )
    } else {
        format!(
            "edit {} then run: git add {} && git commit -m 'Start plan: {}'",
            plan_rel.display(),
            plan_rel.display(),
            args.session_id
        )
    };
    Ok(json!({
        "canonical_path": plan_abs.to_string_lossy(),
        "committed": committed,
        "next_step": next_step,
    }))
}

#[derive(Debug, Deserialize)]
struct GetContextArgs {
    session_id: String,
    author_label: Option<String>,
    /// Optional absolute repo path. Falls back to req.cwd if absent.
    /// Used when responding to a `wait_for_work` match in a different repo.
    repo: Option<String>,
}

async fn get_context(state: &AppState, req: &ToolCallRequest) -> Result<Value, ToolError> {
    let args: GetContextArgs = serde_json::from_value(req.arguments.clone())
        .map_err(|e| ToolError::Invalid(format!("args: {e}")))?;
    let repo = resolve_repo_with_override(args.repo.as_deref(), &req.cwd).await?;
    state.runtime.add_repo_if_unknown(repo.clone()).await;
    let session_id = SessionId::from(args.session_id.clone());
    let author = AgentLabel::from(args.author_label.unwrap_or_else(|| "anonymous".to_string()));
    let snapshot = state
        .runtime
        .snapshot_session(&repo, &session_id)
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
    match snapshot {
        Some(snapshot) => get_context_response(&snapshot, &author)
            .map_err(|e| ToolError::Internal(anyhow::anyhow!(e))),
        None => {
            let plan_rel = PathBuf::from(".trinity/plans").join(format!("{}.md", args.session_id));
            let plan_abs = repo.join(&plan_rel);
            Ok(json!({
                "error": "session_not_committed",
                "canonical_path": plan_abs.to_string_lossy(),
                "next_step": format!(
                    "commit the plan file: git add {} && git commit -m 'Start plan: {}'",
                    plan_rel.display(),
                    args.session_id
                ),
            }))
        }
    }
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
/// (dunce, no `~` expansion — callers pass absolute paths). Otherwise fall
/// back to `git rev-parse --show-toplevel` from the shim's cwd.
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

/// Append `repo` to `~/.trinity/repos` if not already present. The file is
/// the daemon's persistent registry of known repos; serve() reads it at
/// startup. Missing parent dirs are created.
fn persist_repo_in_registry(repo: &Path) -> std::io::Result<()> {
    let Some(home) = std::env::var_os("HOME") else {
        return Ok(()); // no $HOME; tolerate silently
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
