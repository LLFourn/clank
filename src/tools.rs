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
/// `work_context` returns the narrow coordination view for one plan:
/// the same `WorkPayload` shape `wait_for_work` returns (flattened
/// `{plan_id, repo, kind, ...action fields}`) plus state context
/// (`current_path`, `lifecycle`, `phase`, `plan_worktree_status`,
/// `waiting_on`). Read-only and synchronous.
///
/// `list_plans` returns the in-memory plan summary for the caller's repo,
/// including any plan-key conflicts the operator must resolve.
///
/// `wait_for_work` long-polls until a plan needs the caller's role, then
/// returns minimal identifiers. Replaces poll-loops over `work_context` /
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
            name: "watch_repo".to_string(),
            description: "Register a git repo with the trinity daemon. Idempotent — \
                          re-registering an already-watched repo returns \
                          `status: \"already_watching\"`.\n\n\
                          This is the modern bootstrap primitive: it does ONLY the \
                          repo-watching work (resolve canonical path, derive basename, add \
                          to the watched set). Plan-file creation is a filesystem convention: \
                          write `.trinity/plans/<slug>.md` and `git commit` it; the daemon's \
                          fold pipeline picks it up automatically.\n\n\
                          Inputs:\n\
                          - `path` (optional): absent uses the caller's cwd-repo (`git \
                            rev-parse --show-toplevel`); absolute path uses it as-is; \
                            relative path is resolved against cwd. Basename-only is NOT \
                            accepted for registration — an unknown basename has no root to \
                            resolve to.\n\n\
                          Response: `{ repo, basename, status }` where `status` is \
                          `\"registered\"` or `\"already_watching\"`.\n\n\
                          Errors: `Forbidden` when the basename is shadowed by a different \
                          canonical path already in the watched set."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Optional repo path. Absolute or relative-from-cwd. Absent = cwd."
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
                          Prefer `watch_repo` + a filesystem write to \
                          `.trinity/plans/<slug>.md` for new plans — `start_plan` remains as \
                          a convenience wrapper that combines both.\n\n\
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
            description: "Block until a plan needs the caller's role, then return the work \
                          to do as a flat payload. This is the idiomatic way to drive an \
                          agent loop — replaces polling `list_plans` / `work_context` on a \
                          timer. The happy-path response carries `{ plan_id, repo, kind, \
                          ...action-specific fields }` with the action variant flattened to \
                          the top level — the same shape `work_context` embeds.\n\n\
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
                            write path for `write_feedback`. The MCP shim caches this.\n\
                          - `timeout_secs` (optional, positive seconds, default 1800 / 30 minutes).\n\n\
                          The work payload is wrapped: `{ ...work fields..., \
                          stale_reviews: [...] }`. `stale_reviews` is a master-only \
                          sidecar listing reviews against superseded commits (one-shot \
                          per agent; empty `[]` for reviewers). See action variants \
                          below.\n\n\
                          Action variants (tagged by `kind` on the wire):\n\
                          - `write_feedback { path, target_sha, plan_file: { path, \
                            content? } }` — reviewer: write your verdict markdown \
                            (`APPROVE\\n...` / `REQUEST_CHANGES\\n...`) to `path` against \
                            the commit at `target_sha`. `plan_file.content` is inlined \
                            on first encounter of this plan file by this agent at this \
                            content hash; absent on subsequent polls and for files >64 KB.\n\
                          - `address_changes { target_sha, reviews: [{ path, author, \
                            verdict, content? }, ...], plan_path? }` — master: each \
                            entry in `reviews` is a blocking review (request_changes or \
                            unmarked) you must address. `content` is inlined on first \
                            encounter per (agent, path, hash). `plan_path` is present \
                            when the review is plan-side (PlanOnly/Mixed) — revise the \
                            plan body too.\n\
                          - `commit_plan_revision { plan_path }` — master: the plan file \
                            is dirty in the worktree. Commit the revision.\n\
                          - `start_implementation { previous_commit, plan_path }` — \
                            master: the previous commit is approved; write the next \
                            implementation commit.\n\
                          - `session_finished` — plan is finalized; no further action.\n\n\
                          Stale reviews (master-only sidecar):\n\
                          `stale_reviews: [{ path, author, verdict, target_sha, content? }, \
                          ...]` — reviews recorded against a non-current target. Delivered \
                          exactly once per (agent, path); rides on a legitimate master \
                          wakeup (does NOT independently wake master). Reviewer-bound \
                          responses always have `stale_reviews: []`.\n\n\
                          Timeout responses:\n\
                          ```\n\
                          { \"timed_out\": true }\n\
                          { \"timed_out\": true, \"no_active_plans\": true, \"repo\": \"...\" }\n\
                          { \"error\": \"ambiguous_plan\", \"candidates\": [{\"plan_id\":...},...] }\n\
                          ```"
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
                        "description": "Your agent label. Required to construct the canonical reviewer write path for write_feedback work. The MCP shim caches this across calls."
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
            name: "work_context".to_string(),
            description: "Returns the narrow coordination view for one plan: the work \
                          payload (same shape as `wait_for_work`'s happy path — `plan_id`, \
                          `repo`, `kind`, plus action-specific fields flattened to the top \
                          level) plus state context (`current_path`, `lifecycle`, `phase`, \
                          `plan_worktree_status`, `waiting_on`). Read-only. Synchronous \
                          (no blocking — use `wait_for_work` if you want to block).\n\n\
                          This is intentionally smaller than the HTTP `/api/plan/<id>` shape — \
                          it omits `commits[]`, full timelines, archived cycles, plan body \
                          markdown, and PR hints. Those are UI-content surfaces; MCP only \
                          coordinates work. Read HTTP if you need them.\n\n\
                          Errors: `invalid_plan_id`, `unknown_repo`, `unknown_plan`, \
                          `plan_not_committed`, `plan_conflict`, `no_active_plan` (inference \
                          path: zero active plans in the resolved repo), `ambiguous_plan` \
                          (inference path: multiple active plans — caller must retry with \
                          explicit `plan_id`).\n\n\
                          Inputs:\n\
                          - `plan_id` (optional): canonical `<repo_basename>/<stem>.md`. If \
                            omitted, the daemon infers from `repo` (or the caller's cwd): \
                            exactly one active+visible plan resolves it, zero returns \
                            `no_active_plan`, multiple returns `ambiguous_plan` with a \
                            candidate list.\n\
                          - `repo` (optional): scopes the inference. Basename or abs path.\n\
                          - `author_label` (optional, defaults to last cached).\n\n\
                          The `waiting_on` field tells you whether the current bottleneck is \
                          master or reviewers; `write_feedback.path` is the canonical \
                          reviewer write location when applicable."
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
        ToolDescriptor {
            name: "set_active_work".to_string(),
            description: "Record an ephemeral active-plan selection for the current agent. \
                          `wait_for_work` and `work_context` consult this when the caller \
                          omits `plan_id` and the resolved repo has multiple active plans — \
                          instead of raising `ambiguous_plan`, the resolver uses the selection.\n\n\
                          The selection is in-memory only: it does not create plan files, \
                          change review gates, emit timeline events, or persist across daemon \
                          restart. Stale selections (the plan is frozen, missing from the \
                          worktree, or in the wrong repo) are dropped on first consult.\n\n\
                          Inputs:\n\
                          - `plan_id` (required): canonical `<repo_basename>/<stem>.md`.\n\
                          - `repo` (optional): if present, must resolve to the same basename \
                            as `plan_id`. Mismatch is an invalid-args error.\n\
                          - `author_label` (daemon-required, schema-optional for shim autofill).\n\n\
                          Errors: `unknown_repo`, `plan_not_committed`, plus `Invalid` when \
                          plan is frozen or when `repo` arg disagrees with the plan_id basename."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["plan_id"],
                "properties": {
                    "plan_id": {
                        "type": "string",
                        "description": "Canonical `<repo_basename>/<stem>.md`."
                    },
                    "repo": {
                        "type": "string",
                        "description": "Optional. If present, must match plan_id's basename."
                    },
                    "author_label": {"type": "string"}
                },
                "additionalProperties": false
            }),
        },
        ToolDescriptor {
            name: "clear_active_work".to_string(),
            description: "Drop the ephemeral active-plan selection for the current agent in \
                          the resolved repo, if any. Idempotent — clearing an absent selection \
                          is a non-error.\n\n\
                          Inputs:\n\
                          - `repo` (optional): basename or absolute path; defaults to caller's cwd-repo.\n\
                          - `author_label` (daemon-required, schema-optional for shim autofill)."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "repo": {
                        "type": "string",
                        "description": "Optional repo filter. Basename or absolute path."
                    },
                    "author_label": {"type": "string"}
                },
                "additionalProperties": false
            }),
        },
    ]
}
