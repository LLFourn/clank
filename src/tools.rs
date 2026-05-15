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
/// then commit it before Trinity treats the plan as live.
///
/// `get_context` returns the canonical per-plan waiting_on, phase,
/// plan_worktree_status, review_gate, and pr_hint. Read-only.
///
/// `list_plans` returns the in-memory plan summary for the caller's repo,
/// including any plan-key conflicts the operator must resolve.
///
/// `wait_for_work` long-polls until a plan needs the caller's role, then
/// returns minimal identifiers. Replaces poll-loops over `get_context` /
/// `list_plans`.
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
            name: "list_plans".to_string(),
            description: "List Trinity plans across watched repos. Pass `repo` (basename or \
                          absolute path) to scope. Response: `{ plans, conflicts }`. Each plan \
                          row: `{ plan_id, slug, state, current_path, phase, \
                          plan_worktree_status, waiting_on }`. `plan_id` is the canonical \
                          `<repo_basename>/<stem>.md` identifier. Each conflict row: \
                          `{ plan_id, slug, paths }` — the same plan stem maps to multiple \
                          files on disk; operator must resolve before any feedback can route."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "repo": {
                        "type": "string",
                        "description": "Filter by repo. Basename or absolute path. Optional; defaults to all watched repos."
                    }
                },
                "additionalProperties": false
            }),
        },
        ToolDescriptor {
            name: "start_plan".to_string(),
            description: "Create a new plan file at `.trinity/plans/<slug>.md` in the caller's \
                          cwd-repo. The daemon resolves the cwd-repo via `git rev-parse \
                          --show-toplevel`, canonicalizes it, and registers it with the \
                          basename derived from the working-tree root directory.\n\n\
                          Adopts an existing file (never overwrites body). Ensures \
                          `.gitignore` excludes `.trinity/feedback/` and `.trinity/cache/` \
                          but keeps `.trinity/plans/` tracked.\n\n\
                          The plan does NOT exist in Trinity until the file is committed. \
                          After this call, edit the plan body and then `git add` + `git \
                          commit` — that commit is the plan_intro under the walk-back \
                          attribution model.\n\n\
                          Inputs:\n\
                          - `slug` (required): the plan-file stem. Becomes the filename \
                            `<slug>.md`. Must not contain `/`. The resulting `plan_id` is \
                            `<repo_basename>/<slug>.md`.\n\
                          - `label` (required): attribution name for the agent.\n\n\
                          Errors: `invalid_slug`, `plan_already_exists`, `plan_in_conflict`, \
                          `repo_basename_taken` (the cwd repo's basename collides with another \
                          watched repo; rename the directory).\n\n\
                          Response: `{ plan_id, repo, canonical_path, committed, next_step }`."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["slug", "label"],
                "properties": {
                    "slug": {
                        "type": "string",
                        "description": "Plan-file stem (the basename without `.md`). No `/`."
                    },
                    "label": {"type": "string"}
                },
                "additionalProperties": false
            }),
        },
        ToolDescriptor {
            name: "wait_for_work".to_string(),
            description: "Block until the named plan needs the caller's role, then return the \
                          work to do plus the file paths to act on. This is the idiomatic way \
                          to drive an agent loop — replaces polling `list_plans` / \
                          `get_context` on a timer.\n\n\
                          Inputs:\n\
                          - `role` (required, `master` | `reviewers`).\n\
                          - `plan_id` (required): the plan you're watching, in the canonical \
                            form `<repo_basename>/<stem>.md` (e.g. `trinity/leptos-frontend.md`). \
                            The shim does NOT cache this across calls — pass it on every \
                            invocation.\n\
                          - `author_label` (daemon-required, schema-optional for shim \
                            autofill): your label. Used to construct the canonical reviewer \
                            write path for `review_plan` / `review_impl`. The MCP shim caches \
                            this across calls.\n\
                          - `timeout_secs` (optional, 1–300, default 60).\n\n\
                          Response — one of two shapes:\n\
                          ```\n\
                          { \"plan_id\": \"...\", \"repo\": \"<abs path>\", \
                          \"work\": \"<action>\", \"locations\": [\"<repo-relative path>\", ...] }\n\
                          ```\n\
                          or, after the timeout:\n\
                          ```\n\
                          { \"timed_out\": true }\n\
                          ```\n\n\
                          `repo` is the canonical absolute path so callers can resolve the \
                          repo-relative `locations` directly without parsing `plan_id`.\n\n\
                          The `work` vocabulary is the imperative action:\n\
                          - `review_plan` / `review_impl` → `[<canonical write path>]`\n\
                          - `address_plan_request_changes` → `[<each RC>, <plan file>]`\n\
                          - `address_impl_request_changes` → `[<each RC>]`\n\
                          - `commit_plan_revision` → `[<plan file>]`\n\
                          - `commit_done_move` / `restore_or_commit_done_move` → `[<plan file>]`\n\
                          - `implement_and_commit` → `[<plan file>]`\n\
                          - `move_to_done` → `[<plan file>]`"
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["role", "plan_id"],
                "additionalProperties": false,
                "properties": {
                    "role": {
                        "type": "string",
                        "enum": ["master", "reviewers"],
                        "description": "Which role's attention you're polling for."
                    },
                    "plan_id": {
                        "type": "string",
                        "description": "Canonical `<repo_basename>/<stem>.md`. Pass it on every call — the shim does NOT cache plan_id. Discover plans with `list_plans`."
                    },
                    "author_label": {
                        "type": "string",
                        "description": "Your agent label. Required to construct the canonical reviewer write path for review_* work. The MCP shim caches this across calls."
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
            description: "Returns the per-plan view: `{ plan_id, slug, state, current_path, \
                          phase, plan_worktree_status, waiting_on, review_gate, \
                          latest_plan_revision, latest_implementation_revision, ... }`. \
                          `state` is `active` | `done`. `current_path` is the plan's current \
                          repo-relative path (`.trinity/plans/<stem>.md` or \
                          `.trinity/plans/done/<stem>.md`).\n\n\
                          Errors: `invalid_plan_id`, `unknown_repo` (basename not watched), \
                          `unknown_plan` (no plan with that stem in the repo), \
                          `plan_not_committed` (the stem hasn't been committed yet), \
                          `plan_conflict` (same stem maps to multiple files).\n\n\
                          Inputs: `plan_id` (required, canonical `<repo_basename>/<stem>.md`); \
                          `author_label` (optional, defaults to last cached).\n\n\
                          Always call this before reviewing or implementing. The `waiting_on` \
                          field tells you whether the current bottleneck is master or reviewers."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["plan_id"],
                "properties": {
                    "plan_id": {
                        "type": "string",
                        "description": "Canonical `<repo_basename>/<stem>.md`. Pass it on every call — the shim does NOT cache plan_id."
                    },
                    "author_label": {"type": "string"}
                },
                "additionalProperties": false
            }),
        },
    ]
}
