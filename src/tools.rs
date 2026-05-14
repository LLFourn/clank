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

/// Four coordination tools (plus the `echo_cwd` diagnostic stub).
///
/// `start_plan` creates the plan file in the working tree; the agent must
/// then commit it before Trinity treats the session as live.
///
/// `get_context` returns the canonical per-session waiting_on, phase,
/// plan_worktree_status, review_gate, and pr_hint. Read-only.
///
/// `list_sessions` returns the in-memory session summary for one or all
/// known repos.
///
/// `wait_for_work` long-polls until a session needs the caller's role,
/// then returns minimal identifiers. Replaces poll-loops over
/// `get_context` / `list_sessions`.
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
            description: "List Trinity sessions in the caller's repo. Default repo is resolved \
                          via `git rev-parse --show-toplevel`; pass `repo` (absolute path) to \
                          target a different watched repo. Each row: id, plan_path, phase, \
                          plan_worktree_status, waiting_on."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "repo": {
                        "type": "string",
                        "description": "Absolute repo root path. Optional; defaults to the caller's cwd-repo."
                    }
                },
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
            name: "wait_for_work".to_string(),
            description: "Block until one named session in one repo needs the caller's role, \
                          then return the work to do plus the file paths to act on. This is \
                          the idiomatic way to drive an agent loop — replaces polling \
                          `list_sessions` / `get_context` on a timer.\n\n\
                          Single-session, single-repo by design: you say which session you're \
                          watching, the response says what to do for that session.\n\n\
                          Inputs:\n\
                          - `role` (required, `master` | `reviewers`).\n\
                          - `session_id` (required): the session you're watching.\n\
                          - `author_label` (required): your label. Used to construct the \
                          canonical reviewer write path for `review_plan` / `review_impl`. \
                          The MCP shim caches this across calls so you usually pass it once.\n\
                          - `repo` (optional absolute path): defaults to the caller's \
                          cwd-repo (via `git rev-parse --show-toplevel`). HTTP callers must \
                          pass this explicitly.\n\
                          - `timeout_secs` (optional, 1–300, default 60).\n\n\
                          Response — one of two shapes:\n\
                          ```\n\
                          { \"work\": \"<action>\", \"locations\": [\"<repo-relative path>\", ...] }\n\
                          ```\n\
                          or, after the timeout:\n\
                          ```\n\
                          { \"timed_out\": true }\n\
                          ```\n\n\
                          The `work` vocabulary is the imperative action you should take. \
                          `locations` are repo-relative paths whose meaning depends on \
                          `work`:\n\
                          - `review_plan` → `[<canonical write path for your APPROVE / \
                          REQUEST_CHANGES feedback file>]`. Create that file.\n\
                          - `review_impl` → same, but for an implementation commit.\n\
                          - `address_plan_request_changes` → `[<each REQUEST_CHANGES \
                          feedback file on the current plan target>, <plan file>]`. Read the \
                          feedbacks; revise the plan; commit.\n\
                          - `address_impl_request_changes` → `[<each REQUEST_CHANGES \
                          feedback file on the current impl target>]`. Read the feedbacks; \
                          fix in code; commit.\n\
                          - `commit_plan_revision` → `[<plan file>]`. The plan was edited; \
                          commit the revision.\n\
                          - `commit_done_move` / `restore_or_commit_done_move` → \
                          `[<plan file>]`. Resolve the active-vs-done state.\n\
                          - `implement_and_commit` → `[<plan file>]`. Plan is approved; \
                          start implementing.\n\
                          - `move_to_done` → `[<plan file>]`. Implementation approved; move \
                          the plan to `.trinity/plans/done/` and commit.\n\n\
                          Typical reviewer loop:\n\
                          ```\n\
                          loop {\n\
                            let r = wait_for_work({\n\
                              role: \"reviewers\",\n\
                              session_id: \"my-feature\",\n\
                              author_label: \"codex\"\n\
                            });\n\
                            if r.timed_out { continue; }\n\
                            // r.work == \"review_impl\"\n\
                            // r.locations[0] == \".trinity/feedback/my-feature/impl/<sha>/codex.md\"\n\
                            // Read the impl commit, write your verdict to that path.\n\
                          }\n\
                          ```\n\n\
                          Master loop is the same with `role: \"master\"` — the response will \
                          tell you whether to commit a plan revision, address request_changes, \
                          implement, or move to done."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["role", "session_id"],
                "additionalProperties": false,
                "properties": {
                    "role": {
                        "type": "string",
                        "enum": ["master", "reviewers"],
                        "description": "Which role's attention you're polling for."
                    },
                    "session_id": {
                        "type": "string",
                        "description": "The single session you're watching. Use `list_sessions` to discover ids."
                    },
                    "author_label": {
                        "type": "string",
                        "description": "Your agent label. Required to construct the canonical reviewer write path for review_* work. The MCP shim caches this across calls — pass it on the first call and subsequent calls inherit it. Schema-optional so cache inheritance works with strict MCP clients; the daemon errors clearly if it's never been provided."
                    },
                    "repo": {
                        "type": "string",
                        "description": "Absolute repo root path. Optional over MCP (defaults to caller's cwd-repo); required over HTTP."
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 300,
                        "default": 60,
                        "description": "How long to block before returning `timed_out: true`."
                    }
                }
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
                          Inputs: `session_id`; `author_label` (optional, defaults to \
                          last cached); `repo` (optional absolute path; defaults to the \
                          caller's cwd-repo).\n\n\
                          Always call this before reviewing or implementing. The `waiting_on` \
                          field tells you whether the current bottleneck is master or reviewers."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["session_id"],
                "properties": {
                    "session_id": {"type": "string"},
                    "author_label": {"type": "string"},
                    "repo": {
                        "type": "string",
                        "description": "Absolute repo root path. Optional; defaults to the caller's cwd-repo."
                    }
                },
                "additionalProperties": false
            }),
        },
    ]
}
