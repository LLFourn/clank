//! Closed-vocabulary enums shared between the Trinity daemon and
//! the Leptos frontend.
//!
//! Every enum here is a small fixed set that BOTH sides branch on.
//! Wire form is snake_case (via `#[serde(rename_all = "snake_case")]`).
//! `as_str(self) -> &'static str` returns the wire string for log
//! / path / display use without round-tripping through serde.
//!
//! Pure semantics methods (no daemon types in the signature) live
//! here. Daemon-dependent helpers (e.g. `PlanLifecycle::from_plan(&Plan)`)
//! become methods on the daemon-side struct that owns the input.

use serde::{Deserialize, Serialize};

/// Implement `std::fmt::Display` for an enum by delegating to its
/// `as_str(self) -> &'static str` inherent method. The wire string
/// is the right Display form — log lines, format strings, and SPA
/// `{enum}` interpolation all see the same snake_case text serde
/// produces.
macro_rules! impl_display_via_as_str {
    ($($ty:ty),+ $(,)?) => {
        $(
            impl ::core::fmt::Display for $ty {
                fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                    f.write_str(self.as_str())
                }
            }
        )+
    };
}

// ============================================================
// Plan lifecycle / posture / worktree status
// ============================================================

/// Plan lifecycle: `active` while no `Finalize` event has landed,
/// `finished` once one has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanLifecycle {
    Active,
    Finished,
}

impl PlanLifecycle {
    pub fn as_str(self) -> &'static str {
        match self {
            PlanLifecycle::Active => "active",
            PlanLifecycle::Finished => "finished",
        }
    }
}

/// Activity posture for an active plan, derived from the latest
/// reviewable commit's `CommitKind` — `PlanOnly | Mixed → Planning`,
/// `CodeOnly → Implementing`. Surfaced as `phase` on the wire for
/// back-compat.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Posture {
    Planning,
    Implementing,
}

impl Posture {
    pub fn as_str(self) -> &'static str {
        match self {
            Posture::Planning => "planning",
            Posture::Implementing => "implementing",
        }
    }
}

/// Working-tree state of a plan's plan file relative to HEAD's
/// blob. Recomputed at read time — never stored on `Plan`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanWorktreeStatus {
    /// Working-tree body hash matches HEAD's blob hash.
    #[default]
    Clean,
    /// File exists at the active path with a different body.
    BodyDirty,
    /// Plan file is missing from the working tree but still present
    /// in HEAD. Hidden-from-surfaces when the plan is unfrozen — see
    /// `Plan::is_visible` on the daemon side.
    PlanFileMissing,
}

impl PlanWorktreeStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            PlanWorktreeStatus::Clean => "clean",
            PlanWorktreeStatus::BodyDirty => "body_dirty",
            PlanWorktreeStatus::PlanFileMissing => "plan_file_missing",
        }
    }
}

// ============================================================
// Commit classification
// ============================================================

/// Per-plan classification carried on each `PlanTimelineEvent`.
/// `Unattributed` never appears on a plan's timeline (commits with
/// no relevance to a plan are simply absent).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(
    feature = "cache-encoding",
    derive(wincode::SchemaWrite, wincode::SchemaRead)
)]
#[serde(rename_all = "snake_case")]
pub enum CommitKind {
    /// Touches exactly this plan file (single-plan-touch) and no code.
    PlanOnly,
    /// Touches code attributed to this plan; no plan-file touch on
    /// this commit (attribution inherited via walk-back).
    CodeOnly,
    /// Touches this plan file AND code (single-plan-touch + code).
    Mixed,
    /// Touches two or more distinct plan files on a single commit.
    /// Surfaced in the timeline but never gated.
    MultiPlan,
    /// The lifecycle commit that froze the plan — `.trinity/finished/
    /// <stem>/` satisfied the finalize rule at this commit's tree.
    /// Visible in the timeline as a clickable boundary; not a review
    /// target.
    Finalize,
    /// Commit has no relevance to this plan. Never appears on a
    /// plan's timeline.
    Unattributed,
}

impl CommitKind {
    pub fn as_str(self) -> &'static str {
        match self {
            CommitKind::PlanOnly => "plan_only",
            CommitKind::CodeOnly => "code_only",
            CommitKind::Mixed => "mixed",
            CommitKind::MultiPlan => "multi_plan",
            CommitKind::Finalize => "finalize",
            CommitKind::Unattributed => "unattributed",
        }
    }

    /// Reviewable kinds have a `Some(CommitGate)` on their
    /// `CommitNode` in `RepoState.commits` (Phase 2 of
    /// `commit-first-review-model`). `MultiPlan` / `Finalize` /
    /// `Unattributed` are excluded.
    pub fn is_reviewable(self) -> bool {
        matches!(
            self,
            CommitKind::PlanOnly | CommitKind::CodeOnly | CommitKind::Mixed
        )
    }
}

/// Plan-touch flavor: was this commit the introduction of the plan
/// file or a later revision?
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanTouchKind {
    Intro,
    Revision,
}

impl PlanTouchKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PlanTouchKind::Intro => "intro",
            PlanTouchKind::Revision => "revision",
        }
    }
}

/// Disambiguator on the wire's review-target shape: which side of
/// the review pipeline (plan or impl) the target SHA represents.
/// Derived from the targeted commit's `CommitKind` —
/// `PlanOnly | Mixed → Plan`, `CodeOnly → Impl`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewTargetPhase {
    Plan,
    Impl,
}

impl ReviewTargetPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            ReviewTargetPhase::Plan => "plan",
            ReviewTargetPhase::Impl => "impl",
        }
    }
}

// ============================================================
// Review feedback
// ============================================================

/// Verdict on a single feedback file. Parsed from the file's first
/// non-blank line: `APPROVE` → `Approve`, `REQUEST_CHANGES` →
/// `RequestChanges`, anything else → `Unmarked`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(
    feature = "cache-encoding",
    derive(wincode::SchemaWrite, wincode::SchemaRead)
)]
pub enum Verdict {
    Approve,
    RequestChanges,
    Unmarked,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Approve => "approve",
            Verdict::RequestChanges => "request_changes",
            Verdict::Unmarked => "unmarked",
        }
    }
}

/// Per-commit gate state.
///
/// `Approved`: all cumulative participants have voted APPROVE.
/// `ChangesRequested`: ≥1 cumulative participant voted
/// REQUEST_CHANGES (or Unmarked).
/// `Unreviewed`: no verdict yet, OR ≥1 participant hasn't voted on
/// THIS commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(
    feature = "cache-encoding",
    derive(wincode::SchemaWrite, wincode::SchemaRead)
)]
pub enum CommitGateState {
    Unreviewed,
    Approved,
    ChangesRequested,
}

impl CommitGateState {
    pub fn as_str(self) -> &'static str {
        match self {
            CommitGateState::Unreviewed => "unreviewed",
            CommitGateState::Approved => "approved",
            CommitGateState::ChangesRequested => "changes_requested",
        }
    }
}

/// Pre-projected review-gate state used by the plan-page `ReviewGate`
/// shape. Aliases the per-commit `CommitGateState` vocabulary for the
/// historical plan/impl-tagged wire shape: `Approved` maps to
/// `Ready`, `Unreviewed` to `NeedsReview`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewGateState {
    Ready,
    NeedsReview,
    ChangesRequested,
}

impl ReviewGateState {
    pub fn as_str(self) -> &'static str {
        match self {
            ReviewGateState::Ready => "ready",
            ReviewGateState::NeedsReview => "needs_review",
            ReviewGateState::ChangesRequested => "changes_requested",
        }
    }
}

impl From<CommitGateState> for ReviewGateState {
    fn from(g: CommitGateState) -> Self {
        match g {
            CommitGateState::Approved => ReviewGateState::Ready,
            CommitGateState::Unreviewed => ReviewGateState::NeedsReview,
            CommitGateState::ChangesRequested => ReviewGateState::ChangesRequested,
        }
    }
}

// ============================================================
// Waiting-on / work-action
// ============================================================

/// Which role is responsible for the next move on a plan.
/// `None` means the plan is sealed (`SessionFinished`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitingRole {
    Master,
    Reviewers,
    None,
}

impl WaitingRole {
    pub fn as_str(self) -> &'static str {
        match self {
            WaitingRole::Master => "master",
            WaitingRole::Reviewers => "reviewers",
            WaitingRole::None => "none",
        }
    }
}

/// Why the named role is currently the bottleneck.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitingReason {
    /// Plan is frozen. No work for either role.
    SessionFinished,
    /// Plan body has uncommitted changes in the worktree. Master
    /// commits to release blocked reviews.
    CommitPlanRevision,
    /// REQUEST_CHANGES on the latest reviewable commit; master
    /// addresses and commits.
    AddressCommitChanges,
    /// Latest reviewable commit is approved; master moves forward.
    ReadyToStartImplementation,
    /// Latest reviewable commit hasn't been reviewed yet.
    CommitNeedsReview,
}

impl WaitingReason {
    pub fn as_str(self) -> &'static str {
        match self {
            WaitingReason::SessionFinished => "session_finished",
            WaitingReason::CommitPlanRevision => "commit_plan_revision",
            WaitingReason::AddressCommitChanges => "address_commit_changes",
            WaitingReason::ReadyToStartImplementation => "ready_to_start_implementation",
            WaitingReason::CommitNeedsReview => "commit_needs_review",
        }
    }
}

/// Suggested PR-landing option kind. Closed vocabulary for
/// `api::PrHintOption.kind` — eliminates the prior stringly-typed
/// `name: String` field that the frontend matched on with a `_ =>
/// "Option"` fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrHintOptionKind {
    KeepPlanInPr,
    ExcludePlanFromPr,
}

impl PrHintOptionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PrHintOptionKind::KeepPlanInPr => "keep_plan_in_pr",
            PrHintOptionKind::ExcludePlanFromPr => "exclude_plan_from_pr",
        }
    }
}

impl_display_via_as_str! {
    PlanLifecycle,
    Posture,
    PlanWorktreeStatus,
    CommitKind,
    PlanTouchKind,
    ReviewTargetPhase,
    Verdict,
    CommitGateState,
    ReviewGateState,
    WaitingRole,
    WaitingReason,
    PrHintOptionKind,
}
