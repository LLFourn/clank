//! Master-agent MCP tools: register_plan_file, register_implementation_commit,
//! get_current_feedback. All take `session_id` as a master-supplied slug.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::daemon::AppState;
use crate::daemon::apply::AppliedEffect;
use crate::daemon::git;
use crate::daemon::internal_api::ToolCallRequest;
use crate::daemon::{CurrentFeedbackView, ServiceError};
use crate::domain::{FeedbackTargetRef, TargetKind};
use crate::lifecycle::{
    AgentLabel, CommitSha, CommitSnapshot, Observation, PlanFilePath, SessionId,
};
use crate::storage::{agents, events as ev_store, sessions};

use super::ToolError;

const MASTER_STALE_SECONDS: i64 = 300;

#[derive(Debug, Deserialize)]
struct RegisterPlanFileArgs {
    session_id: String,
    path: String,
    label: String,
}

#[derive(Debug, Deserialize)]
struct RegisterImplementationCommitArgs {
    session_id: String,
    commit_sha: String,
    #[serde(default)]
    force: bool,
}

#[derive(Debug, Deserialize)]
struct GetCurrentFeedbackArgs {
    session_id: String,
    #[serde(default)]
    target_kind: Option<String>,
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

    // Resolve plan-file path against caller cwd if relative, canonicalize.
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

    // Derive repo_root from caller cwd.
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

    // Session row + master claim. Pure DB metadata; the lifecycle reducer
    // does the body/head decision separately via observe().
    let existing = sessions::fetch(&state.pool, &session_id)
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
    let display_title = canonical
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("plan")
        .to_string();

    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
    let agent_id = match existing.as_ref() {
        None => {
            sessions::insert(
                &mut *tx,
                &session_id,
                &repo_root_str,
                &PlanFilePath::from(plan_path_str.clone()),
                Some(&display_title),
                now,
            )
            .await
            .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
            let new_id = agents::insert(&mut *tx, &session_id, agents::Role::Master, &label, now)
                .await
                .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
            ev_store::append(
                &mut *tx,
                &session_id,
                None,
                None,
                None,
                crate::domain::EventKind::AgentJoined.as_str(),
                &format!("master:{label}"),
                &json!({"role": "master", "label": label.as_str()}),
                None,
                now,
            )
            .await
            .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
            sessions::set_master_agent(&mut *tx, &session_id, Some(new_id), now)
                .await
                .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
            new_id
        }
        Some(session) => {
            // Master claim: same label always reclaims; different label only if stale.
            if let Some(master) = agents::fetch_master(&state.pool, &session_id)
                .await
                .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?
            {
                let stale = now - master.last_seen >= MASTER_STALE_SECONDS;
                if master.label != label.as_str() && !stale {
                    return Err(ToolError::Forbidden(format!(
                        "session `{}` already has live master `{}` (last seen {}s ago); evict via UI or wait {}s",
                        session_id,
                        master.label,
                        now - master.last_seen,
                        MASTER_STALE_SECONDS - (now - master.last_seen)
                    )));
                }
                if master.label == label.as_str() {
                    agents::touch_last_seen(&mut *tx, master.id, now)
                        .await
                        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
                    master.id
                } else {
                    // Stale master: evict and replace.
                    agents::delete(&mut *tx, master.id)
                        .await
                        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
                    let new_id =
                        agents::insert(&mut *tx, &session_id, agents::Role::Master, &label, now)
                            .await
                            .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
                    sessions::set_master_agent(&mut *tx, &session_id, Some(new_id), now)
                        .await
                        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
                    new_id
                }
            } else {
                // No master yet — claim it.
                let new_id =
                    agents::insert(&mut *tx, &session_id, agents::Role::Master, &label, now)
                        .await
                        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
                sessions::set_master_agent(&mut *tx, &session_id, Some(new_id), now)
                    .await
                    .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
                new_id
            };
            // Repo_root must match across re-registrations.
            if session.repo_root != repo_root_str {
                return Err(ToolError::Forbidden(format!(
                    "session `{}` was created against repo `{}` but caller cwd resolves to `{}`",
                    session_id, session.repo_root, repo_root_str
                )));
            }
            // sessions.plan_file_path is updated atomically inside observe() below.
            // Touch updated_at to reflect the master ping.
            sessions::touch_updated_at(&mut *tx, &session_id, now)
                .await
                .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
            agents::fetch_by_label(&state.pool, &session_id, &label)
                .await
                .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?
                .map(|a| a.id)
                .unwrap_or(0)
        }
    };
    tx.commit()
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
    let _ = agent_id; // currently used only via FK; reserved for future need.

    // Feed the lifecycle observation. This handles path-update inside the
    // same transaction as the reducer's effects + watcher switch.
    let outcome = state
        .lifecycle
        .observe(
            &session_id,
            &format!("master:{label}"),
            Observation::PlanRegistered {
                path: PlanFilePath::from(plan_path_str.clone()),
                body,
                head,
            },
        )
        .await
        .map_err(map_lifecycle_err)?;

    // The first effect (if any) tells the caller which plan/revision was started or extended.
    // On a same-body no-op the reducer emits no effects; we fall back to the
    // current active plan's (plan_id, latest revision_id) so an idempotent
    // re-registration returns the actual identifiers rather than sentinels.
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
            // Same-body no-op: look up the current active plan's latest revision.
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

pub async fn register_implementation_commit(
    state: &AppState,
    req: &ToolCallRequest,
) -> Result<Value, ToolError> {
    let args: RegisterImplementationCommitArgs = serde_json::from_value(req.arguments.clone())
        .map_err(|e| ToolError::Invalid(format!("invalid args: {e}")))?;
    let session_id = sessions::validate_slug(&args.session_id).map_err(ToolError::Invalid)?;
    let label = require_label(req)?;
    require_master(state, &session_id, &label).await?;

    let session = sessions::fetch(&state.pool, &session_id)
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?
        .ok_or_else(|| ToolError::NotFound(format!("session {session_id} not found")))?;
    let repo = Path::new(&session.repo_root).to_path_buf();

    let full_sha = git::resolve_to_full_sha(&repo, &args.commit_sha)
        .await
        .map_err(|e| ToolError::Invalid(format!("cannot resolve {}: {e}", args.commit_sha)))?;
    let head_sha = git::rev_parse_head(&repo)
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
    let is_head = full_sha == head_sha;
    if !is_head && !args.force {
        return Err(ToolError::Invalid(format!(
            "commit {} is not HEAD ({}); pass force=true to register an older commit",
            git::short_sha(&full_sha),
            git::short_sha(&head_sha)
        )));
    }

    let parent = git::parent_sha(&repo, &full_sha)
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
    let branch = git::current_branch(&repo)
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
    let message = git::commit_message(&repo, &full_sha)
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
    let stat = git::diff_stat(&repo, parent.as_deref(), &full_sha)
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
    let porcelain = git::worktree_porcelain(&repo)
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
    let dirty = !porcelain.trim().is_empty();

    let commit = CommitSnapshot {
        sha: CommitSha::from(full_sha.clone()),
        parent_sha: parent.map(CommitSha::from),
        branch,
        message,
        diff_stat: stat,
        worktree_status: Some(if dirty {
            porcelain
        } else {
            "clean".to_string()
        }),
        is_head,
    };

    let outcome = state
        .lifecycle
        .observe(
            &session_id,
            &format!("master:{label}"),
            Observation::CommitObserved { commit },
        )
        .await
        .map_err(map_lifecycle_err)?;

    let impl_revision_id = outcome.apply.items.iter().find_map(|e| match e {
        AppliedEffect::Implementation {
            implementation_revision_id,
            ..
        } => Some(*implementation_revision_id),
        _ => None,
    });
    Ok(json!({
        "session_id": session_id.as_str(),
        "commit_sha": full_sha,
        "is_head": is_head,
        "dirty_worktree": dirty,
        "implementation_revision_id": impl_revision_id,
        "noop": impl_revision_id.is_none(),
    }))
}

pub async fn get_current_feedback(
    state: &AppState,
    req: &ToolCallRequest,
) -> Result<Value, ToolError> {
    let args: GetCurrentFeedbackArgs = serde_json::from_value(req.arguments.clone())
        .map_err(|e| ToolError::Invalid(format!("invalid args: {e}")))?;
    let session_id = sessions::validate_slug(&args.session_id).map_err(ToolError::Invalid)?;
    let label = require_label(req)?;
    require_master(state, &session_id, &label).await?;

    let filter = match args.target_kind.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(s) => Some(
            TargetKind::parse(s)
                .ok_or_else(|| ToolError::Invalid(format!("unknown target_kind: {s}")))?,
        ),
    };

    let view = state
        .lifecycle
        .current_feedback(&session_id, filter)
        .await
        .map_err(map_service_err)?;

    Ok(match view {
        CurrentFeedbackView::NoActivePlan { session_id } => json!({
            "session_id": session_id.as_str(),
            "state": "no_active_plan",
        }),
        CurrentFeedbackView::Active {
            session_id,
            plan_id,
            filter,
            feedback_digest,
            feedback,
        } => {
            let items: Vec<Value> = feedback
                .into_iter()
                .map(|r| {
                    let (target_kind, target_id) = match &r.target {
                        FeedbackTargetRef::PlanRevision(id) => ("plan_revision", id.to_string()),
                        FeedbackTargetRef::ImplementationCommit(sha) => {
                            ("implementation_commit", sha.as_str().to_string())
                        }
                    };
                    json!({
                        "feedback_id": r.id,
                        "plan_id": r.plan_id,
                        "target_kind": target_kind,
                        "target_id": target_id,
                        "author_label": r.author_label.as_str(),
                        "body": r.body,
                        "created_at": r.created_at,
                        "updated_at": r.updated_at,
                    })
                })
                .collect();
            json!({
                "session_id": session_id.as_str(),
                "state": "active",
                "plan_id": plan_id,
                "filter": filter.map(|k| k.as_str()),
                "feedback_digest": feedback_digest,
                "feedback": items,
            })
        }
    })
}

// ------------- helpers -------------

pub(crate) fn require_label(req: &ToolCallRequest) -> Result<AgentLabel, ToolError> {
    req.label
        .clone()
        .filter(|s| !s.trim().is_empty())
        .map(AgentLabel::from)
        .ok_or_else(|| {
            ToolError::Forbidden(
                "this tool requires a label; call register_plan_file or join_session first".into(),
            )
        })
}

pub(crate) async fn require_master(
    state: &AppState,
    session_id: &SessionId,
    label: &AgentLabel,
) -> Result<agents::Agent, ToolError> {
    let agent = agents::fetch_by_label(&state.pool, session_id, label)
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?
        .ok_or_else(|| {
            ToolError::Forbidden(format!(
                "no agent `{label}` on session `{session_id}`; call register_plan_file first"
            ))
        })?;
    if agent.role != "master" {
        return Err(ToolError::Forbidden(format!(
            "label `{label}` joined as {} on session `{session_id}`; only the master can run this tool",
            agent.role
        )));
    }
    let now = chrono::Utc::now().timestamp();
    let _ = agents::touch_last_seen(&state.pool, agent.id, now).await;
    Ok(agent)
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
