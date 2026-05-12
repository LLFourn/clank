//! `list_sessions` MCP tool. The only read tool that does not require
//! a `session_id` argument.

use serde_json::{Value, json};

use crate::daemon::AppState;
use crate::daemon::git;
use crate::daemon::internal_api::ToolCallRequest;
use crate::domain::TargetKind;
use crate::lifecycle::SessionId;
use crate::storage::{agents, feedback as feedback_store, sessions};

use super::ToolError;

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
