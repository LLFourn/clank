//! Feedback + read-context MCP tools: list_sessions, get_review_context,
//! put_feedback.
//!
//! There is no `join_session` and no master/reviewer role check. `put_feedback`
//! takes `author_label` as a required argument; other tools take an optional
//! `label` for `agents.last_seen` attribution.

use serde::Deserialize;
use serde_json::{Value, json};

use crate::daemon::AppState;
use crate::daemon::git;
use crate::daemon::internal_api::ToolCallRequest;
use crate::domain::{FeedbackTargetRef, TargetKind};
use crate::lifecycle::{AgentLabel, CommitSha, SessionId};
use crate::storage::{
    agents, feedback as feedback_store, implementation_revisions as impl_revs, plan_revisions,
    plans, sessions,
};

use super::ToolError;
use super::master::{map_service_err, upsert_seen_with_event};

#[derive(Debug, Deserialize)]
struct GetReviewContextArgs {
    session_id: String,
    #[serde(default)]
    target: Option<String>,
    #[serde(default)]
    label: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PutFeedbackArgs {
    session_id: String,
    target_kind: String,
    target_id: String,
    body: String,
    author_label: String,
}

pub async fn list_sessions(state: &AppState, req: &ToolCallRequest) -> Result<Value, ToolError> {
    let scope = match git::rev_parse_show_toplevel(&req.cwd).await {
        Ok(p) => dunce::canonicalize(&p)
            .ok()
            .map(|c| c.to_string_lossy().into_owned()),
        Err(_) => None,
    };

    let rows = match scope.as_deref() {
        Some(repo_root) => sessions::list_by_repo_root(&state.pool, repo_root)
            .await
            .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?,
        None => sessions::list_active(&state.pool)
            .await
            .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?,
    };

    let mut out = Vec::with_capacity(rows.len());
    for s in rows {
        let sid = SessionId::from(s.id.clone());
        let agents_for = agents::list_for_session(&state.pool, &sid)
            .await
            .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
        let recent_agents: Vec<String> = agents_for.iter().map(|a| a.label.clone()).collect();
        let (plan_fb, impl_fb) = active_feedback_counts(state, &sid).await?;
        out.push(json!({
            "session_id": s.id,
            "display_title": s.display_title,
            "plan_file_path": s.plan_file_path,
            "repo_root": s.repo_root,
            "has_active_plan": s.active_plan_id.is_some(),
            "active_plan_id": s.active_plan_id,
            "recent_agents": recent_agents,
            "updated_at": s.updated_at,
            "plan_feedback_count": plan_fb,
            "impl_feedback_count": impl_fb,
        }));
    }
    Ok(json!({"scope_repo_root": scope, "sessions": out}))
}

pub async fn get_review_context(
    state: &AppState,
    req: &ToolCallRequest,
) -> Result<Value, ToolError> {
    let args: GetReviewContextArgs = serde_json::from_value(req.arguments.clone())
        .map_err(|e| ToolError::Invalid(format!("invalid args: {e}")))?;
    let session_id = sessions::validate_slug(&args.session_id).map_err(ToolError::Invalid)?;

    // Verify the session exists BEFORE upserting agents — otherwise an
    // unknown session_id (typo / cross-session poll with cache-filled
    // label) hits the agents.session_id FK and returns 500 instead of the
    // intended 404.
    let session = sessions::fetch(&state.pool, &session_id)
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?
        .ok_or_else(|| ToolError::NotFound(format!("session `{session_id}` not found")))?;

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

    let agents_for = agents::list_for_session(&state.pool, &session_id)
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
    let joined_agents: Vec<Value> = agents_for
        .iter()
        .map(|a| json!({ "label": a.label, "last_seen": a.last_seen }))
        .collect();

    let active_plan = match plans::load_active_plan(&state.pool, &session_id).await {
        Ok(p) => p,
        Err(plans::LoadActivePlanError::Sql(e)) => {
            return Err(ToolError::Internal(anyhow::anyhow!(e)));
        }
        Err(plans::LoadActivePlanError::Inconsistent(inc)) => {
            return Err(ToolError::Internal(anyhow::anyhow!(inc)));
        }
    };
    let Some(active) = active_plan else {
        return Ok(json!({
            "session_id": session_id.as_str(),
            "has_active_plan": false,
            "joined_agents": joined_agents,
        }));
    };

    let resolved_target: &str = match args.target.as_deref() {
        Some(s) => s,
        None => {
            if active.state == "implementing" {
                "implementation"
            } else {
                "plan"
            }
        }
    };

    match resolved_target {
        "plan" => {
            let latest = plan_revisions::latest_for_plan(&state.pool, active.id)
                .await
                .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
            Ok(json!({
                "session_id": session_id.as_str(),
                "has_active_plan": true,
                "plan_id": active.id,
                "plan_state": active.state,
                "base_commit": active.base_commit,
                "joined_agents": joined_agents,
                "latest_plan_revision": latest.map(|r| json!({
                    "revision_id": r.id,
                    "revision_number": r.revision_number,
                    "content_hash": r.content_hash,
                    "body": r.body,
                    "created_at": r.created_at,
                })),
            }))
        }
        "implementation" => {
            let latest = impl_revs::latest_for_plan(&state.pool, active.id)
                .await
                .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
            let revision_payload =
                match latest {
                    None => Value::Null,
                    Some(rev) => {
                        let repo = std::path::Path::new(&session.repo_root);
                        let diff =
                            match git::diff_text(repo, rev.parent_sha.as_deref(), &rev.commit_sha)
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
                                "{}\n... (truncated, full diff is {} bytes)",
                                &diff[..end],
                                diff.len()
                            )
                        } else {
                            diff
                        };
                        json!({
                            "implementation_revision_id": rev.id,
                            "commit_sha": rev.commit_sha,
                            "short_sha": git::short_sha(&rev.commit_sha),
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
                "session_id": session_id.as_str(),
                "has_active_plan": true,
                "plan_id": active.id,
                "plan_state": active.state,
                "base_commit": active.base_commit,
                "joined_agents": joined_agents,
                "latest_implementation_revision": revision_payload,
            }))
        }
        other => Err(ToolError::Invalid(format!(
            "target must be 'plan' or 'implementation', got '{other}'"
        ))),
    }
}

pub async fn put_feedback(state: &AppState, req: &ToolCallRequest) -> Result<Value, ToolError> {
    let args: PutFeedbackArgs = serde_json::from_value(req.arguments.clone())
        .map_err(|e| ToolError::Invalid(format!("invalid args: {e}")))?;
    let session_id = sessions::validate_slug(&args.session_id).map_err(ToolError::Invalid)?;
    let author_label_trim = args.author_label.trim();
    if author_label_trim.is_empty() {
        return Err(ToolError::Invalid("author_label must be non-empty".into()));
    }
    let author_label = AgentLabel::from(author_label_trim.to_string());

    let kind = TargetKind::parse(&args.target_kind).ok_or_else(|| {
        ToolError::Invalid(format!(
            "target_kind must be 'plan_revision' or 'implementation_commit', got '{}'",
            args.target_kind
        ))
    })?;
    let target = match kind {
        TargetKind::PlanRevision => {
            let id: i64 = args.target_id.parse().map_err(|_| {
                ToolError::Invalid("target_id for plan_revision must be numeric".into())
            })?;
            FeedbackTargetRef::PlanRevision(id)
        }
        TargetKind::ImplementationCommit => {
            FeedbackTargetRef::ImplementationCommit(CommitSha::from(args.target_id.clone()))
        }
    };

    // Validate the feedback write before touching the agents table:
    // rejected calls (no active plan, target not in active plan, empty
    // body) must not leave a phantom agent_joined event behind.
    let outcome = state
        .lifecycle
        .put_feedback(&session_id, &author_label, target, args.body)
        .await
        .map_err(map_service_err)?;

    let now = chrono::Utc::now().timestamp();
    upsert_seen_with_event(state, &session_id, &author_label, now).await?;

    Ok(json!({
        "feedback_id": outcome.record.id,
        "plan_id": outcome.record.plan_id,
        "was_insert": outcome.was_insert,
        "was_no_op": outcome.was_no_op,
        "created_at": outcome.record.created_at,
        "updated_at": outcome.record.updated_at,
    }))
}

async fn active_feedback_counts(
    state: &AppState,
    session_id: &SessionId,
) -> Result<(i64, i64), ToolError> {
    let active = feedback_store::list_for_active_plan(&state.pool, session_id)
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
    let mut plan = 0_i64;
    let mut implv = 0_i64;
    for f in &active {
        match f.target.kind() {
            TargetKind::PlanRevision => plan += 1,
            TargetKind::ImplementationCommit => implv += 1,
        }
    }
    Ok((plan, implv))
}
