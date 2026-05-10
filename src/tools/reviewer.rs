use std::path::PathBuf;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::daemon::AppState;
use crate::daemon::git;
use crate::daemon::internal_api::ToolCallRequest;
use crate::domain::{EventKind, FeedbackStatus, TargetKind};
use crate::storage::{
    agents, events as ev_store, implementation_revisions as impl_revs, plan_revisions, plans,
};

use super::ToolError;

#[derive(Debug, Deserialize)]
struct JoinPlanArgs {
    plan_id_or_path: String,
    label: String,
}

#[derive(Debug, Deserialize)]
struct GetReviewContextArgs {
    plan_id: String,
    #[serde(default)]
    target: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AddFeedbackArgs {
    plan_id: String,
    target_kind: String,
    target_id: String,
    text: String,
}

pub async fn list_plans(state: &AppState, req: &ToolCallRequest) -> Result<Value, ToolError> {
    let scope = match git::rev_parse_show_toplevel(&req.cwd).await {
        Ok(root) => match dunce::canonicalize(&root) {
            Ok(canonical) => Some(canonical.to_string_lossy().into_owned()),
            Err(_) => None,
        },
        Err(_) => None,
    };

    let candidates = match scope.as_deref() {
        Some(repo_root) => plans::list_by_repo_root(&state.pool, repo_root)
            .await
            .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?,
        None => plans::list_active(&state.pool)
            .await
            .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?,
    };

    let mut rows = Vec::with_capacity(candidates.len());
    for plan in candidates {
        let agents_for_plan = agents::list_for_plan(&state.pool, &plan.id)
            .await
            .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
        let master_label = agents_for_plan
            .iter()
            .find(|a| a.role == "master")
            .map(|a| a.label.clone());
        let reviewer_labels: Vec<String> = agents_for_plan
            .iter()
            .filter(|a| a.role == "reviewer")
            .map(|a| a.label.clone())
            .collect();
        let latest_plan_revision_at = plan_revisions::latest(&state.pool, &plan.id)
            .await
            .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?
            .map(|r| r.created_at);
        let pending_plan_count = count_pending(state, &plan.id, "plan_revision").await?;
        let pending_impl_count = count_pending(state, &plan.id, "implementation_commit").await?;

        rows.push(json!({
            "plan_id": plan.id,
            "display_title": plan.display_title,
            "plan_path": plan.plan_path,
            "repo_root": plan.repo_root,
            "state": plan.state,
            "master_label": master_label,
            "joined_reviewer_labels": reviewer_labels,
            "latest_plan_revision_at": latest_plan_revision_at,
            "pending_plan_count": pending_plan_count,
            "pending_impl_count": pending_impl_count,
        }));
    }
    Ok(json!({
        "scope_repo_root": scope,
        "plans": rows,
    }))
}

pub async fn join_plan(state: &AppState, req: &ToolCallRequest) -> Result<Value, ToolError> {
    let args: JoinPlanArgs = serde_json::from_value(req.arguments.clone())
        .map_err(|e| ToolError::Invalid(format!("invalid args: {e}")))?;
    if args.label.trim().is_empty() {
        return Err(ToolError::Invalid("label must be non-empty".into()));
    }

    let plan = resolve_plan_or_path(state, &req.cwd, &args.plan_id_or_path).await?;

    let now = chrono::Utc::now().timestamp();

    // Reject if label already in use on this plan with a non-reviewer role.
    if let Some(existing) = agents::fetch_by_label(&state.pool, &plan.id, &args.label)
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?
    {
        if existing.role != "reviewer" {
            return Err(ToolError::Forbidden(format!(
                "label `{}` is already a {} on this plan",
                args.label, existing.role
            )));
        }
        // Same-label re-join: just touch last_seen and succeed.
        let mut tx = state
            .pool
            .begin()
            .await
            .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
        agents::touch_last_seen(&mut *tx, existing.id, now)
            .await
            .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
        tx.commit()
            .await
            .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
        return Ok(json!({"plan_id": plan.id, "state": plan.state}));
    }

    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
    let _agent_id = agents::insert(&mut *tx, &plan.id, agents::Role::Reviewer, &args.label, now)
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
    let actor = format!("reviewer:{}", args.label);
    let payload = json!({"role": "reviewer", "label": args.label});
    ev_store::append(
        &mut *tx,
        &ev_store::NewEvent::note(&plan.id, EventKind::AgentJoined, &actor, &payload, now),
    )
    .await
    .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
    plans::touch_updated_at(&mut *tx, &plan.id, now)
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
    tx.commit()
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;

    Ok(json!({"plan_id": plan.id, "state": plan.state}))
}

pub async fn get_review_context(
    state: &AppState,
    req: &ToolCallRequest,
) -> Result<Value, ToolError> {
    let args: GetReviewContextArgs = serde_json::from_value(req.arguments.clone())
        .map_err(|e| ToolError::Invalid(format!("invalid args: {e}")))?;
    let label = require_label(req)?;
    let plan = require_plan(state, &args.plan_id).await?;
    require_bound_agent(state, &plan.id, &label).await?;

    let agents_for_plan = agents::list_for_plan(&state.pool, &plan.id)
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
    let joined_agents: Vec<Value> = agents_for_plan
        .iter()
        .map(|a| json!({"role": a.role, "label": a.label}))
        .collect();

    // If target is omitted, pick the active artifact based on plan state.
    let resolved_target: &str = match args.target.as_deref() {
        Some(s) => s,
        None => {
            let plan_state = crate::domain::WorkState::parse(&plan.state).ok_or_else(|| {
                ToolError::Internal(anyhow::anyhow!("plan has invalid state: {}", plan.state))
            })?;
            match plan_state.active_artifact() {
                TargetKind::PlanRevision => "plan",
                TargetKind::ImplementationCommit => "implementation",
            }
        }
    };

    match Some(resolved_target) {
        Some("plan") => {
            let latest = plan_revisions::latest(&state.pool, &plan.id)
                .await
                .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
            let latest_revision = latest.map(|r| {
                json!({
                    "revision_id": r.id,
                    "revision_number": r.revision_number,
                    "content_hash": r.content_hash,
                    "body": r.body,
                    "created_at": r.created_at,
                    "detected_by": r.detected_by,
                })
            });
            Ok(json!({
                "state": plan.state,
                "plan_id": plan.id,
                "joined_agents": joined_agents,
                "latest_plan_revision": latest_revision,
            }))
        }
        Some("implementation") => {
            let latest = impl_revs::latest(&state.pool, &plan.id)
                .await
                .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
            let revision_payload = match latest {
                None => Value::Null,
                Some(rev) => {
                    let repo = std::path::Path::new(&plan.repo_root).to_path_buf();
                    let diff = match crate::daemon::git::diff_text(
                        &repo,
                        rev.parent_sha.as_deref(),
                        &rev.commit_sha,
                    )
                    .await
                    {
                        Ok(d) => d,
                        Err(e) => format!("(failed to compute diff: {e})"),
                    };
                    let truncated = diff.len() > 200_000;
                    let diff_text = if truncated {
                        let mut end = 200_000.min(diff.len());
                        while end > 0 && !diff.is_char_boundary(end) {
                            end -= 1;
                        }
                        format!(
                            "{}\n... (truncated, full diff is {} bytes; ask the curator to fetch via web UI)",
                            &diff[..end],
                            diff.len()
                        )
                    } else {
                        diff
                    };
                    json!({
                        "implementation_revision_id": rev.id,
                        "commit_sha": rev.commit_sha,
                        "short_sha": crate::daemon::git::short_sha(&rev.commit_sha),
                        "parent_sha": rev.parent_sha,
                        "branch": rev.branch,
                        "commit_message": rev.commit_message,
                        "diff_stat": rev.diff_stat,
                        "diff_text": diff_text,
                        "diff_truncated": truncated,
                        "worktree_status": rev.worktree_status,
                        "is_head": rev.is_head != 0,
                        "registered_by": rev.registered_by,
                        "created_at": rev.created_at,
                    })
                }
            };
            Ok(json!({
                "state": plan.state,
                "plan_id": plan.id,
                "joined_agents": joined_agents,
                "latest_implementation_revision": revision_payload,
            }))
        }
        Some(other) => Err(ToolError::Invalid(format!(
            "target must be 'plan' or 'implementation', got '{other}'"
        ))),
        None => unreachable!(),
    }
}

pub async fn add_feedback(state: &AppState, req: &ToolCallRequest) -> Result<Value, ToolError> {
    let args: AddFeedbackArgs = serde_json::from_value(req.arguments.clone())
        .map_err(|e| ToolError::Invalid(format!("invalid args: {e}")))?;
    if args.text.trim().is_empty() {
        return Err(ToolError::Invalid("text must be non-empty".into()));
    }
    let label = require_label(req)?;
    let plan = require_plan(state, &args.plan_id).await?;

    let agent = agents::fetch_by_label(&state.pool, &plan.id, &label)
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?
        .ok_or_else(|| {
            ToolError::Forbidden(format!(
                "no agent `{}` joined to plan; call register_plan_file or join_plan first",
                label
            ))
        })?;
    if agent.role != "reviewer" {
        return Err(ToolError::Forbidden(format!(
            "label `{}` joined as {} — only reviewers can post feedback",
            label, agent.role
        )));
    }

    // Validate target.
    let kind = TargetKind::parse(&args.target_kind).ok_or_else(|| {
        ToolError::Invalid(format!(
            "target_kind must be 'plan_revision' or 'implementation_commit', got '{}'",
            args.target_kind
        ))
    })?;
    match kind.as_str() {
        "plan_revision" => {
            let id: i64 = args.target_id.parse().map_err(|_| {
                ToolError::Invalid("target_id for plan_revision must be a numeric id".into())
            })?;
            let exists: Option<(i64,)> =
                sqlx::query_as("SELECT id FROM plan_revisions WHERE plan_id = ? AND id = ?")
                    .bind(&plan.id)
                    .bind(id)
                    .fetch_optional(&state.pool)
                    .await
                    .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
            if exists.is_none() {
                return Err(ToolError::NotFound(format!(
                    "plan_revision {id} does not belong to plan {}",
                    plan.id
                )));
            }
        }
        "implementation_commit" => {
            let exists: Option<(i64,)> = sqlx::query_as(
                "SELECT id FROM implementation_revisions WHERE plan_id = ? AND commit_sha = ?",
            )
            .bind(&plan.id)
            .bind(&args.target_id)
            .fetch_optional(&state.pool)
            .await
            .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
            if exists.is_none() {
                return Err(ToolError::NotFound(format!(
                    "implementation commit {} not registered on plan {}",
                    args.target_id, plan.id
                )));
            }
        }
        _ => unreachable!("TargetKind::parse handled above"),
    }

    let now = chrono::Utc::now().timestamp();
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
    let actor = format!("reviewer:{}", label);
    let payload = json!({"text": args.text});
    let event_id = ev_store::append(
        &mut *tx,
        &ev_store::NewEvent {
            plan_id: &plan.id,
            kind: EventKind::FeedbackAdded,
            actor: &actor,
            payload: &payload,
            ts: now,
            target: Some(ev_store::EventTarget {
                kind,
                id: &args.target_id,
            }),
            status: Some(FeedbackStatus::Pending),
        },
    )
    .await
    .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
    agents::touch_last_seen(&mut *tx, agent.id, now)
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
    plans::touch_updated_at(&mut *tx, &plan.id, now)
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
    tx.commit()
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;

    Ok(json!({"event_id": event_id, "status": "pending"}))
}

async fn count_pending(
    state: &AppState,
    plan_id: &str,
    target_kind: &str,
) -> Result<i64, ToolError> {
    let row: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM events WHERE plan_id = ? AND kind = 'feedback_added' \
         AND target_kind = ? AND status IN ('pending', 'staged')",
    )
    .bind(plan_id)
    .bind(target_kind)
    .fetch_one(&state.pool)
    .await
    .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
    Ok(row.0)
}

fn require_label(req: &ToolCallRequest) -> Result<String, ToolError> {
    req.label
        .clone()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            ToolError::Forbidden(
            "this tool requires you to first call register_plan_file or join_plan to bind a label"
                .into(),
        )
        })
}

async fn require_plan(state: &AppState, plan_id: &str) -> Result<plans::Plan, ToolError> {
    plans::fetch(&state.pool, plan_id)
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?
        .ok_or_else(|| ToolError::NotFound(format!("plan {plan_id} not found")))
}

async fn require_bound_agent(
    state: &AppState,
    plan_id: &str,
    label: &str,
) -> Result<agents::Agent, ToolError> {
    let agent = agents::fetch_by_label(&state.pool, plan_id, label)
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?
        .ok_or_else(|| {
            ToolError::Forbidden(format!(
                "no agent `{label}` joined to plan {plan_id}; call register_plan_file or join_plan first"
            ))
        })?;
    let now = chrono::Utc::now().timestamp();
    let _ = agents::touch_last_seen(&state.pool, agent.id, now).await;
    Ok(agent)
}

async fn resolve_plan_or_path(
    state: &AppState,
    cwd: &std::path::Path,
    spec: &str,
) -> Result<plans::Plan, ToolError> {
    // 64-char hex → plan id.
    let trimmed = spec.trim();
    if trimmed.len() == 64 && trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
        return plans::fetch(&state.pool, trimmed)
            .await
            .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?
            .ok_or_else(|| ToolError::NotFound(format!("plan {trimmed} not found")));
    }

    // Otherwise treat as a path; resolve via canonicalize + repo root from cwd.
    let raw = expand_home(trimmed);
    let abs = if raw.is_absolute() {
        raw
    } else {
        cwd.join(raw)
    };
    let canonical = dunce::canonicalize(&abs).map_err(|e| {
        ToolError::Invalid(format!(
            "plan path does not exist or is unreadable ({}): {e}",
            abs.display()
        ))
    })?;
    let repo_root = git::rev_parse_show_toplevel(cwd)
        .await
        .map_err(|e| ToolError::Invalid(format!("cwd is not in a git repo: {e}")))?;
    let repo_canonical = dunce::canonicalize(&repo_root)
        .map_err(|e| ToolError::Internal(anyhow::anyhow!("canonicalize repo_root: {e}")))?;
    let plan_id = plans::compute_plan_id(&repo_canonical, &canonical);
    plans::fetch(&state.pool, &plan_id)
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?
        .ok_or_else(|| {
            ToolError::NotFound(format!(
                "no plan registered for path {} in repo {}",
                canonical.display(),
                repo_canonical.display()
            ))
        })
}

fn expand_home(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(s)
}
