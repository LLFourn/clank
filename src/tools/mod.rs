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
            name: "register_implementation_commit".to_string(),
            description: "Master tool. Tells Trinity that a specific git commit is ready for \
                          implementation review. Make the commit normally with `git commit`, \
                          then call this with the SHA. The plan transitions to \
                          `implementation_review`.\n\n\
                          - `plan_id`: the plan this commit implements.\n\
                          - `commit_sha`: full or short SHA. Trinity resolves it to a full \
                          SHA, captures parent, branch, commit message, diff stat, and \
                          worktree dirty state.\n\
                          - `force` (default false): by default Trinity rejects the \
                          registration if the SHA is not the current HEAD. Pass true to \
                          register an older commit (rare; typically used when re-registering \
                          after a force-push).\n\n\
                          Iterating: address impl-stage feedback by amending the commit or \
                          creating a new one, then call this tool again with the new SHA. \
                          Old impl-stage feedback stays attached to the old SHA and is \
                          flagged `outdated` in the UI."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["plan_id", "commit_sha"],
                "properties": {
                    "plan_id": {"type": "string"},
                    "commit_sha": {"type": "string"},
                    "force": {"type": "boolean", "default": false}
                },
                "additionalProperties": false
            }),
        },
        ToolDescriptor {
            name: "poll_directive".to_string(),
            description: "Master tool. Returns the oldest unacked feedback batch the human \
                          curator has delivered, or `{directive: \"none\"}` if nothing is \
                          waiting.\n\n\
                          - `plan_id`: the plan you are master of.\n\n\
                          The same batch is returned on every poll until you call \
                          `ack_directive(batch_id)` — this is intentional and makes the \
                          handoff replay-safe across crashes/reconnects. After integrating \
                          the feedback (typically by editing the plan file), call \
                          `ack_directive` to advance to the next batch."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["plan_id"],
                "properties": {"plan_id": {"type": "string"}},
                "additionalProperties": false
            }),
        },
        ToolDescriptor {
            name: "ack_directive".to_string(),
            description: "Master tool. Acknowledge a feedback batch returned by \
                          `poll_directive`. Required before the next batch will be \
                          surfaced. Idempotent — acking an already-acked batch is fine.\n\n\
                          - `plan_id`: the plan the batch belongs to. Must match the \
                          batch's plan; the daemon rejects mismatches.\n\
                          - `batch_id`: the id returned by `poll_directive`."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["plan_id", "batch_id"],
                "properties": {
                    "plan_id": {"type": "string"},
                    "batch_id": {"type": "integer"}
                },
                "additionalProperties": false
            }),
        },
        ToolDescriptor {
            name: "echo_cwd".to_string(),
            description: "Diagnostic stub. Returns the current working directory the Trinity \
                          MCP shim captured at launch and the agent label cached by the shim \
                          (if any). Use this to verify your shell is wired to Trinity correctly \
                          before calling the real registration tools."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        },
        ToolDescriptor {
            name: "register_plan_file".to_string(),
            description: "Master tool. Registers a plan file with Trinity and binds this MCP \
                          session as the master agent for that plan. Call this exactly once \
                          per shell after the plan file has been written to disk.\n\n\
                          - `path`: absolute or repo-relative path to the plan file. The file \
                          must exist; Trinity will not create it for you.\n\
                          - `label`: short identifier you choose for this agent in the timeline \
                          (e.g. `claude-main`). It must be unique within the plan.\n\n\
                          Trinity derives `repo_root` from your shell's current working \
                          directory (`git rev-parse --show-toplevel`), so you must launch the \
                          shell inside the repo whose work this plan describes. Plan identity \
                          is `hash(repo_root + canonical_plan_file_path)`.\n\n\
                          On success the daemon snapshots the plan file as the first revision \
                          (or as a `resume_snapshot` if drifted from a prior revision) and \
                          starts watching the file for changes. After registration, simply \
                          edit the plan file with your normal editing tools — Trinity detects \
                          the changes automatically and creates new revisions."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["path", "label"],
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the plan file. Absolute or relative to your cwd."
                    },
                    "label": {
                        "type": "string",
                        "description": "Stable display name for this agent in the Trinity timeline."
                    }
                },
                "additionalProperties": false
            }),
        },
        ToolDescriptor {
            name: "list_plans".to_string(),
            description: "List Trinity-managed plans. By default this is scoped to the repo \
                          your shell is in (Trinity derives repo_root via `git rev-parse \
                          --show-toplevel`). If your cwd is not in a git repo, returns all \
                          active plans. Use this to discover which plan to join as a reviewer."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        },
        ToolDescriptor {
            name: "join_plan".to_string(),
            description: "Reviewer tool. Joins an existing managed plan as a reviewer and binds \
                          this MCP session to a label.\n\n\
                          - `plan_id_or_path`: either the 64-char plan id from `list_plans`, or \
                          a path to the plan file (absolute or repo-relative).\n\
                          - `label`: short identifier you pick for this reviewer in the \
                          timeline (e.g. `claude-architect`). Must be unique within the plan.\n\n\
                          After joining you can call `get_review_context` to read the plan and \
                          `add_feedback` to post critiques."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["plan_id_or_path", "label"],
                "properties": {
                    "plan_id_or_path": {"type": "string"},
                    "label": {"type": "string"}
                },
                "additionalProperties": false
            }),
        },
        ToolDescriptor {
            name: "get_review_context".to_string(),
            description: "Reviewer tool. Returns the artifact you should review, plus enough \
                          context to review it.\n\n\
                          - `plan_id`: the plan to fetch context for.\n\
                          - `target` (optional): \"plan\" returns the latest plan revision, \
                          \"implementation\" returns the latest registered commit metadata + \
                          diff. If omitted, defaults to whatever artifact is currently active \
                          for the plan's state.\n\n\
                          You must have called `register_plan_file` or `join_plan` first."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["plan_id"],
                "properties": {
                    "plan_id": {"type": "string"},
                    "target": {"type": "string", "enum": ["plan", "implementation"]}
                },
                "additionalProperties": false
            }),
        },
        ToolDescriptor {
            name: "add_feedback".to_string(),
            description: "Reviewer tool. Posts feedback against a specific artifact. The \
                          feedback is `pending` until the human stages it and clicks Deliver \
                          in the Trinity web UI.\n\n\
                          - `plan_id`: the plan this feedback belongs to.\n\
                          - `target_kind`: \"plan_revision\" for plan-stage feedback, or \
                          \"implementation_commit\" for impl-stage feedback. Trinity \
                          server-validates that the target exists and belongs to the plan.\n\
                          - `target_id`: the numeric `revision_id` (for plan_revision) or \
                          full commit SHA (for implementation_commit). You get these from \
                          `get_review_context`.\n\
                          - `text`: your critique. Markdown is fine.\n\n\
                          Reviewers cannot deliver their own feedback to the master, approve \
                          plans, or mark work done — those are human-only actions in the web UI."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["plan_id", "target_kind", "target_id", "text"],
                "properties": {
                    "plan_id": {"type": "string"},
                    "target_kind": {
                        "type": "string",
                        "enum": ["plan_revision", "implementation_commit"]
                    },
                    "target_id": {"type": "string"},
                    "text": {"type": "string"}
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
        "poll_directive" => master::poll_directive(state, req).await,
        "ack_directive" => master::ack_directive(state, req).await,
        "list_plans" => reviewer::list_plans(state, req).await,
        "join_plan" => reviewer::join_plan(state, req).await,
        "get_review_context" => reviewer::get_review_context(state, req).await,
        "add_feedback" => reviewer::add_feedback(state, req).await,
        other => Err(ToolError::NotFound(format!("unknown tool: {other}"))),
    }
}

async fn echo_cwd(_state: &AppState, req: &ToolCallRequest) -> Result<Value, ToolError> {
    Ok(json!({
        "cwd": req.cwd,
        "label": req.label,
    }))
}
