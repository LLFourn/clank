use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::daemon::AppState;
use crate::daemon::internal_api::ToolCallRequest;

pub(crate) mod get_context;
pub(crate) mod master;
pub(crate) mod reviewer;

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDescriptor {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// Agent-facing MCP catalog. **Three tools only.** Everything else is
/// observed from the filesystem / git directly. See the plan at
/// `~/.claude/plans/i-want-to-auto-track-impl-commits.md` for rationale.
pub fn catalog() -> Vec<ToolDescriptor> {
    vec![
        ToolDescriptor {
            name: "echo_cwd".to_string(),
            description: "Diagnostic stub. Returns the cwd captured by the stdio shim at \
                          launch."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        },
        ToolDescriptor {
            name: "list_sessions".to_string(),
            description: "List Trinity sessions in the caller's repo (via \
                          `git rev-parse --show-toplevel`). Use this to find a \
                          `session_id` to call `get_context` on."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        },
        ToolDescriptor {
            name: "register_plan_file".to_string(),
            description: "Register a watched plan file for a session.\n\n\
                          - `session_id`: URL-safe slug, `A–Za–z0–9_-.`, 1–64 chars.\n\
                          - `path`: absolute or repo-relative path; the file must exist.\n\
                          - `label`: attribution name (e.g. `claude-main`).\n\n\
                          On first call for a session, Trinity creates the session row, \
                          starts a `planning` plan with the current HEAD as `base_commit`, \
                          creates `<repo_root>/.trinity/feedback/<session_id>/` if absent, \
                          and attaches three watchers: the plan file, `.git/logs/HEAD`, and \
                          the feedback directory. Subsequent calls update the plan-file path \
                          or record a new revision if the body differs. Idempotent on \
                          same-body re-calls.\n\n\
                          After registration, just edit the plan file normally — Trinity \
                          observes everything else (plan revisions, implementation commits, \
                          reviewer feedback files) from the filesystem and git automatically."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["session_id", "path", "label"],
                "properties": {
                    "session_id": {"type": "string"},
                    "path": {"type": "string"},
                    "label": {"type": "string"}
                },
                "additionalProperties": false
            }),
        },
        ToolDescriptor {
            name: "get_context".to_string(),
            description: "Returns pointers + freshness for the session's active review target. \
                          Never returns artifact content (plan body, commit message, diff \
                          text, feedback prose) — the agent reads `plan_file_path` directly \
                          and runs `git` locally against `repo_root` to inspect commits.\n\n\
                          - `session_id`: the session.\n\
                          - `author_label` (optional): if supplied, the response includes a \
                          `feedback_file` block telling the caller where to write their \
                          feedback file and its current ingestion status. Also upserts \
                          `agents.last_seen` and emits at most one `agent_joined` event on \
                          first sight.\n\n\
                          Read-only with respect to session/plan/feedback state. Does not \
                          bump `sessions.updated_at`."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["session_id"],
                "properties": {
                    "session_id": {"type": "string"},
                    "author_label": {"type": "string"}
                },
                "additionalProperties": false
            }),
        },
    ]
}

pub async fn dispatch(state: &AppState, req: &ToolCallRequest) -> Result<Value, ToolError> {
    match req.tool.as_str() {
        "echo_cwd" => echo_cwd(state, req).await,
        "list_sessions" => reviewer::list_sessions(state, req).await,
        "register_plan_file" => master::register_plan_file(state, req).await,
        "get_context" => get_context::get_context(state, req).await,
        other => Err(ToolError::NotFound(format!("unknown tool: {other}"))),
    }
}

async fn echo_cwd(_state: &AppState, req: &ToolCallRequest) -> Result<Value, ToolError> {
    Ok(json!({
        "cwd": req.cwd,
    }))
}
