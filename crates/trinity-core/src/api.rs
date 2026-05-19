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

/// One row in `commits[]` — same shape for MCP and HTTP. Tagged
/// by `kind` on the wire; the daemon's `build_commits_array`
/// filter only emits rows for reviewable commit kinds so all
/// variants carry `gate` and `feedback` directly. Feedback bodies
/// are raw markdown — the wasm frontend renders at display time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CommitRow {
    PlanOnly {
        sha: String,
        gate: CommitGate,
        feedback: Vec<Feedback>,
    },
    CodeOnly {
        sha: String,
        gate: CommitGate,
        feedback: Vec<Feedback>,
    },
    Mixed {
        sha: String,
        gate: CommitGate,
        feedback: Vec<Feedback>,
    },
}

impl CommitRow {
    pub fn sha(&self) -> &str {
        match self {
            CommitRow::PlanOnly { sha, .. }
            | CommitRow::CodeOnly { sha, .. }
            | CommitRow::Mixed { sha, .. } => sha,
        }
    }

    pub fn kind(&self) -> CommitKind {
        match self {
            CommitRow::PlanOnly { .. } => CommitKind::PlanOnly,
            CommitRow::CodeOnly { .. } => CommitKind::CodeOnly,
            CommitRow::Mixed { .. } => CommitKind::Mixed,
        }
    }

    pub fn gate(&self) -> &CommitGate {
        match self {
            CommitRow::PlanOnly { gate, .. }
            | CommitRow::CodeOnly { gate, .. }
            | CommitRow::Mixed { gate, .. } => gate,
        }
    }

    pub fn feedback(&self) -> &[Feedback] {
        match self {
            CommitRow::PlanOnly { feedback, .. }
            | CommitRow::CodeOnly { feedback, .. }
            | CommitRow::Mixed { feedback, .. } => feedback,
        }
    }
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
    /// The work prefix — same shape as `wait_for_work`'s happy
    /// path. `plan_id`, `repo`, and the action's fields surface
    /// at the top level via `#[serde(flatten)]`.
    #[serde(flatten)]
    pub work: WorkPayload,
    pub current_path: String,
    pub lifecycle: PlanLifecycle,
    pub phase: Posture,
    pub plan_worktree_status: PlanWorktreeStatus,
    pub waiting_on: WaitingOn,
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
    /// `plan_file` is what the reviewer reads to compose their
    /// verdict (master wrote it). On `wait_for_work` the
    /// `plan_file.content` is populated opportunistically on
    /// first encounter; on `work_context` it's always omitted.
    WriteFeedback {
        path: String,
        target_sha: String,
        plan_file: PlanFile,
    },
    /// Master: address the request-changes / unmarked feedback in
    /// `reviews` against `target_sha` and follow up with a fix
    /// commit. Each review carries the author, verdict, and
    /// opportunistic `content` (populated on first encounter by
    /// `wait_for_work`; absent on `work_context`).
    /// `plan_path` is `Some` when the RC is plan-side (commit
    /// kind `PlanOnly | Mixed`) — meaning the master needs to
    /// revise the plan body as part of the fix-up commit.
    /// `None` for code-side RCs (`CodeOnly`).
    AddressChanges {
        target_sha: String,
        reviews: Vec<CurrentReview>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        plan_path: Option<String>,
    },
    /// Master: the plan file at `plan_path` is dirty in the
    /// worktree. Commit the revision to release blocked reviews.
    CommitPlanRevision { plan_path: String },
    /// Master: latest commit at `previous_commit` is approved.
    /// `plan_path` is the plan file (provided so the agent has
    /// the plan body without reconstructing it from `plan_id`).
    /// Write the next implementation commit.
    StartImplementation {
        previous_commit: String,
        plan_path: String,
    },
    /// Plan is finalized; no further action.
    SessionFinished,
}

/// A plan file the caller may want to read. `content` is
/// populated opportunistically on `wait_for_work` first
/// encounter (per `(canonical-repo-root, agent, path)` keyed by
/// content hash); omitted on subsequent polls. `work_context`
/// never populates it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanFile {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

/// One reviewer's feedback at the current review target.
/// `target_sha` is implicit (it's `AddressChanges.target_sha`).
/// `content` follows the same opportunistic rule as
/// `PlanFile.content`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CurrentReview {
    pub path: String,
    pub author: crate::ids::AgentLabel,
    pub verdict: Verdict,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

/// A review against a superseded commit, surfaced to the master
/// exactly once. Carries its own `target_sha` (different from
/// the current target). `content` is `None` only when the file
/// exceeds the inline cap — in that case the metadata still
/// reaches the master and the entry is marked seen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StaleReview {
    pub path: String,
    pub author: crate::ids::AgentLabel,
    pub verdict: Verdict,
    pub target_sha: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
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

/// `finish_preview` response — everything `trinity finish` needs to
/// validate before sealing. Returned by
/// `GET /api/plan/{repo}/{stem}.md/finish_preview`.
///
/// **The CLI consumes `readiness` and nothing else for the
/// finalize/abort decision.** Raw fields (`gate_state`,
/// `plan_worktree_status`, …) are exposed for human display only
/// (e.g. `trinity finish --dry`) and must not be recombined into a
/// CLI-side ready/blocked decision. Projecting the readiness once
/// in the daemon keeps the CLI from disagreeing with itself across
/// invocations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinishPreviewResponse {
    pub plan_id: String,
    pub plan_path: String,
    /// The daemon's typed finalize decision. Single source of truth
    /// for "can `trinity finish` proceed?"
    pub readiness: FinalizeReadiness,
    /// Raw current gate. For display only.
    pub gate_state: crate::vocab::CommitGateState,
    /// SHA of the latest reviewable commit. For display only.
    /// `None` when the plan has no reviewable commits yet.
    pub latest_reviewable_sha: Option<crate::ids::CommitSha>,
    /// Worktree status of the plan file. For display only.
    pub plan_worktree_status: crate::vocab::PlanWorktreeStatus,
    /// True if the plan has already been finalized. For display
    /// only; `readiness == AlreadyFinished` carries the same info.
    pub is_finished: bool,
    /// Exact approval files the CLI is allowed to seal when
    /// `readiness == Ready`. Each entry's `body_hash` was computed
    /// from `feedback.body` (the daemon's UTF-8 in-memory copy,
    /// loaded via `std::fs::read_to_string`). The CLI must re-read
    /// each `source_path` via `std::fs::read_to_string` and re-hash
    /// the body before copying — any drift aborts the seal.
    pub sealed_approvals: Vec<SealedApproval>,
}

/// The daemon's typed finalize decision. The CLI dispatches on this
/// and only this for "can we proceed?"
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FinalizeReadiness {
    /// All pre-flights pass; the CLI may seal `sealed_approvals`
    /// and commit.
    Ready,
    /// Plan is already finalized — `trinity finish` exits 0 with
    /// "already finished" without writing anything.
    AlreadyFinished,
    /// One or more pre-flights failed. Each reason is independent
    /// human-readable + machine-tagged so the CLI can report all
    /// of them at once rather than fail-fast.
    Blocked { reasons: Vec<FinalizeBlockReason> },
}

/// One specific reason `trinity finish` cannot proceed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FinalizeBlockReason {
    /// No reviewable commit attributed to the plan yet.
    NoReviewableCommit,
    /// Gate is not `Approved` (still unreviewed, or has RC/unmarked).
    GateNotApproved {
        state: crate::vocab::CommitGateState,
    },
    /// Plan file is missing from the worktree.
    PlanFileMissing,
    /// Plan file's worktree body differs from HEAD's blob.
    PlanFileDirty,
}

/// One approving feedback the daemon has projected as part of the
/// approved gate. The CLI re-reads `source_path`, hashes the body,
/// and aborts if `body_hash` doesn't match — that catches feedback
/// edits between preview and commit.
///
/// The daemon's `body_hash` is computed via `content_hash` over the
/// UTF-8 bytes of `feedback.body`, which was loaded from disk via
/// `std::fs::read_to_string`. The CLI MUST use the same load path
/// (`read_to_string`) so the comparison is byte-for-byte; reading
/// raw bytes would diverge on invalid UTF-8 (which the daemon
/// would have rejected anyway).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SealedApproval {
    pub author: crate::ids::AgentLabel,
    /// Repo-relative path: `.trinity/feedback/<stem>/<sha>/<author>.md`.
    pub source_path: String,
    /// Hash of the body the daemon projected.
    pub body_hash: crate::ids::ContentHash,
}

/// `rewrite_preview` response — the typed manifest `trinity purge`
/// and `trinity finish --squash`/`--purge` execute. Returned by
/// `GET /api/plan/{repo}/{stem}.md/rewrite_preview`.
///
/// The CLI is a thin executor of this manifest. The classification
/// rules (drop/keep_verbatim/rewrite, foreign-commit definition,
/// what counts as a plan-`.trinity/` path) live in the daemon's
/// projection — the CLI walks `commits` in order and applies the
/// per-commit disposition without re-deriving Trinity attribution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RewritePreviewResponse {
    pub plan_id: String,
    pub plan_stem: crate::ids::PlanKey,
    /// The plan's intro commit — the earliest sha attributed to the
    /// plan. `None` only for plans with no commits yet (which the
    /// CLI should refuse before reaching this endpoint).
    pub intro_sha: Option<crate::ids::CommitSha>,
    pub head_sha: crate::ids::CommitSha,
    /// True if the range `[intro_sha, head_sha]` is first-parent
    /// linear (no merge commits). False means the engine refuses
    /// to rewrite — merge-tree rewriting is out of scope.
    pub linear: bool,
    /// In chronological order from `intro_sha` to `head_sha`
    /// (inclusive). Empty when `intro_sha` is `None`.
    pub commits: Vec<RewriteCommit>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RewriteCommit {
    pub sha: crate::ids::CommitSha,
    pub subject: String,
    pub disposition: RewriteDisposition,
    /// True if the commit is NOT attributed to this plan (another
    /// plan's commit, unattributed, or cross-plan mixed). `--squash`
    /// refuses cleanly when this is true for any commit in the
    /// range; `--purge` handles it.
    pub foreign: bool,
    /// Repo-relative paths to strip from the rewritten tree for
    /// this commit. Empty for `Drop` (the commit goes away anyway)
    /// and `KeepVerbatim` (we don't change the tree). Non-empty
    /// only for `Rewrite`.
    pub strip_paths: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RewriteDisposition {
    /// Commit touched ONLY this plan's `.trinity/` paths. The
    /// rewrite engine skips it entirely; the parent chain hops over.
    Drop,
    /// Commit didn't touch any of this plan's `.trinity/` paths.
    /// Reuse the SHA verbatim in the rewritten chain.
    KeepVerbatim,
    /// Commit touched this plan's `.trinity/` paths AND other
    /// paths. The engine builds a new tree omitting
    /// `strip_paths`, then commit-tree's it with the original
    /// author/message/timestamp.
    Rewrite,
}

/// Response shape for the all-plans purge preview. Returned by
/// `GET /api/repos/{basename}/rewrite_preview_all`.
///
/// Same `commits` shape as `RewritePreviewResponse` so the CLI's
/// rewrite engine consumes one type. `foreign` is always false in
/// the all-plans case (the "is this commit's attribution mine"
/// question doesn't apply when "mine" is "every plan").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PurgeAllPreviewResponse {
    pub repo: String,
    pub head_sha: crate::ids::CommitSha,
    pub linear: bool,
    /// Earliest commit in the first-parent walk that touched ANY
    /// `.trinity/` path. `None` when no commit ever touched
    /// `.trinity/` — the CLI short-circuits with "nothing to purge"
    /// in that case.
    pub intro_sha: Option<crate::ids::CommitSha>,
    /// Distinct plan stems whose plan/finalize artifacts appear
    /// anywhere in the walk. Sorted, deduped. For the
    /// confirmation prompt's "N plans touched" rendering — NOT
    /// the source of truth for what gets stripped (that's per-
    /// commit `strip_paths`).
    pub plans_touched: Vec<crate::ids::PlanKey>,
    pub commits: Vec<RewriteCommit>,
}

// ============================================================
// wait_for_work response
// ============================================================

/// `wait_for_work` response: either work to do or a timeout
/// notice. Untagged so the wire's field-presence
/// discriminator (`kind` vs `timed_out`) keeps the existing
/// consumer shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum WaitForWorkResponse {
    Work(WaitWorkPayload),
    Timeout(WaitTimeout),
}

/// `wait_for_work` happy-path payload. Wraps the projection
/// `WorkPayload` (the shape `work_context` also embeds) with a
/// WFW-only master sidecar `stale_reviews` carrying any unseen
/// reviews against superseded targets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitWorkPayload {
    #[serde(flatten)]
    pub work: WorkPayload,
    /// Master-only sidecar: reviews against superseded commits
    /// the agent hasn't been sent yet. Riding-along delivery —
    /// the sidecar attaches to any legitimate master wakeup;
    /// it does NOT independently wake master WFW. Empty on
    /// reviewer variants and after every entry's been emitted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stale_reviews: Vec<StaleReview>,
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

/// The work to do, with the surrounding identity. Used flat on
/// `wait_for_work` (as `WaitForWorkResponse::Work(WorkPayload)`)
/// and flatten-embedded into `WorkContextResponse`. The
/// work-prefix of `work_context`'s response is the same shape
/// as `wait_for_work`'s happy path — both surfaces share the
/// same projection (`responses::build_work_payload`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkPayload {
    pub plan_id: String,
    pub repo: String,
    #[serde(flatten)]
    pub action: ExpectedAction,
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
