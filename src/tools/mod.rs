use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::daemon::AppState;
use crate::daemon::internal_api::ToolCallRequest;

pub(crate) mod master;
mod reviewer;

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

/// Tool descriptor in the shape rmcp expects.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDescriptor {
    pub name: String,
    pub description: String,
    /// JSON Schema for arguments.
    pub input_schema: Value,
}

pub fn catalog() -> Vec<ToolDescriptor> {
    vec![
        ToolDescriptor {
            name: "echo_cwd".to_string(),
            description: "Diagnostic stub. Returns the cwd captured by the stdio shim at \
                          launch plus the label cached for this MCP session (if any). Use \
                          to verify the shell is wired to Trinity before calling real tools."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        },
        ToolDescriptor {
            name: "register_plan_file".to_string(),
            description: "Register (or update) a plan file as the artifact Trinity should watch \
                          for a session.\n\n\
                          - `session_id`: caller-chosen URL-safe slug \
                          (`A–Z a–z 0–9 _ - .`, 1–64 chars). Identifies a coordination thread \
                          that may span many plan lifecycle attempts.\n\
                          - `path`: absolute or repo-relative path to the plan file. The file \
                          must already exist; Trinity will not create it.\n\
                          - `label`: attribution name for this agent (e.g. `claude-main`); \
                          recorded on the emitted event and on `agents.last_seen`. No \
                          ownership / claim is implied.\n\n\
                          Trinity captures the repo's current HEAD as the new plan's \
                          `base_commit`, reads the file body, and starts a new lifecycle in \
                          `planning` if there is no active plan for `session_id`. If the \
                          session is already active in planning with the same body, this is \
                          a no-op. If the body differs while still planning, it is recorded \
                          as the next `plan_revision` under the same `plan_id`. If \
                          implementation has begun and the body differs, the active plan is \
                          archived and a new one starts based on current HEAD.\n\n\
                          After registration, just edit the plan file normally — the watcher \
                          picks up changes automatically."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["session_id", "path", "label"],
                "properties": {
                    "session_id": {"type": "string", "description": "URL-safe slug"},
                    "path": {"type": "string"},
                    "label": {"type": "string"}
                },
                "additionalProperties": false
            }),
        },
        ToolDescriptor {
            name: "register_implementation_commit".to_string(),
            description: "Tell Trinity a git commit is ready for implementation review.\n\n\
                          - `session_id`: the session.\n\
                          - `commit_sha`: full or short SHA. Trinity validates it exists in \
                          the session's `repo_root` and captures parent / branch / message / \
                          diff-stat / worktree state.\n\
                          - `force` (default false): by default Trinity rejects if the SHA \
                          isn't current HEAD. Pass true to register an older commit.\n\
                          - `label` (optional): attribution.\n\n\
                          If the session is in `planning`, this transitions it to \
                          `implementing`. Same-SHA re-registration is idempotent (no \
                          duplicate revisions). Amend/fixup workflows: commit, register, \
                          repeat — old feedback stays attached to the old SHA."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["session_id", "commit_sha"],
                "properties": {
                    "session_id": {"type": "string"},
                    "commit_sha": {"type": "string"},
                    "force": {"type": "boolean", "default": false},
                    "label": {"type": "string"}
                },
                "additionalProperties": false
            }),
        },
        ToolDescriptor {
            name: "get_current_feedback".to_string(),
            description: "Returns the current feedback rows for the session's active plan, \
                          plus a digest you can compare against your last read to skip \
                          reprocessing when nothing has changed.\n\n\
                          - `session_id`: the session.\n\
                          - `target_kind` (optional): `plan_revision` or `implementation_commit`. \
                          If omitted, returns feedback for both.\n\
                          - `label` (optional): attribution; recorded on `agents.last_seen`.\n\n\
                          Response shape:\n\
                          - `state`: `\"active\"` or `\"no_active_plan\"`.\n\
                          - `plan_id`: the active plan id (only when active).\n\
                          - `feedback_digest`: hex blake3 over the rows + plan_id + filter; \
                          stable across re-reads when nothing changed.\n\
                          - `feedback[]`: `{feedback_id, plan_id, target_kind, target_id, \
                          author_label, body, created_at, updated_at}`.\n\n\
                          Re-reading is cheap and idempotent. There is no acknowledgement; \
                          if the digest matches your last read, the feedback set is unchanged."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["session_id"],
                "properties": {
                    "session_id": {"type": "string"},
                    "target_kind": {
                        "type": "string",
                        "enum": ["plan_revision", "implementation_commit"]
                    },
                    "label": {"type": "string"}
                },
                "additionalProperties": false
            }),
        },
        ToolDescriptor {
            name: "list_sessions".to_string(),
            description: "List Trinity sessions. Scoped to the repo your shell is in (via \
                          `git rev-parse --show-toplevel`). Use this to find a session_id to \
                          start posting feedback or registering commits on."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        },
        ToolDescriptor {
            name: "get_review_context".to_string(),
            description: "Returns the artifact you should review.\n\n\
                          - `session_id`: the session.\n\
                          - `target` (optional): `plan` for the latest plan revision body, \
                          `implementation` for the latest registered commit + diff. If \
                          omitted, defaults to whichever artifact matches the active plan's \
                          state (`planning`→plan, `implementing`→implementation).\n\
                          - `label` (optional): attribution; recorded on `agents.last_seen`."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["session_id"],
                "properties": {
                    "session_id": {"type": "string"},
                    "target": {"type": "string", "enum": ["plan", "implementation"]},
                    "label": {"type": "string"}
                },
                "additionalProperties": false
            }),
        },
        ToolDescriptor {
            name: "put_feedback".to_string(),
            description: "Upserts feedback against an artifact of the session's active plan. \
                          Each `(target, author_label)` slot holds one current row; calling \
                          again with the same target updates the body in place. Identical-body \
                          re-calls are a no-op (no update, stable digest).\n\n\
                          - `session_id`: the session.\n\
                          - `target_kind`: `plan_revision` or `implementation_commit`.\n\
                          - `target_id`: numeric `revision_id` (plan_revision) or full \
                          `commit_sha` (implementation_commit), from `get_review_context`.\n\
                          - `body`: your critique (markdown).\n\
                          - `author_label`: stable name for this reviewer (e.g. `claude-architect`).\n\n\
                          The daemon rejects feedback targeting any plan_revision or \
                          implementation_commit that doesn't belong to the active plan. \
                          Archived plans are read-only.\n\n\
                          Response carries `was_insert` (true on first post for this \
                          target+author) and `was_no_op` (true when the body matches the \
                          existing row byte-for-byte)."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["session_id", "target_kind", "target_id", "body", "author_label"],
                "properties": {
                    "session_id": {"type": "string"},
                    "target_kind": {
                        "type": "string",
                        "enum": ["plan_revision", "implementation_commit"]
                    },
                    "target_id": {"type": "string"},
                    "body": {"type": "string"},
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
        "register_plan_file" => master::register_plan_file(state, req).await,
        "register_implementation_commit" => {
            master::register_implementation_commit(state, req).await
        }
        "get_current_feedback" => master::get_current_feedback(state, req).await,
        "list_sessions" => reviewer::list_sessions(state, req).await,
        "get_review_context" => reviewer::get_review_context(state, req).await,
        "put_feedback" => reviewer::put_feedback(state, req).await,
        other => Err(ToolError::NotFound(format!("unknown tool: {other}"))),
    }
}

async fn echo_cwd(_state: &AppState, req: &ToolCallRequest) -> Result<Value, ToolError> {
    Ok(json!({
        "cwd": req.cwd,
    }))
}
