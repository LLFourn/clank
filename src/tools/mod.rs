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
                          and creates two feedback subdirectories: \
                          `<repo_root>/.trinity/feedback/<session_id>/plan/` and \
                          `.../impl/`. The plan file, `.git/logs/HEAD`, and both feedback \
                          directories are attached to the watcher.\n\n\
                          Subsequent calls update the plan-file path or record a new \
                          revision if the body differs. Idempotent on same-body re-calls.\n\n\
                          After registration, edit the plan file normally and drop \
                          `<author_label>.md` files into the appropriate feedback subdirectory; \
                          Trinity ingests plan revisions, implementation commits, and reviewer \
                          feedback from filesystem + git automatically."
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
            description: "Returns pointers + freshness for the session, in the v1 response \
                          schema. Never returns artifact content (plan body, commit message, \
                          diff text, feedback prose) — the agent reads `plan_file_path` \
                          directly and runs `git` locally against `repo_root` to inspect \
                          commits.\n\n\
                          - `session_id`: the session.\n\
                          - `author_label` (optional): when supplied, populates the \
                          `write_feedback` and `prior_feedback` blocks scoped to that author. \
                          Also upserts `agents.last_seen` and emits at most one `agent_joined` \
                          event on first sight.\n\n\
                          Response keys (always present, nullable when not applicable): \
                          `schema_version`, `session_id`, `repo_root`, `plan_file_path`, \
                          `git_logs_head_path`, `phase`, `review_target`, \
                          `latest_plan_revision`, `latest_implementation_revision`, \
                          `write_feedback` (caller's current-phase file), `prior_feedback` \
                          (`self` + `others` from the caller's prior phase, e.g. plan files \
                          surfaced during implementation), and `other_feedback_files` \
                          grouped by `plan` / `impl` arrays.\n\n\
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
