//! Public response DTOs for Trinity's HTTP / MCP / SSE surfaces.
//!
//! Every DTO derives both `Serialize` and `Deserialize` so the
//! daemon (producer) and the frontend (consumer) round-trip
//! through one shared definition.
//!
//! Closed-vocabulary fields use typed enums from [`crate::vocab`].
//! Shared `model`/`api` structs (`Feedback`, `CommitGate`, `Plan`,
//! `PlanTimelineEvent`, `WaitingOn`, `ArchivedCycle`) reach for
//! `crate::ids` newtypes (`AgentLabel`, `CommitSha`, etc.) where
//! they OWN validated identity — serde-transparent over `String`,
//! so the wire form is unchanged from "plain string" but both
//! ends validate on parse / deserialize.
//!
//! Many endpoint-only fields remain plain `String` because they
//! are opaque to the daemon (commit SHAs received as URL path
//! segments, repo paths echoed back as-is, etc.). The rule is
//! "type it as `crate::ids` when this struct OWNS the identity
//! invariant; leave it as `String` when it's just a wire-level
//! echo."
//!
//! Tagged enums use `#[serde(tag = "kind")]` so kind-dependent
//! response shapes have ONE discriminator in the type system AND
//! one on the wire. `CommitDetailResponse` uses
//! `#[serde(flatten)]` over the tagged `CommitDetail` enum so the
//! `kind` field is at the response root — see plan §"Tagged enums
//! where shape varies by kind".

use serde::{Deserialize, Serialize};

use crate::vocab::{
    CommitKind, PlanLifecycle, PlanTouchKind, PlanWorktreeStatus, Posture, ReviewGateState,
    ReviewTargetPhase, Verdict, WaitingReason, WaitingRole,
};

// ============================================================
// Composing structs
// ============================================================

/// Wire representation of a feedback file. Same type as
/// [`crate::model::Feedback`] — daemon storage and wire response
/// carry one struct. The wasm frontend renders `body` (raw
/// markdown) to HTML at display time; no `body_html` field
/// crosses the wire.
pub use crate::model::Feedback;

/// Wire representation of a commit's review gate. Same type as
/// [`crate::model::CommitGate`] now that `Feedback` is unified —
/// daemon storage and wire response carry one struct.
pub use crate::model::CommitGate;

/// One row in `commits[]` — same shape for MCP and HTTP. Carries
/// the per-author feedback bodies (raw markdown — the wasm
/// frontend renders to HTML at display time) so a single response
/// covers both agent and SPA consumers. `gate` is `Some` for
/// reviewable kinds (PlanOnly, CodeOnly, Mixed); `None` for
/// non-reviewable kinds (MultiPlan, Finalize).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitRow {
    pub sha: String,
    pub kind: CommitKind,
    pub gate: Option<CommitGate>,
    pub feedback: Vec<Feedback>,
}

/// One row in a plan's per-session timeline. Kind-dependent shape
/// modeled as a `#[serde(tag = "kind")]` enum — frontend matches
/// exhaustively.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TimelineEvent {
    /// Pure plan-file revision (or intro). `plan_touch`
    /// distinguishes intro vs revision.
    CommitPlan {
        sha: String,
        subject: String,
        plan_touch: PlanTouchKind,
    },
    /// Implementation commit attributed to this plan via
    /// walk-back.
    CommitImpl { sha: String, subject: String },
    /// Plan revision AND code change in one commit.
    CommitMixed {
        sha: String,
        subject: String,
        plan_touch: PlanTouchKind,
    },
    /// Commit touched multiple plan files. Non-reviewable.
    CommitMultiPlan {
        sha: String,
        subject: String,
        plan_touch: PlanTouchKind,
    },
    /// Lifecycle commit that froze the plan. Non-reviewable;
    /// approvals are sealed in `.trinity/finished/<stem>/` at this
    /// commit's tree.
    CommitFinalize { sha: String, subject: String },
    /// One reviewer's verdict against a specific commit.
    Review {
        target: String,
        author: String,
        verdict: Verdict,
        phase: ReviewTargetPhase,
        #[serde(default)]
        created_at: i64,
    },
}

// `ArchivedCycle` lives in [`crate::model`] because daemon storage
// (`model::Plan.archived_cycles`) holds it directly. Response
// shapes here re-export it at use sites.
pub use crate::model::ArchivedCycle;

/// Approval snapshot file under `.trinity/finished/<stem>/<agent>.md`
/// at the freeze commit's tree. Surfaced on `CommitDetailResponse`
/// when the commit's kind is `Finalize`. `body` is raw markdown;
/// the wasm frontend renders to HTML at display time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinalizeApproval {
    /// Filename's `<author>` segment (without the `.md`).
    pub author: String,
    pub filename: String,
    pub body: String,
}

/// What's blocking progress on a plan + who's responsible.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitingOn {
    pub role: WaitingRole,
    pub reason: WaitingReason,
    /// Specific agent names tied to the reason (e.g. the requesters
    /// for `AddressCommitChanges`). May be empty. Newtype is
    /// `#[serde(transparent)]` so the wire form is a JSON string
    /// array — identical to the prior `Vec<String>` shape — while
    /// the daemon retains parse-time validation on every value.
    pub agents: Vec<crate::ids::AgentLabel>,
    /// Human-readable explanation. Daemon-side text; the wire crate
    /// just carries the string.
    pub description: String,
}

/// The single canonical review target: SHA + which side (plan vs
/// impl) it represents. Both fields are present together or absent
/// together — `Option<ReviewTarget>` on the parent response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewTarget {
    pub commit_sha: String,
    pub phase: ReviewTargetPhase,
}

/// Canonical write path for the next reviewer feedback file +
/// which side it targets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteFeedback {
    pub phase: ReviewTargetPhase,
    pub target_sha: String,
    /// Repo-relative path the reviewer should write to.
    pub path: String,
}

/// Plan + impl review gates pre-projected for the legacy
/// plan/impl-tagged wire shape. The daemon constructs this from a
/// `CommitGateState` via `ReviewGateState::from(...)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewGate {
    pub state: ReviewGateState,
    pub phase: ReviewTargetPhase,
    pub participants: Vec<String>,
    pub approvals: Vec<String>,
    pub request_changes: Vec<String>,
    pub missing_approvals: Vec<String>,
}

/// `{ commit_sha }` reference — used for `latest_plan_revision`,
/// `latest_implementation_revision`, `latest_implementation_revision`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitRef {
    pub commit_sha: String,
}

/// Suggested squash command alternatives for the PR landing flow.
/// The `kind` discriminator is a closed vocabulary (see
/// [`PrHintOptionKind`]); the frontend matches on it exhaustively
/// to choose the option's display label.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrHintOption {
    pub kind: crate::vocab::PrHintOptionKind,
    pub base: String,
    pub command: String,
}

/// PR landing hint for the implementing phase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrHint {
    pub plan_intro: String,
    pub plan_intro_parent: Option<String>,
    pub implementation_commits: Vec<String>,
    pub options: Vec<PrHintOption>,
    pub suggested_message: String,
}

/// One file's diff inside a structured-diff payload. Mirrors what
/// the daemon's `diff_parser::parse_diff` produces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDiff {
    pub path: String,
    #[serde(default)]
    pub old_path: Option<String>,
    pub additions: usize,
    pub deletions: usize,
    pub mode: FileDiffMode,
    pub binary: bool,
    pub always_folded: bool,
    pub hunks: Vec<DiffHunk>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileDiffMode {
    Added,
    Removed,
    Renamed,
    Modified,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffHunk {
    pub header: String,
    pub lines: Vec<DiffLine>,
}

/// One line inside a `DiffHunk`. Tagged by `kind` on the wire;
/// each variant carries exactly the lineno fields valid for it
/// — no `null` placeholders.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DiffLine {
    Insert {
        content: String,
        new_lineno: usize,
    },
    Delete {
        content: String,
        old_lineno: usize,
    },
    Context {
        content: String,
        old_lineno: usize,
        new_lineno: usize,
    },
    /// Hunk-header / file-header line (`@@ ... @@`, `+++ ...`,
    /// etc.). No lineno; content is the raw header text.
    Meta {
        content: String,
    },
}

/// One conflict row: a plan stem with multiple paths on disk.
/// Surfaced in list/index responses so the operator can resolve
/// before the plan routes any work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanConflict {
    pub plan_id: Option<String>,
    pub slug: String,
    pub paths: Vec<String>,
}

// ============================================================
// Top-level response DTOs
// ============================================================

/// One row in MCP `list_plans` and HTTP `/api/plans`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanRow {
    pub repo: String,
    pub plan_id: Option<String>,
    pub slug: String,
    pub lifecycle: PlanLifecycle,
    pub current_path: String,
    pub phase: Posture,
    pub plan_worktree_status: PlanWorktreeStatus,
    pub waiting_on: WaitingOn,
    #[serde(default)]
    pub archived_cycles: Vec<ArchivedCycle>,
    /// Powers the homepage sort.
    #[serde(default)]
    pub last_activity_ts: i64,
}

/// MCP `list_plans` + HTTP `/api/plans` response body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListPlansResponse {
    pub plans: Vec<PlanRow>,
    pub conflicts: Vec<PlanConflict>,
}

/// MCP `work_context` response body. The narrow coordination
/// view: just enough to act on the latest `wait_for_work` result.
///
/// Anything that needs the full per-plan fold (timeline, all
/// commits with feedback bodies, archived cycles, plan body
/// markdown, PR hints) reads HTTP `/api/plan/<id>` instead. MCP
/// coordinates work; HTTP transports content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkContextResponse {
    pub plan_id: String,
    pub repo: String,
    pub current_path: String,
    pub lifecycle: PlanLifecycle,
    pub phase: Posture,
    pub plan_worktree_status: PlanWorktreeStatus,
    pub waiting_on: WaitingOn,
    pub expected_action: ExpectedAction,
}

/// The action the caller should take next, tagged-enum form.
/// Each variant carries exactly the data needed to perform it —
/// reviewers get the canonical write path directly instead of
/// reconstructing it from sha + author_label; masters get the
/// RC paths to read directly instead of walking the gate.
///
/// Serialized as `{ "kind": "<variant>", <payload> }`. Variant
/// names are imperative ("write the feedback", "commit the
/// revision") — they're directives, not states. The state
/// half lives on `WaitingOn` (role / reason / description /
/// agents).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExpectedAction {
    /// Reviewer: write an `APPROVE\n...` / `REQUEST_CHANGES\n...`
    /// markdown file to `path` against the commit at `target_sha`.
    WriteFeedback { path: String, target_sha: String },
    /// Master: address the request-changes feedback at `rc_paths`
    /// against `target_sha` and follow up with a fix commit.
    AddressChanges {
        target_sha: String,
        rc_paths: Vec<String>,
    },
    /// Master: the plan file is dirty in the worktree. Commit
    /// the revision (at `WorkContextResponse.current_path`) to
    /// release blocked reviews.
    CommitPlanRevision,
    /// Master: latest commit at `previous_commit` is approved.
    /// Write the next implementation commit.
    StartImplementation { previous_commit: String },
    /// Plan is finalized; no further action.
    SessionFinished,
}

/// MCP `set_active_work` response. The selection is recorded in
/// the runtime's in-memory selection map; the plan_id echo
/// confirms which plan the daemon understood.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetActiveWorkResponse {
    pub ok: bool,
    pub plan_id: String,
}

/// MCP `watch_repo` outcome. `Registered` means the daemon
/// learned about this repo as a result of the call; `AlreadyWatching`
/// is the idempotent no-op when the repo is already in the watched
/// set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WatchRepoStatus {
    Registered,
    AlreadyWatching,
}

/// MCP `watch_repo` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchRepoResponse {
    pub repo: String,
    pub basename: String,
    pub status: WatchRepoStatus,
}

/// MCP `clear_active_work` response. Idempotent — `ok: true`
/// regardless of whether a selection was present.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClearActiveWorkResponse {
    pub ok: bool,
}

/// `/api/plan/{repo}/{stem_md}` response body — the UI's richer
/// per-plan shape with full timeline, commits, archived cycles,
/// and the plan body markdown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanDetailResponse {
    pub repo: String,
    pub plan_id: Option<String>,
    pub slug: String,
    pub lifecycle: PlanLifecycle,
    pub current_path: String,
    pub phase: Posture,
    pub plan_worktree_status: PlanWorktreeStatus,
    pub waiting_on: WaitingOn,
    pub review_target: Option<ReviewTarget>,
    pub review_gate: Option<ReviewGate>,
    pub latest_plan_revision: Option<CommitRef>,
    pub latest_implementation_revision: Option<CommitRef>,
    pub plan_revisions: Vec<String>,
    pub implementation_commits: Vec<String>,
    pub commits: Vec<CommitRow>,
    pub latest_relevant_commit: Option<String>,
    /// Raw plan-file markdown. For `lifecycle == Finished` this is
    /// the body at the freeze commit (captured by the fold); for
    /// `Active` it's the body at HEAD. The wasm frontend renders
    /// to HTML and decides truncation locally — neither the
    /// rendered HTML nor a truncation flag cross the wire.
    pub plan_body: String,
    pub timeline: Vec<TimelineEvent>,
    pub pr_hint: Option<PrHint>,
    pub archived_cycles: Vec<ArchivedCycle>,
}

/// `/api/plan/{repo}/{stem_md}/commit/{sha}` response body. Uses
/// `#[serde(flatten)]` over `CommitDetail` so the wire has ONE
/// `kind` discriminator + variant-specific fields at the top level.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitDetailResponse {
    pub repo: String,
    pub plan_id: String,
    pub slug: String,
    pub commit_sha: String,
    pub subject: String,
    pub message_body: String,
    pub diff_files: Vec<FileDiff>,
    #[serde(flatten)]
    pub detail: CommitDetail,
}

/// Kind-dependent payload for `CommitDetailResponse`. `Finalize`
/// carries the approval snapshot at the freeze commit; reviewable
/// kinds carry live feedback. `kind` is the wire discriminator —
/// `#[serde(flatten)]` hoists it to the response root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CommitDetail {
    PlanOnly { feedback: Vec<Feedback> },
    CodeOnly { feedback: Vec<Feedback> },
    Mixed { feedback: Vec<Feedback> },
    MultiPlan {},
    Finalize { snapshot: Vec<FinalizeApproval> },
}

/// `/api/plan/{repo}/{stem_md}/revision/{sha}` response body.
/// Returns the plan-file body AT THAT SHA (not HEAD). Raw markdown
/// — the wasm frontend renders to HTML at display time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanRevisionResponse {
    pub repo: String,
    pub plan_id: String,
    pub slug: String,
    pub commit_sha: String,
    pub body: String,
    pub plan_intro: String,
    pub plan_intro_parent: Option<String>,
    pub previous_sha: Option<String>,
    pub next_sha: Option<String>,
    pub feedback: Vec<Feedback>,
}

/// `/api/plan/{repo}/{stem_md}/diff/{from}/{to}` response body —
/// patch between two plan-file blobs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffResponse {
    pub from: String,
    pub to: String,
    pub from_path: String,
    pub to_path: String,
    pub diff_files: Vec<FileDiff>,
}

/// One row in `/api/repos`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoRow {
    pub basename: String,
    pub root: String,
    pub plan_count: usize,
    pub last_activity_ts: i64,
}

/// `/api/repos` response body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoListResponse {
    pub repos: Vec<RepoRow>,
}

/// Outcome of `DELETE /api/repos/{basename}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteRepoOutcome {
    /// True if the repo was removed from in-memory state.
    pub ok: bool,
    pub basename: String,
    /// Number of plans dropped along with the repo.
    pub plan_count: usize,
    /// Set when in-memory removal succeeded but writing the
    /// `.trinity-watched` registry file failed. The repo will
    /// reappear on daemon restart until the operator fixes the
    /// file.
    pub registry_write_error: Option<String>,
}

// ============================================================
// wait_for_work response
// ============================================================

/// `wait_for_work` response: either work to do or a timeout
/// notice. Untagged so the wire keeps the field-presence
/// discriminator (`work` vs `timed_out`) the existing consumers
/// depend on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum WaitForWorkResponse {
    Work(WorkPayload),
    Timeout(WaitTimeout),
}

/// Typed MCP-tool error payload. Each variant lifts one of the
/// previously ad-hoc `{"error": "...", ...}` dynamic-JSON shapes
/// in `server::mcp` so a rename of a variant fails compilation at
/// the producer AND any structured consumer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "error", rename_all = "snake_case")]
pub enum McpErrorPayload {
    /// `wait_for_work` / `work_context`: caller passed `plan_id`
    /// omitted (or blank) and the daemon couldn't infer a single
    /// active plan in the scoped repo.
    NoActivePlan { repo: String, message: String },
    /// Plan-id inference found multiple active plans. Each
    /// candidate carries the shape the historical JSON emitted.
    AmbiguousPlan {
        message: String,
        candidates: Vec<PlanCandidate>,
    },
    /// `plan_id` referred to a basename Trinity isn't watching.
    UnknownRepo { basename: String },
    /// Two `.trinity/plans/` files with the same stem; daemon
    /// refuses to route work until the operator resolves.
    PlanConflict { slug: String, paths: Vec<String> },
    /// `plan_id` matches a watched repo but the plan file hasn't
    /// been committed yet (still untracked / staged).
    PlanNotCommitted {
        plan_id: String,
        slug: String,
        next_step: String,
    },
    /// `set_active_work` rejected because the target plan is
    /// already finalized — selecting a frozen plan would make WFW
    /// route to a terminal state every call.
    PlanNotActive { plan_id: String, message: String },
    /// `set_active_work` rejected because the plan's worktree file
    /// is missing (the plan is hidden via `Plan::is_visible`).
    /// Restore or commit the deletion before selecting.
    PlanHidden { plan_id: String, message: String },
}

/// One row in `AmbiguousPlan.candidates`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanCandidate {
    pub plan_id: String,
    pub current_path: String,
    pub lifecycle: PlanLifecycle,
}

/// `start_plan` happy-path response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartPlanResponse {
    pub plan_id: String,
    pub repo: String,
    pub canonical_path: String,
    pub slug: String,
    pub committed: bool,
    pub next_step: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitTimeout {
    pub timed_out: bool,
    /// Set on the timeout path that fires when MCP `wait_for_work`
    /// inference finds zero active plans. Omitted when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_active_plans: bool,
    /// Repo that the inference scoped to when `no_active_plans`
    /// fires. Omitted on the regular-timeout path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
}

/// One work assignment from `wait_for_work`. Action-specific fields
/// live on the flattened [`WorkAction`] variant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkPayload {
    pub plan_id: String,
    pub repo: String,
    pub locations: Vec<String>,
    #[serde(flatten)]
    pub action: WorkAction,
}

/// Tagged by the wire `work` discriminator. Variants that carry a
/// target SHA also carry `commit_kind` (`CommitKind` enum) and
/// `prompt_hint`. `SessionFinished` is the only no-target variant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "work", rename_all = "snake_case")]
pub enum WorkAction {
    ReviewCommit {
        target_sha: String,
        commit_kind: CommitKind,
        prompt_hint: String,
    },
    AddressCommitChanges {
        target_sha: String,
        commit_kind: CommitKind,
        prompt_hint: String,
    },
    CommitPlanRevision {
        target_sha: String,
        commit_kind: CommitKind,
        prompt_hint: String,
    },
    StartImplementation {
        target_sha: String,
        commit_kind: CommitKind,
        prompt_hint: String,
    },
    SessionFinished,
}

// ============================================================
// SSE live events
// ============================================================

/// One SSE event. Tagged by `scope` so the (`repo` / `plan`)-shaped
/// distinction is structural, not conventional. The kind-dependent
/// payload is carried by a flattened tagged enum on `RepoEvent` /
/// `PlanEvent` — `kind` is the discriminator, and any kind-specific
/// fields appear alongside the common ones on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "snake_case")]
pub enum LiveEvent {
    Repo(RepoEvent),
    Plan(PlanEvent),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoEvent {
    pub ts: i64,
    #[serde(flatten)]
    pub payload: RepoEventPayload,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RepoEventPayload {
    RepoRebuilt {},
    RepoUnwatched { plan_count: usize },
}

impl std::fmt::Display for RepoEventPayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            RepoEventPayload::RepoRebuilt {} => "repo_rebuilt",
            RepoEventPayload::RepoUnwatched { .. } => "repo_unwatched",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanEvent {
    pub ts: i64,
    pub plan_id: String,
    pub lifecycle: PlanLifecycle,
    #[serde(flatten)]
    pub payload: PlanEventPayload,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PlanEventPayload {
    PlanWorktreeChanged { path: String },
    FeedbackChanged {},
    FeedbackRemoved {},
}

impl std::fmt::Display for PlanEventPayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            PlanEventPayload::PlanWorktreeChanged { .. } => "plan_worktree_changed",
            PlanEventPayload::FeedbackChanged {} => "feedback_changed",
            PlanEventPayload::FeedbackRemoved {} => "feedback_removed",
        })
    }
}
