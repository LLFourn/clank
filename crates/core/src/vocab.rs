//! Closed-vocabulary enums shared across Clank.
//!
//! Every enum here is a small fixed set that callers branch on.
//! Wire form is snake_case (via `#[serde(rename_all = "snake_case")]`).
//! `as_str(self) -> &'static str` returns the wire string for log
//! / path / display use without round-tripping through serde.

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
    /// in HEAD.
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

/// Verdict on a single feedback file. Parsed from the file's first
/// non-blank line: `APPROVE` → `Approve`, `FINISHED` → `Finished`,
/// `REQUEST_CHANGES` → `RequestChanges`, anything else → `Unmarked`.
///
/// `Finished` is the "ship it — finalize this plan" signal,
/// distinct from `Approve` which just blesses the commit's work.
/// Reviewers explicitly opt into `Finished`; clank never infers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(
    feature = "cache-encoding",
    derive(wincode::SchemaWrite, wincode::SchemaRead)
)]
pub enum Verdict {
    Approve,
    Finished,
    RequestChanges,
    Unmarked,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Approve => "approve",
            Verdict::Finished => "finished",
            Verdict::RequestChanges => "request_changes",
            Verdict::Unmarked => "unmarked",
        }
    }
}

/// Per-commit gate state.
///
/// Precedence (any single review pushes the gate to the highest
/// matching state):
/// - `ChangesRequested`: ≥1 reviewer voted REQUEST_CHANGES.
/// - `Finished`: ≥1 reviewer voted FINISHED, none requested changes.
/// - `Approved`: ≥1 reviewer voted APPROVE, none FINISHED or
///   request-changes.
/// - `Unreviewed`: no recognized verdict yet on this commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(
    feature = "cache-encoding",
    derive(wincode::SchemaWrite, wincode::SchemaRead)
)]
pub enum CommitGateState {
    Unreviewed,
    Approved,
    Finished,
    ChangesRequested,
}

impl CommitGateState {
    pub fn as_str(self) -> &'static str {
        match self {
            CommitGateState::Unreviewed => "unreviewed",
            CommitGateState::Approved => "approved",
            CommitGateState::Finished => "finished",
            CommitGateState::ChangesRequested => "changes_requested",
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
    /// Gate is FINISHED + worktree clean — master should run
    /// `clank finish`.
    ReadyToFinalize,
    /// Latest reviewable commit is APPROVE (not FINISHED). Master
    /// keeps working — more impl, more docs, more tests, or
    /// nudge a reviewer to upgrade to FINISHED.
    GateApproved,
    /// Latest reviewable commit hasn't been reviewed yet.
    CommitNeedsReview,
}

impl WaitingReason {
    pub fn as_str(self) -> &'static str {
        match self {
            WaitingReason::SessionFinished => "session_finished",
            WaitingReason::CommitPlanRevision => "commit_plan_revision",
            WaitingReason::AddressCommitChanges => "address_commit_changes",
            WaitingReason::ReadyToFinalize => "ready_to_finalize",
            WaitingReason::GateApproved => "gate_approved",
            WaitingReason::CommitNeedsReview => "commit_needs_review",
        }
    }
}

/// Which agent CLI an agent process is running inside. Used by
/// `clank stop-hook` (passed via `--tool`) and `clank as` (auto-
/// detected from which session-id env var is set).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tool {
    Claude,
    Codex,
}

impl Tool {
    pub fn as_str(self) -> &'static str {
        match self {
            Tool::Claude => "claude",
            Tool::Codex => "codex",
        }
    }
}

/// Auto-mode setting for an agent's stop-hook behavior.
///
/// - `Off`: hook exits immediately.
/// - `On`: hook long-polls `clank wfw` until work arrives or
///   `wfw_timeout` elapses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoMode {
    #[default]
    Off,
    #[serde(alias = "hint", alias = "wait")]
    On,
}

impl AutoMode {
    pub fn as_str(self) -> &'static str {
        match self {
            AutoMode::Off => "off",
            AutoMode::On => "on",
        }
    }
}

/// Role an agent plays — their default perspective for `wfw`,
/// `stop-hook`, etc. Per-user preference stored on the agent's
/// own [`AgentConfig`]; not a repo-shared assertion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Master,
    #[default]
    Reviewers,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Master => "master",
            Role::Reviewers => "reviewers",
        }
    }
}

/// Work-item event that can trigger a configured shell hook.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HookEvent {
    MasterWork,
    ReviewerWork,
    PlanFinalized,
    Idle,
    Blocked,
}

impl HookEvent {
    pub fn as_str(self) -> &'static str {
        match self {
            HookEvent::MasterWork => "master-work",
            HookEvent::ReviewerWork => "reviewer-work",
            HookEvent::PlanFinalized => "plan-finalized",
            HookEvent::Idle => "idle",
            HookEvent::Blocked => "blocked",
        }
    }
}

impl_display_via_as_str! {
    PlanLifecycle,
    PlanWorktreeStatus,
    Verdict,
    CommitGateState,
    WaitingReason,
    Tool,
    AutoMode,
    Role,
    HookEvent,
}
