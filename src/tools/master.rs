//! Lifecycle MCP tools: register_plan_file, register_implementation_commit,
//! get_current_feedback. All take `session_id` as a caller-chosen slug.
//!
//! There is no master/reviewer role; any caller may invoke any tool. `label`
//! is for attribution only and is recorded on the emitted event + upserted
//! into `agents.last_seen`.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::daemon::AppState;
use crate::daemon::apply::AppliedEffect;
use crate::daemon::git;
use crate::daemon::internal_api::ToolCallRequest;
use crate::daemon::{CurrentFeedbackView, ServiceError};
use crate::domain::{EventKind, FeedbackTargetRef, TargetKind};
use crate::lifecycle::{AgentLabel, CommitSha, CommitSnapshot, Observation, PlanFilePath};
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
struct RegisterImplementationCommitArgs {
    session_id: String,
    commit_sha: String,
    #[serde(default)]
    force: bool,
    #[serde(default)]
    label: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GetCurrentFeedbackArgs {
    session_id: String,
    #[serde(default)]
    target_kind: Option<String>,
    #[serde(default)]
    label: Option<String>,
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

    let existing = sessions::fetch(&state.pool, &session_id)
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;

    // First-sight session insert (initial plan_file_path + repo_root). All
    // subsequent path updates flow through SessionService::observe so the
    // path write, reducer effects, and watcher switch stay consistent under
    // failure.
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

    // Feed the lifecycle observation. This handles path-update inside the
    // same transaction as the reducer's effects + watcher switch.
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

    // On a same-body no-op the reducer emits no effects; fall back to the
    // current active plan's (plan_id, latest revision_id) so idempotent
    // re-registration returns the actual identifiers.
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

pub async fn register_implementation_commit(
    state: &AppState,
    req: &ToolCallRequest,
) -> Result<Value, ToolError> {
    let args: RegisterImplementationCommitArgs = serde_json::from_value(req.arguments.clone())
        .map_err(|e| ToolError::Invalid(format!("invalid args: {e}")))?;
    let session_id = sessions::validate_slug(&args.session_id).map_err(ToolError::Invalid)?;

    let label_opt = args
        .label
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| AgentLabel::from(s.to_string()));

    // Validate session existence BEFORE upserting the agent row — otherwise
    // an unknown session_id surfaces as a 500 (FK violation) instead of the
    // clean 404 we want, and a non-existent session ends up with a phantom
    // agents row that the user never wrote to.
    let session = sessions::fetch(&state.pool, &session_id)
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?
        .ok_or_else(|| ToolError::NotFound(format!("session {session_id} not found")))?;
    let repo = Path::new(&session.repo_root).to_path_buf();

    let now = chrono::Utc::now().timestamp();
    if let Some(label) = label_opt.as_ref() {
        upsert_seen_with_event(state, &session_id, label, now).await?;
    }

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

    let actor = match label_opt.as_ref() {
        Some(l) => format!("agent:{l}"),
        None => "external".to_string(),
    };

    let outcome = state
        .lifecycle
        .observe(&session_id, &actor, Observation::CommitObserved { commit })
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

    let filter = match args.target_kind.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(s) => Some(
            TargetKind::parse(s)
                .ok_or_else(|| ToolError::Invalid(format!("unknown target_kind: {s}")))?,
        ),
    };

    // Call the service FIRST so an unknown session_id returns the clean
    // NotFound from current_feedback rather than an FK-violation 500 from
    // the agents upsert below. The shim's per-tool autofill means `label`
    // is frequently present even when the caller didn't think about it,
    // so this ordering matters for ergonomic typos / cross-session polls.
    let view = state
        .lifecycle
        .current_feedback(&session_id, filter)
        .await
        .map_err(map_service_err)?;

    if let Some(label) = args
        .label
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let label = AgentLabel::from(label.to_string());
        let now = chrono::Utc::now().timestamp();
        upsert_seen_with_event(state, &session_id, &label, now).await?;
    }

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

/// Upsert `agents.last_seen` for `(session_id, label)`. On the first
/// insert ever, emit one `agent_joined` audit event. Used by both write
/// tools (`register_plan_file`, `register_implementation_commit`,
/// `put_feedback`) and read tools (`get_current_feedback`,
/// `get_review_context`). The plan invariant is "exactly once per
/// `(session_id, label)`", which means even first-seen-via-read counts.
///
/// **This helper never touches `sessions.updated_at`.** Write tools
/// already bump `updated_at` elsewhere (via the apply layer or their own
/// explicit calls); the read path must not, so polling doesn't churn the
/// session list / future SSE timeline.
pub(crate) async fn upsert_seen_with_event(
    state: &AppState,
    session_id: &crate::lifecycle::SessionId,
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
