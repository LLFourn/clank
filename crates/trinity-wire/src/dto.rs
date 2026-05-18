//! Public response DTOs for Trinity's HTTP / MCP / SSE surfaces.
//!
//! Every DTO derives both `Serialize` and `Deserialize` so the
//! daemon (producer) and the frontend (consumer) round-trip
//! through one shared definition.
//!
//! All string-valued closed-vocabulary fields are typed enums from
//! [`crate::vocab`]. Validated newtype identifiers (commit SHAs,
//! plan keys, agent labels, repo basenames) surface here as
//! `String` — the daemon validates on parse, the frontend treats
//! them opaquely.
//!
//! Tagged enums use `#[serde(tag = "kind")]` so kind-dependent
//! response shapes have ONE discriminator in the type system AND
//! one on the wire. `CommitDetailResponse` uses
//! `#[serde(flatten)]` over the tagged `CommitDetail` enum so the
//! `kind` field is at the response root — see plan §"Tagged enums
//! where shape varies by kind".

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::vocab::{
    CommitGateState, CommitKind, DiffLineKind, ExpectedAction, PlanEventKind, PlanLifecycle,
    PlanTouchKind, PlanWorktreeStatus, Posture, RepoEventKind, ReviewTargetPhase, Verdict,
    WaitingReason, WaitingRole,
};

// ============================================================
// Composing structs
// ============================================================

/// One reviewer's verdict file (`.trinity/feedback/<stem>/<sha>/
/// <author>.md`) surfaced on the wire. `body_html` is sanitized
/// markdown; `body_raw` is included for the UI cards that render
/// the rendered HTML AND need access to the raw text (e.g. to
/// strip the verdict marker prefix).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Feedback {
    pub author: String,
    pub verdict: Verdict,
    /// Raw markdown body. May include the verdict marker prefix.
    pub body_raw: String,
    /// Sanitized HTML rendering of `body_raw`.
    pub body_html: String,
    /// Repo-relative path of the feedback file. For UI copy-link
    /// affordances.
    pub path: String,
    /// File mtime as unix seconds, for chronological sort when SHA
    /// + author alone don't establish order.
    pub created_at: i64,
}

/// One commit's review gate. Cumulative-participant set plus the
/// per-commit verdict breakdown plus each participant's feedback
/// body. Empty `feedback` map means no reviewer has posted yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitGate {
    pub state: CommitGateState,
    /// Every reviewer who has ever posted on any reviewable commit
    /// of this plan (cumulative).
    pub participants: Vec<String>,
    /// Approvers ON THIS COMMIT.
    pub approvers: Vec<String>,
    /// Requesters of changes ON THIS COMMIT.
    pub requesters: Vec<String>,
    /// Authors who posted Unmarked verdicts on this commit.
    pub ambiguous: Vec<String>,
    /// Participants who haven't voted on this commit yet.
    pub missing: Vec<String>,
    /// Feedback bodies on this commit, keyed by author.
    pub feedback: BTreeMap<String, Feedback>,
}

/// Per-commit row in `commits[]` on the rich plan-detail / context
/// response. `gate` is `Some` for reviewable kinds (PlanOnly,
/// CodeOnly, Mixed); `None` for non-reviewable kinds (MultiPlan,
/// Finalize).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitRow {
    pub sha: String,
    pub kind: CommitKind,
    pub gate: Option<CommitGate>,
    /// Verdict-summary list per author for this commit. Subset of
    /// `gate.feedback` projection; provided as a convenience for
    /// MCP consumers that don't want the full body.
    pub feedback: Vec<FeedbackSummary>,
}

/// `{ author, verdict }` summary — the compact form on the MCP
/// `commits[]` array. Full bodies are on the matching `CommitGate`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedbackSummary {
    pub author: String,
    pub verdict: Verdict,
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

/// Per-cycle summary surfaced in the plan-detail wire under
/// `archived_cycles`. One entry per freeze event (today: 0 or 1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchivedCycle {
    pub closer: String,
    pub approver_count: u32,
}

/// Approval snapshot file under `.trinity/finished/<stem>/<agent>.md`
/// at the freeze commit's tree. Surfaced on `CommitDetailResponse`
/// when the commit's kind is `Finalize`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinalizeApproval {
    /// Filename's `<author>` segment (without the `.md`).
    pub author: String,
    pub filename: String,
    /// Sanitized HTML rendering of the approval body.
    pub body_html: String,
}

/// What's blocking progress on a plan + who's responsible.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitingOn {
    pub role: WaitingRole,
    pub reason: WaitingReason,
    /// Specific agent names tied to the reason (e.g. the requesters
    /// for `AddressCommitChanges`). May be empty.
    pub agents: Vec<String>,
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
/// plan/impl-tagged wire shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewGate {
    /// Wire-string state — `ready` / `needs_review` /
    /// `changes_requested`. This is a legacy mapping of
    /// `CommitGateState`; daemon converts via
    /// `legacy_gate_state_wire`.
    pub state: String,
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrHintOption {
    pub name: String,
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
/// `diff_parser::parse_diff` produces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDiff {
    pub old_path: String,
    pub new_path: String,
    pub status: String,
    pub mode: String,
    pub binary: bool,
    #[serde(default)]
    pub always_folded: bool,
    #[serde(default)]
    pub hunks: Vec<DiffHunk>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffHunk {
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
    pub new_lines: u32,
    pub header: String,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub content: String,
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
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

/// One row in MCP `list_plans` (and, via a Phase 5 conversion,
/// `/api/plans`). The legacy `state` field is a duplicate of
/// `lifecycle` carried for wire back-compat through Phase 7;
/// Phase 8 drops it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanRow {
    pub repo: String,
    pub plan_id: Option<String>,
    pub slug: String,
    /// Legacy alias for `lifecycle` — same value, kept so the
    /// frontend's `state: String` reader keeps working until
    /// Phase 6. Phase 8 drops this field.
    pub state: PlanLifecycle,
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

/// MCP `get_context` response body. Rich per-plan view used to
/// drive the agent loop. Most fields are also present on
/// `PlanDetailResponse` (the UI's richer shape with plan body
/// included).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetContextResponse {
    pub repo: String,
    pub plan_id: Option<String>,
    pub slug: String,
    /// Legacy alias for `lifecycle`. Drop in Phase 8.
    pub state: PlanLifecycle,
    pub lifecycle: PlanLifecycle,
    pub current_path: String,
    pub phase: Posture,
    pub plan_worktree_status: PlanWorktreeStatus,
    pub waiting_on: WaitingOn,
    pub expected_action: ExpectedAction,
    pub review_target: Option<ReviewTarget>,
    pub write_feedback: Option<WriteFeedback>,
    pub review_gate: Option<ReviewGate>,
    pub latest_plan_revision: Option<CommitRef>,
    pub latest_implementation_revision: Option<CommitRef>,
    pub plan_revisions: Vec<String>,
    pub implementation_commits: Vec<String>,
    pub commits: Vec<CommitRow>,
    pub latest_relevant_commit: Option<String>,
    pub timeline: Vec<TimelineEvent>,
    pub pr_hint: Option<PrHint>,
    pub archived_cycles: Vec<ArchivedCycle>,
}

/// `/api/plan/{repo}/{stem_md}` response body — the UI's richer
/// per-plan shape. Adds the plan body markdown to `GetContextResponse`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanDetailResponse {
    pub repo: String,
    pub plan_id: Option<String>,
    pub slug: String,
    /// Legacy alias for `lifecycle`. Drop in Phase 8.
    pub state: PlanLifecycle,
    pub lifecycle: PlanLifecycle,
    pub current_path: String,
    pub phase: Posture,
    pub plan_worktree_status: PlanWorktreeStatus,
    pub waiting_on: WaitingOn,
    pub expected_action: ExpectedAction,
    pub review_target: Option<ReviewTarget>,
    pub review_gate: Option<ReviewGate>,
    pub latest_plan_revision: Option<CommitRef>,
    pub latest_implementation_revision: Option<CommitRef>,
    pub plan_revisions: Vec<String>,
    pub implementation_commits: Vec<String>,
    pub commits: Vec<CommitRow>,
    pub latest_relevant_commit: Option<String>,
    /// Sanitized HTML of the plan body. For `lifecycle == Finished`
    /// this is the body at the freeze commit (captured by the
    /// fold); for `Active` it's the body at HEAD.
    pub plan_body_html: String,
    /// Hint to the SPA on whether to render a see-more toggle.
    pub plan_body_truncated: bool,
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
/// Returns the plan-file body AT THAT SHA (not HEAD).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanRevisionResponse {
    pub repo: String,
    pub plan_id: String,
    pub slug: String,
    pub commit_sha: String,
    pub body_raw: String,
    pub body_html: String,
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
    pub path: String,
    pub plan_count: usize,
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

/// One SSE event. Tagged by `scope` so the wire's
/// (`repo` / `plan`)-shaped payload distinction is structural,
/// not conventional. Today the wire carries only the event kind
/// and identifying fields — Phase 7 of the purge plan adds typed
/// kind-dependent payloads via a flattened tagged enum on
/// `RepoEvent` / `PlanEvent`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "snake_case")]
pub enum LiveEvent {
    Repo(RepoEvent),
    Plan(PlanEvent),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoEvent {
    pub ts: i64,
    pub kind: RepoEventKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanEvent {
    pub ts: i64,
    pub plan_id: String,
    pub lifecycle: PlanLifecycle,
    pub kind: PlanEventKind,
}
