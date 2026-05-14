//! Static MCP tool catalog. The descriptor schema is used by `mcp_shim`
//! to answer `tools/list` over stdio without the daemon being up. The
//! actual dispatch logic lives in `server::mcp`.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDescriptor {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// Three coordination tools (plus the `echo_cwd` diagnostic stub).
///
/// `start_plan` creates the plan file in the working tree; the agent must
/// then commit it before Trinity treats the session as live.
///
/// `get_context` returns the canonical per-session waiting_on, phase,
/// plan_worktree_status, review_gate, and pr_hint. Read-only.
///
/// `list_sessions` returns the in-memory session summary for one or all
/// known repos.
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
                          `git rev-parse --show-toplevel`). Returns each session's id, \
                          plan_path, phase, plan_worktree_status, and waiting_on."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        },
        ToolDescriptor {
            name: "start_plan".to_string(),
            description: "Create a new plan file at `.trinity/plans/<session_id>.md` in the \
                          caller's repo. Adopts an existing file (never overwrites body). \
                          Ensures `.gitignore` excludes `.trinity/feedback/` and \
                          `.trinity/cache/` but keeps `.trinity/plans/` tracked.\n\n\
                          The session does NOT exist in Trinity until the file is committed. \
                          After this call, edit the plan file body and then `git add` + \
                          `git commit` — that commit is the plan_intro under the walk-back \
                          attribution model.\n\n\
                          Inputs: `session_id` (URL-safe slug, e.g. `my-plan`); `label` \
                          (attribution name for the agent).\n\n\
                          Response: `{ canonical_path, committed, next_step }`. `committed` \
                          is `false` until the user commits."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["session_id", "label"],
                "properties": {
                    "session_id": {"type": "string"},
                    "label": {"type": "string"}
                },
                "additionalProperties": false
            }),
        },
        ToolDescriptor {
            name: "get_context".to_string(),
            description: "Returns the per-session view: phase (planning | implementing | done), \
                          plan_worktree_status (clean | body_dirty | done_move_pending | \
                          missing_active_plan_file), waiting_on (role + reason + agents + \
                          description), review_gate (SHA-anchored), latest_plan_revision, \
                          latest_implementation_revision.\n\n\
                          When `session_id` doesn't correspond to a plan file in HEAD, returns \
                          an error `session_not_committed` — commit the plan to register the \
                          session.\n\n\
                          Inputs: `session_id`; `author_label` (optional).\n\n\
                          Always call this before reviewing or implementing. The `waiting_on` \
                          field tells you whether the current bottleneck is master or reviewers."
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
