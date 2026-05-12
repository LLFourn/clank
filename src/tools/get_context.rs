//! `get_context` — the agent-facing read tool. Returns POINTERS and
//! FRESHNESS, never artifact content. The agent reads `plan_file_path`
//! directly from disk and runs `git` locally against `repo_root`. Other
//! agents' feedback bodies are read off the filesystem via the paths
//! in `other_feedback_files`.
//!
//! Audit-state contract: read-only for sessions/plans/feedback state.
//! The single permitted mutation is `agents.last_seen` upsert + at-most-one
//! `agent_joined` event when `author_label` is supplied. Does not bump
//! `sessions.updated_at`.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::daemon::AppState;
use crate::daemon::git;
use crate::daemon::internal_api::ToolCallRequest;
use crate::domain::TargetKind;
use crate::lifecycle::{AgentLabel, SessionId};
use crate::storage::{
    feedback_files, implementation_revisions as impl_revs, plan_revisions, plans, sessions,
};

use super::ToolError;
use super::master::upsert_seen_with_event;

#[derive(Debug, Deserialize)]
struct GetContextArgs {
    session_id: String,
    #[serde(default)]
    author_label: Option<String>,
}

pub async fn get_context(state: &AppState, req: &ToolCallRequest) -> Result<Value, ToolError> {
    let args: GetContextArgs = serde_json::from_value(req.arguments.clone())
        .map_err(|e| ToolError::Invalid(format!("invalid args: {e}")))?;
    let session_id = sessions::validate_slug(&args.session_id).map_err(ToolError::Invalid)?;

    // Existence check FIRST so unknown session is 404 not 500 from FK.
    let session = sessions::fetch(&state.pool, &session_id)
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?
        .ok_or_else(|| ToolError::NotFound(format!("session `{session_id}` not found")))?;

    let author_label = args
        .author_label
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| AgentLabel::from(s.to_string()));

    if let Some(label) = author_label.as_ref() {
        let now = chrono::Utc::now().timestamp();
        upsert_seen_with_event(state, &session_id, label, now).await?;
    }

    let active = match plans::load_active_plan(&state.pool, &session_id).await {
        Ok(p) => p,
        Err(plans::LoadActivePlanError::Sql(e)) => {
            return Err(ToolError::Internal(anyhow::anyhow!(e)));
        }
        Err(plans::LoadActivePlanError::Inconsistent(inc)) => {
            return Err(ToolError::Internal(anyhow::anyhow!(inc)));
        }
    };

    let mut payload = json!({
        "session_id": session_id.as_str(),
        "repo_root": session.repo_root,
        "plan_file_path": session.plan_file_path,
    });

    let Some(active) = active else {
        payload["phase"] = json!("no_active_plan");
        if let Some(label) = author_label.as_ref() {
            payload["feedback_file"] =
                feedback_file_pointer_only(&session.repo_root, &session_id, label);
        }
        return Ok(payload);
    };

    let phase: &str = if active.state == "implementing" {
        "implementing"
    } else {
        "planning"
    };
    payload["phase"] = json!(phase);

    let latest_plan_rev = plan_revisions::latest_for_plan(&state.pool, active.id)
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
    if let Some(rev) = latest_plan_rev.as_ref() {
        payload["latest_plan_revision"] = json!({
            "id": rev.id,
            "number": rev.revision_number,
            "content_hash": rev.content_hash,
            "created_at": rev.created_at,
        });
    }

    // Resolve active target via the shared service helper so the MCP
    // response, file-ingest dispatcher, and web UI all agree after a
    // reset-to-older-SHA.
    let active_target = state
        .lifecycle
        .resolve_active_target(&session_id)
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
    let target_kind_now = if phase == "implementing" {
        TargetKind::ImplementationCommit
    } else {
        TargetKind::PlanRevision
    };
    let target_id_now: Option<String> = active_target.as_ref().map(|t| t.id.clone());

    if phase == "implementing"
        && let Some(target) = active_target.as_ref()
    {
        let head_sha = git::rev_parse_head(Path::new(&session.repo_root))
            .await
            .map_err(|e| ToolError::Internal(anyhow::anyhow!("rev-parse HEAD: {e}")))?;
        let rev = impl_revs::fetch_by_sha(
            &state.pool,
            active.id,
            &crate::lifecycle::CommitSha::from(target.id.clone()),
        )
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
        if let Some(rev) = rev.as_ref() {
            let porcelain = git::worktree_porcelain(Path::new(&session.repo_root))
                .await
                .unwrap_or_default();
            let dirty = !porcelain.trim().is_empty();
            let status_hash = blake3::hash(porcelain.as_bytes()).to_hex().to_string();

            payload["latest_implementation_revision"] = json!({
                "commit_sha": rev.commit_sha,
                "parent_sha": rev.parent_sha,
                "branch": rev.branch,
                "is_head": rev.commit_sha == head_sha,
                "worktree_dirty": dirty,
                "worktree_status_hash": status_hash,
                "created_at": rev.created_at,
            });
        }
    }
    if let Some(t) = active_target.as_ref() {
        payload["active_target"] = json!({
            "kind": t.kind.as_str(),
            "id": t.id,
        });
    }

    let all_files = feedback_files::list_for_session(&state.pool, &session_id)
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;

    if let Some(label) = author_label.as_ref() {
        let conv_path = convention_path(&session.repo_root, &session_id, label);
        let row = all_files
            .iter()
            .find(|f| f.author_label == label.as_str())
            .cloned();
        let exists = conv_path.exists();
        payload["feedback_file"] = feedback_file_view(
            &conv_path,
            exists,
            row.as_ref(),
            target_kind_now,
            target_id_now.as_deref(),
        );
    }

    let other_files: Vec<Value> = all_files
        .iter()
        .filter(|f| {
            author_label
                .as_ref()
                .is_none_or(|l| f.author_label != l.as_str())
        })
        .map(|f| {
            let status = derive_status(f, target_kind_now, target_id_now.as_deref());
            json!({
                "author_label": f.author_label,
                "path": f.path,
                "status": status,
                "last_ingested_at": f.last_ingested_at,
                "last_ingested_target": f.last_ingested_target_kind.as_ref().and_then(|k| {
                    f.last_ingested_target_id
                        .as_ref()
                        .map(|id| json!({ "kind": k, "id": id }))
                }),
            })
        })
        .collect();
    if !other_files.is_empty() {
        payload["other_feedback_files"] = json!(other_files);
    }

    Ok(payload)
}

fn convention_path(repo_root: &str, session_id: &SessionId, label: &AgentLabel) -> PathBuf {
    Path::new(repo_root)
        .join(".trinity")
        .join("feedback")
        .join(session_id.as_str())
        .join(format!("{}.md", label.as_str()))
}

/// `feedback_file` view when no sidecar row exists yet. Returns the
/// convention path so the caller knows where to write.
fn feedback_file_pointer_only(
    repo_root: &str,
    session_id: &SessionId,
    label: &AgentLabel,
) -> Value {
    let path = convention_path(repo_root, session_id, label);
    let exists = path.exists();
    json!({
        "path": path,
        "exists": exists,
        "status": "not_yet_ingested",
        "last_observed_hash": Value::Null,
        "last_observed_at": Value::Null,
        "last_ingested_hash": Value::Null,
        "last_ingested_at": Value::Null,
        "last_ingested_target": Value::Null,
        "parse_error": Value::Null,
    })
}

fn feedback_file_view(
    conv_path: &Path,
    exists: bool,
    row: Option<&feedback_files::FeedbackFile>,
    cur_kind: TargetKind,
    cur_id: Option<&str>,
) -> Value {
    let Some(row) = row else {
        return json!({
            "path": conv_path,
            "exists": exists,
            "status": "not_yet_ingested",
            "last_observed_hash": Value::Null,
            "last_observed_at": Value::Null,
            "last_ingested_hash": Value::Null,
            "last_ingested_at": Value::Null,
            "last_ingested_target": Value::Null,
            "parse_error": Value::Null,
        });
    };
    json!({
        "path": row.path,
        "exists": exists,
        "status": derive_status(row, cur_kind, cur_id),
        "last_observed_hash": row.last_observed_hash,
        "last_observed_at": row.last_observed_at,
        "last_ingested_hash": row.last_ingested_hash,
        "last_ingested_at": row.last_ingested_at,
        "last_ingested_target": row
            .last_ingested_target_kind
            .as_ref()
            .and_then(|k| row
                .last_ingested_target_id
                .as_ref()
                .map(|id| json!({ "kind": k, "id": id }))),
        "parse_error": row.parse_error,
    })
}

fn derive_status(
    row: &feedback_files::FeedbackFile,
    cur_kind: TargetKind,
    cur_id: Option<&str>,
) -> &'static str {
    if row.parse_error.is_some() {
        return "parse_error";
    }
    if row.last_observed_hash.is_none() {
        return "missing";
    }
    let target_matches = matches!(
        (&row.last_ingested_target_kind, &row.last_ingested_target_id, cur_id),
        (Some(k), Some(id), Some(want_id))
            if k.as_str() == cur_kind.as_str() && id == want_id
    );
    let hash_synced = match (&row.last_ingested_hash, &row.last_observed_hash) {
        (Some(ing), Some(obs)) => ing == obs,
        _ => false,
    };
    if target_matches && hash_synced {
        "current"
    } else if row.last_ingested_hash.is_some() {
        "stale"
    } else {
        "not_yet_ingested"
    }
}
