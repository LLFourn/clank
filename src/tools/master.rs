//! Lifecycle MCP tool: `register_plan_file`. This is the only write-path
//! agent tool in the catalog after the watcher-coordinator refactor.
//! Implementation commits arrive via the `.git/logs/HEAD` watcher;
//! feedback arrives via the feedback-directory watcher.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::daemon::AppState;
use crate::daemon::apply::AppliedEffect;
use crate::daemon::git;
use crate::daemon::internal_api::ToolCallRequest;
use crate::daemon::ServiceError;
use crate::domain::EventKind;
use crate::lifecycle::{AgentLabel, CommitSha, Observation, PlanFilePath, SessionId};
use crate::storage::{
    agents::{self, SeenOutcome},
    events as ev_store, sessions,
};

use super::ToolError;

#[derive(Debug, Deserialize)]
struct RegisterPlanFileArgs {
    session_id: String,
    path: String,
    label: String,
}

pub async fn register_plan_file(
    state: &AppState,
    req: &ToolCallRequest,
) -> Result<Value, ToolError> {
    let args: RegisterPlanFileArgs = serde_json::from_value(req.arguments.clone())
        .map_err(|e| ToolError::Invalid(format!("invalid args: {e}")))?;

    let session_id = sessions::validate_slug(&args.session_id).map_err(ToolError::Invalid)?;
    let label = AgentLabel::from(args.label.trim().to_string());
    if label.as_str().is_empty() {
        return Err(ToolError::Invalid("label must be non-empty".into()));
    }

    let raw_path = expand_home(&args.path);
    let abs_path = if raw_path.is_absolute() {
        raw_path
    } else {
        req.cwd.join(raw_path)
    };
    let canonical = dunce::canonicalize(&abs_path).map_err(|e| {
        ToolError::Invalid(format!(
            "plan file does not exist or is unreadable ({}): {e}",
            abs_path.display()
        ))
    })?;
    if !canonical.is_file() {
        return Err(ToolError::Invalid(format!(
            "plan path is not a regular file: {}",
            canonical.display()
        )));
    }

    let repo_root = git::rev_parse_show_toplevel(&req.cwd).await.map_err(|e| {
        ToolError::Invalid(format!(
            "caller cwd `{}` is not in a git repository: {e}",
            req.cwd.display()
        ))
    })?;
    let repo_root_canonical = dunce::canonicalize(&repo_root)
        .map_err(|e| ToolError::Internal(anyhow::anyhow!("canonicalize repo_root: {e}")))?;
    let repo_root_str = repo_root_canonical.to_string_lossy().into_owned();

    let plan_path_str = canonical.to_string_lossy().into_owned();

    let now = chrono::Utc::now().timestamp();
    let head = CommitSha::from(
        git::rev_parse_head(&repo_root_canonical)
            .await
            .map_err(|e| ToolError::Internal(anyhow::anyhow!("rev-parse HEAD: {e}")))?,
    );
    let body = tokio::fs::read_to_string(&canonical)
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!("read plan file: {e}")))?;

    let existing = sessions::fetch(&state.pool, &session_id)
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;

    if let Some(session) = existing.as_ref() {
        if session.repo_root != repo_root_str {
            return Err(ToolError::Forbidden(format!(
                "session `{}` was created against repo `{}` but caller cwd resolves to `{}`",
                session_id, session.repo_root, repo_root_str
            )));
        }
    } else {
        let display_title = canonical
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("plan")
            .to_string();
        sessions::insert(
            &state.pool,
            &session_id,
            &repo_root_str,
            &PlanFilePath::from(plan_path_str.clone()),
            Some(&display_title),
            now,
        )
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
    }

    upsert_seen_with_event(state, &session_id, &label, now).await?;

    let outcome = state
        .lifecycle
        .observe(
            &session_id,
            &format!("agent:{label}"),
            Observation::PlanRegistered {
                path: PlanFilePath::from(plan_path_str.clone()),
                body,
                head,
            },
        )
        .await
        .map_err(map_lifecycle_err)?;

    // Auto-watch git logs/HEAD + feedback dir as part of plan
    // registration. Idempotent re-registers are no-ops.
    if let Err(e) =
        attach_companion_watchers(state, &session_id, &repo_root_canonical).await
    {
        tracing::warn!(session_id = session_id.as_str(), error = ?e, "register_plan_file: companion watcher setup partial failure");
    }

    let from_effects = outcome.apply.items.iter().find_map(|e| match e {
        AppliedEffect::Started {
            plan_id,
            plan_revision_id,
        } => Some((*plan_id, *plan_revision_id)),
        AppliedEffect::PlanRevision {
            plan_id,
            plan_revision_id,
            ..
        } => Some((*plan_id, *plan_revision_id)),
        _ => None,
    });
    let (plan_id, revision_id) = match from_effects {
        Some(v) => v,
        None => {
            let active = crate::storage::plans::load_active_plan(&state.pool, &session_id)
                .await
                .map_err(|e| match e {
                    crate::storage::plans::LoadActivePlanError::Sql(s) => {
                        ToolError::Internal(anyhow::anyhow!(s))
                    }
                    crate::storage::plans::LoadActivePlanError::Inconsistent(i) => {
                        ToolError::Internal(anyhow::anyhow!(i))
                    }
                })?;
            match active {
                Some(plan) => {
                    let latest =
                        crate::storage::plan_revisions::latest_for_plan(&state.pool, plan.id)
                            .await
                            .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
                    (plan.id, latest.map(|r| r.id).unwrap_or(0))
                }
                None => (0, 0),
            }
        }
    };

    Ok(json!({
        "session_id": session_id.as_str(),
        "plan_id": plan_id,
        "revision_id": revision_id,
        "noop": outcome.apply.items.is_empty(),
    }))
}

/// Attach the git-logs/HEAD watcher and create + watch the feedback
/// directory for this session. Called by `register_plan_file`.
/// Idempotent — repeated calls with the same paths are no-ops at the
/// watcher level. Failures are logged at warn but don't fail the tool
/// call (the plan-file watcher succeeding is the load-bearing piece).
async fn attach_companion_watchers(
    state: &AppState,
    session_id: &SessionId,
    repo_root: &Path,
) -> anyhow::Result<()> {
    let watcher = state.lifecycle.watcher();

    let git_logs_head = git::resolve_git_logs_head(repo_root).await?;
    if git_logs_head.exists() {
        watcher.watch_git_logs(session_id, &git_logs_head);
    } else {
        tracing::debug!(path = %git_logs_head.display(), "register_plan_file: .git/logs/HEAD doesn't exist yet; watcher will pick up on first commit");
    }

    let feedback_dir = repo_root
        .join(".trinity")
        .join("feedback")
        .join(session_id.as_str());
    tokio::fs::create_dir_all(&feedback_dir).await?;
    watcher.watch_feedback_dir(session_id, &feedback_dir);

    Ok(())
}

pub(crate) async fn upsert_seen_with_event(
    state: &AppState,
    session_id: &SessionId,
    label: &AgentLabel,
    now: i64,
) -> Result<(), ToolError> {
    let outcome = agents::upsert_seen(&state.pool, session_id, label, now)
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
    if outcome == SeenOutcome::Inserted {
        ev_store::append(
            &state.pool,
            session_id,
            None,
            None,
            None,
            EventKind::AgentJoined.as_str(),
            &format!("agent:{label}"),
            &json!({ "label": label.as_str() }),
            None,
            now,
        )
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
    }
    Ok(())
}

pub(crate) fn map_lifecycle_err(err: crate::daemon::LifecycleServiceError) -> ToolError {
    use crate::daemon::LifecycleServiceError as E;
    match err {
        E::Reducer(r) => ToolError::Invalid(format!("lifecycle reducer: {r}")),
        E::Apply(a) => ToolError::Internal(anyhow::anyhow!(a)),
        E::ActivePlan(inc) => ToolError::Internal(anyhow::anyhow!(inc)),
        E::NoSession(s) => ToolError::NotFound(format!("session `{s}` not found")),
        E::Sql(e) => ToolError::Internal(anyhow::anyhow!(e)),
    }
}

pub(crate) fn map_service_err(err: ServiceError) -> ToolError {
    match err {
        ServiceError::NoSession(s) => ToolError::NotFound(format!("session `{s}` not found")),
        ServiceError::NoActivePlanForFeedback(_)
        | ServiceError::FeedbackTargetNotInActivePlan { .. } => {
            ToolError::Forbidden(err.to_string())
        }
        ServiceError::EmptyFeedbackBody => ToolError::Invalid(err.to_string()),
        ServiceError::ActivePlan(inc) => ToolError::Internal(anyhow::anyhow!(inc)),
        ServiceError::Decode(d) => ToolError::Internal(anyhow::anyhow!(d)),
        ServiceError::Feedback(e) => ToolError::Internal(anyhow::anyhow!(e)),
        ServiceError::Sql(e) => ToolError::Internal(anyhow::anyhow!(e)),
    }
}

fn expand_home(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(s)
}
