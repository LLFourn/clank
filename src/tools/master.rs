//! Lifecycle MCP tools for plan registration and explicit repo claim.
//! Implementation commits arrive via the `.git/logs/HEAD` watcher;
//! feedback arrives via the feedback-directory watcher.

use std::path::PathBuf;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::daemon::AppState;
use crate::daemon::apply::AppliedEffect;
use crate::daemon::git;
use crate::daemon::internal_api::ToolCallRequest;
use crate::domain::EventKind;
use crate::lifecycle::{
    ActivationSource, AgentLabel, CommitSha, Observation, PlanFilePath, SessionId,
};
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

#[derive(Debug, Deserialize)]
struct ClaimSessionArgs {
    session_id: String,
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

    let mut took_reactivation_path = false;
    if let Some(session) = existing.as_ref() {
        if session.repo_root != repo_root_str {
            return Err(ToolError::Forbidden(format!(
                "session `{}` was created against repo `{}` but caller cwd resolves to `{}`",
                session_id, session.repo_root, repo_root_str
            )));
        }
        if session.archived_at.is_some() {
            took_reactivation_path = true;
            state
                .lifecycle
                .reactivate_session(
                    &session_id,
                    &repo_root_str,
                    &PlanFilePath::from(plan_path_str.clone()),
                    body.clone(),
                    head.clone(),
                    &format!("agent:{label}"),
                )
                .await
                .map_err(map_lifecycle_err)?;
        } else if session.active_plan_id.is_none() {
            let finished_exists: Option<i64> = sqlx::query_scalar(
                "SELECT id FROM plans WHERE session_id = ? AND state = 'finished' LIMIT 1",
            )
            .bind(session_id.as_str())
            .fetch_optional(&state.pool)
            .await
            .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
            if finished_exists.is_some() {
                return Err(ToolError::Invalid(format!(
                    "session `{}` has already finished; create a new session for new work",
                    session_id
                )));
            }
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

    // Reactivation already issued a `PlanRegistered` observation against
    // the fresh active plan; the fallback below loads the new plan via
    // `load_active_plan` rather than re-issuing the observation.
    let outcome = if took_reactivation_path {
        None
    } else {
        Some(
            state
                .lifecycle
                .observe(
                    &session_id,
                    &format!("agent:{label}"),
                    Observation::PlanRegistered {
                        path: PlanFilePath::from(plan_path_str.clone()),
                        body,
                        head,
                        source: ActivationSource::ExplicitRegister,
                    },
                )
                .await
                .map_err(map_lifecycle_err)?,
        )
    };

    let from_effects = outcome.as_ref().and_then(|o| {
        o.apply.items.iter().find_map(|e| match e {
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
        })
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
        "noop": outcome.as_ref().is_some_and(|o| o.apply.items.is_empty()),
    }))
}

pub async fn claim_session(state: &AppState, req: &ToolCallRequest) -> Result<Value, ToolError> {
    let args: ClaimSessionArgs = serde_json::from_value(req.arguments.clone())
        .map_err(|e| ToolError::Invalid(format!("invalid args: {e}")))?;
    let session_id = sessions::validate_slug(&args.session_id).map_err(ToolError::Invalid)?;
    let label = AgentLabel::from(args.label.trim().to_string());
    if label.as_str().is_empty() {
        return Err(ToolError::Invalid("label must be non-empty".into()));
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

    let session = sessions::fetch(&state.pool, &session_id)
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?
        .ok_or_else(|| ToolError::NotFound(format!("session `{session_id}` not found")))?;
    if session.repo_root != repo_root_str {
        return Err(ToolError::Forbidden(format!(
            "session `{}` is recorded under repo `{}` but caller cwd resolves to `{}`",
            session_id, session.repo_root, repo_root_str
        )));
    }

    let now = chrono::Utc::now().timestamp();
    upsert_seen_with_event(state, &session_id, &label, now).await?;
    state
        .lifecycle
        .claim_repo_effective_session(&session_id, &format!("agent:{label}"))
        .await
        .map_err(map_lifecycle_err)?;

    Ok(json!({
        "session_id": session_id.as_str(),
        "repo_root": repo_root_str,
        "is_repo_effective": true,
    }))
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
        E::Feedback(e) => ToolError::Internal(anyhow::anyhow!(e)),
        E::NoSession(s) => ToolError::NotFound(format!("session `{s}` not found")),
        E::Sql(e) => ToolError::Internal(anyhow::anyhow!(e)),
        E::InvalidArgs(msg) => ToolError::Invalid(msg),
        E::Watcher(err) => ToolError::Internal(err),
        E::RepoMismatch {
            session_id,
            expected,
            actual,
        } => ToolError::Invalid(format!(
            "session `{session_id}` is recorded under repo `{expected}`; cannot re-register under `{actual}`"
        )),
        E::SessionArchived(s) => {
            ToolError::Invalid(format!("session `{s}` is archived; re-register first"))
        }
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
