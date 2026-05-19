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

/// Four coordination tools.
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
///
/// The `echo_cwd` diagnostic remains in the dispatcher
/// (`src/server/mcp.rs`) for hand-rolled `tools/call` debugging but is not
/// advertised here.
pub fn catalog() -> Vec<ToolDescriptor> {
    vec![
        ToolDescriptor {
            name: "list_plans".to_string(),
            description: "List Trinity plans in one repo. Defaults to the caller's cwd-repo; \
                          pass `repo` (basename or absolute path) to scope to a specific watched \
                          repo. Cross-repo aggregation is not exposed on MCP; use HTTP \
                          `/api/plans` if you need it. Response: `{ plans, conflicts }`. Each \
                          plan row: `{ plan_id, slug, state, current_path, phase, \
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
                        "description": "Filter by repo. Basename or absolute path. Optional; defaults to caller's cwd-repo."
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
            description: "Block until a plan needs the caller's role, then return the \
                          work to do plus the file paths to act on. This is the idiomatic way \
                          to drive an agent loop — replaces polling `list_plans` / \
                          `get_context` on a timer.\n\n\
                          Inputs:\n\
                          - `role` (required, `master` | `reviewers`).\n\
                          - `plan_id` (optional): canonical `<repo_basename>/<stem>.md`. \
                            If omitted, the daemon infers from the caller's cwd-repo (or \
                            `repo` below): if exactly one active plan exists, uses it; if \
                            zero, returns `{timed_out: true, no_active_plans: true}`; if \
                            multiple, returns `{error: \"ambiguous_plan\", candidates: [...]}`.\n\
                          - `repo` (optional): basename or absolute path. Scopes the \
                            plan-id inference when `plan_id` is omitted. Ignored otherwise.\n\
                          - `author_label` (daemon-required, schema-optional for shim \
                            autofill): your label. Used to construct the canonical reviewer \
                            write path for `review_commit`. The MCP shim caches this.\n\
                          - `timeout_secs` (optional, positive seconds, default 1800 / 30 minutes).\n\n\
                          Response — one of:\n\
                          ```\n\
                          { \"plan_id\": \"...\", \"repo\": \"<abs path>\", \
                          \"work\": \"<action>\", \"locations\": [\"<repo-relative path>\", ...], \
                          \"target_sha\": \"...\", \"commit_kind\": \"plan_only|code_only|mixed\", \
                          \"prompt_hint\": \"...\" }\n\
                          ```\n\
                          ```\n\
                          { \"timed_out\": true }\n\
                          { \"timed_out\": true, \"no_active_plans\": true, \"repo\": \"...\" }\n\
                          { \"error\": \"ambiguous_plan\", \"candidates\": [{\"plan_id\":...},...] }\n\
                          ```\n\n\
                          The `work` vocabulary (post phase 2.5):\n\
                          - `review_commit` → `[<canonical write path>]`\n\
                          - `address_commit_changes` → `[<each RC>, <plan file?>]`\n\
                          - `commit_plan_revision` → `[<plan file>]`\n\
                          - `start_implementation` → `[<plan file>]`"
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["role"],
                "additionalProperties": false,
                "properties": {
                    "role": {
                        "type": "string",
                        "enum": ["master", "reviewers"],
                        "description": "Which role's attention you're polling for."
                    },
                    "plan_id": {
                        "type": "string",
                        "description": "Canonical `<repo_basename>/<stem>.md`. Optional — if omitted, the daemon infers from cwd-repo (single active plan). Discover plans with `list_plans`."
                    },
                    "repo": {
                        "type": "string",
                        "description": "Optional repo filter for plan-id inference. Basename or absolute path."
                    },
                    "author_label": {
                        "type": "string",
                        "description": "Your agent label. Required to construct the canonical reviewer write path for review_commit work. The MCP shim caches this across calls."
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "minimum": 1,
                        "default": 1800,
                        "description": "How long to block before returning `timed_out: true`. Default 1800 seconds (30 minutes); no upper cap."
                    }
                }
            }),
        },
        ToolDescriptor {
            name: "get_context".to_string(),
            description: "Returns the per-plan view: `{ plan_id, slug, state, current_path, \
                          phase, plan_worktree_status, waiting_on, review_gate, commits, \
                          latest_relevant_commit, latest_plan_revision, ... }`. \
                          `state` is `active` | `done`. `current_path` is the plan's current \
                          repo-relative path. `commits[]` is the canonical per-commit gate + \
                          feedback array — the canonical feedback shape on the wire.\n\n\
                          Errors: `invalid_plan_id`, `unknown_repo`, `unknown_plan`, \
                          `plan_not_committed`, `plan_conflict`, `no_active_plan` (inference \
                          path: zero active plans in the resolved repo), `ambiguous_plan` \
                          (inference path: multiple active plans — caller must retry with \
                          explicit `plan_id`).\n\n\
                          Inputs:\n\
                          - `plan_id` (optional): canonical `<repo_basename>/<stem>.md`. If \
                            omitted, the daemon infers from `repo` (or the caller's cwd): \
                            exactly one active plan resolves it, zero returns `no_active_plan`, \
                            multiple returns `ambiguous_plan` with a candidate list.\n\
                          - `repo` (optional): scopes the inference. Basename or abs path.\n\
                          - `author_label` (optional, defaults to last cached).\n\n\
                          Always call this before reviewing or implementing. The `waiting_on` \
                          field tells you whether the current bottleneck is master or reviewers."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "plan_id": {
                        "type": "string",
                        "description": "Canonical `<repo_basename>/<stem>.md`. Optional — if omitted, the daemon infers from cwd-repo / `repo` (single active plan)."
                    },
                    "repo": {
                        "type": "string",
                        "description": "Optional repo filter for plan-id inference. Basename or absolute path."
                    },
                    "author_label": {"type": "string"}
                },
                "additionalProperties": false
            }),
        },
    ]
}
