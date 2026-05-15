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
            description: "List Trinity plans in the caller's repo. Default repo is resolved \
                          via `git rev-parse --show-toplevel`; pass `repo` (absolute path) to \
                          target a different watched repo. Response: `{ plans, conflicts }`. \
                          Each plan row: `{ repo, plan_path, slug, phase, plan_worktree_status, \
                          waiting_on }`. Each conflict row: `{ slug, paths }` — the same plan \
                          stem maps to multiple files on disk; operator must resolve before \
                          any feedback can route to that plan."
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
            description: "Create a new plan file at the caller-supplied `plan_path` (must be \
                          `.trinity/plans/<stem>.md` — not nested, not under `done/`). Adopts \
                          an existing file (never overwrites body). Ensures `.gitignore` \
                          excludes `.trinity/feedback/` and `.trinity/cache/` but keeps \
                          `.trinity/plans/` tracked.\n\n\
                          The plan does NOT exist in Trinity until the file is committed. \
                          After this call, edit the plan body and then `git add` + `git commit` \
                          — that commit is the plan_intro under the walk-back attribution \
                          model.\n\n\
                          Inputs:\n\
                          - `plan_path` (required, repo-relative): canonical path to the new \
                            plan file. Rejected if outside `.trinity/plans/`, nested, in \
                            `done/`, or its stem already names an existing plan or conflict.\n\
                          - `label` (required): attribution name for the agent.\n\
                          - `repo` (optional, absolute path): defaults to the caller's \
                            cwd-repo.\n\n\
                          Response: `{ canonical_path, committed, next_step }`. `committed` \
                          is `false` until the user commits."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["plan_path", "label"],
                "properties": {
                    "plan_path": {
                        "type": "string",
                        "description": "Repo-relative path: .trinity/plans/<stem>.md"
                    },
                    "label": {"type": "string"},
                    "repo": {
                        "type": "string",
                        "description": "Absolute repo root path. Optional; defaults to the caller's cwd-repo."
                    }
                },
                "additionalProperties": false
            }),
        },
        ToolDescriptor {
            name: "wait_for_work".to_string(),
            description: "Block until the named plan in the named repo needs the caller's role, \
                          then return the work to do plus the file paths to act on. This is \
                          the idiomatic way to drive an agent loop — replaces polling \
                          `list_plans` / `get_context` on a timer.\n\n\
                          Single-plan, single-repo by design: you say which plan you're \
                          watching (`plan_path` is the canonical target on every call), the \
                          response says what to do.\n\n\
                          Inputs:\n\
                          - `role` (required, `master` | `reviewers`).\n\
                          - `plan_path` (required, repo-relative): the plan you're watching, \
                          e.g. `.trinity/plans/leptos-frontend.md` or \
                          `.trinity/plans/done/runtime-lock-boundaries.md`. The shim does \
                          NOT cache this across calls — pass it on every invocation so the \
                          target is always explicit.\n\
                          - `author_label` (daemon-required, schema-optional for shim \
                          autofill): your label. Used to construct the canonical reviewer \
                          write path for `review_plan` / `review_impl`. The MCP shim caches \
                          this across calls.\n\
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
                          the plan to `.trinity/plans/done/` and commit."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["role", "plan_path"],
                "additionalProperties": false,
                "properties": {
                    "role": {
                        "type": "string",
                        "enum": ["master", "reviewers"],
                        "description": "Which role's attention you're polling for."
                    },
                    "plan_path": {
                        "type": "string",
                        "description": "Repo-relative path to the plan file. Pass it on every call — the shim does NOT cache plan_path. Discover plans with `list_plans`."
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
            description: "Returns the per-plan view: phase (planning | implementing | done), \
                          plan_worktree_status (clean | body_dirty | done_move_pending | \
                          missing_active_plan_file), waiting_on (role + reason + agents + \
                          description), review_gate (SHA-anchored), latest_plan_revision, \
                          latest_implementation_revision.\n\n\
                          When `plan_path` doesn't correspond to a plan file in HEAD, returns \
                          an error: `plan_not_committed` (path is canonical but no plan there \
                          yet), `unknown_plan` (no plan with that stem), `plan_conflict` (same \
                          stem maps to multiple files — operator must resolve), \
                          `plan_path_mismatch` (stem matches an existing plan but the path is \
                          neither its current path nor its active/done counterpart), or \
                          `invalid_plan_path` (path doesn't parse).\n\n\
                          Inputs: `plan_path` (required, repo-relative); `author_label` \
                          (optional, defaults to last cached); `repo` (optional absolute path; \
                          defaults to the caller's cwd-repo).\n\n\
                          Always call this before reviewing or implementing. The `waiting_on` \
                          field tells you whether the current bottleneck is master or reviewers."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["plan_path"],
                "properties": {
                    "plan_path": {
                        "type": "string",
                        "description": "Repo-relative path to the plan file. Pass it on every call — the shim does NOT cache plan_path."
                    },
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
