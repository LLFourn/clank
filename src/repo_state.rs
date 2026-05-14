//! In-memory state for the filesystem-truth model. All data here is a
//! derived cache from git + working tree; nothing is persisted.
//!
//! See `.trinity/plans/filesystem-truth-rewrite.md` for the architecture.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;

use crate::lifecycle::{AgentLabel, CommitSha, ContentHash, SessionId};

pub type RepoRoot = PathBuf;

#[derive(Debug, Default)]
pub struct Trinity {
    pub repos: BTreeMap<RepoRoot, RepoState>,
    pub live_events: VecDeque<LiveEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoState {
    pub root: PathBuf,
    pub sessions: BTreeMap<SessionId, Session>,
    pub head: Option<CommitSha>,
    /// `git ls-tree HEAD`-derived ordered set of (commit_sha → attribution).
    /// Built during rebuild by walking history past the oldest plan_intro.
    pub attribution: BTreeMap<CommitSha, AttributionResult>,
}

impl RepoState {
    pub fn empty(root: PathBuf) -> Self {
        Self {
            root,
            sessions: BTreeMap::new(),
            head: None,
            attribution: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub id: SessionId,
    /// Path relative to the repo root. Either `.trinity/plans/<id>.md` or
    /// `.trinity/plans/done/<id>.md` — never the absolute path.
    pub plan_path: PathBuf,
    /// Body from HEAD's blob, not the working tree.
    pub body: String,
    pub body_hash: ContentHash,
    /// `plan_intro` for this session: the commit that first added the
    /// plan file. Used as the lower bound for attribution walks.
    pub plan_intro: CommitSha,
    pub plan_feedback: BTreeMap<(CommitSha, AgentLabel), Feedback>,
    pub impl_feedback: BTreeMap<(CommitSha, AgentLabel), Feedback>,
    /// Plan-phase feedback files dropped while `plan_worktree_status` was
    /// `BodyDirty`. Not auto-organized into `<target-sha>/<author>.md` until
    /// the plan revision lands.
    pub held_plan_feedback: Vec<HeldFeedback>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Feedback {
    /// Absolute path on disk for the feedback file.
    pub path: PathBuf,
    pub body: String,
    pub verdict: Verdict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldFeedback {
    pub path: PathBuf,
    pub author: AgentLabel,
    pub body: String,
    /// Machine-readable key for the held reason. Today: `"plan_dirty"`.
    pub reason: &'static str,
}

/// Working-tree state of a session's plan file relative to HEAD. **Never
/// stored on `Session`** — always recomputed at read time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanWorktreeStatus {
    /// Working tree matches HEAD's plan blob.
    Clean,
    /// File exists at the active path with a different body.
    BodyDirty,
    /// Active path is missing from the working tree but the done counterpart
    /// is present — operator ran `mv` but hasn't committed.
    DoneMovePending,
    /// Active path missing and no done counterpart — operator deleted the
    /// file without doing the proper move.
    MissingActivePlanFile,
}

impl PlanWorktreeStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            PlanWorktreeStatus::Clean => "clean",
            PlanWorktreeStatus::BodyDirty => "body_dirty",
            PlanWorktreeStatus::DoneMovePending => "done_move_pending",
            PlanWorktreeStatus::MissingActivePlanFile => "missing_active_plan_file",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Planning,
    Implementing,
    Done,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Planning => "planning",
            Phase::Implementing => "implementing",
            Phase::Done => "done",
        }
    }
}

/// Per-commit attribution result. See `.trinity/plans/filesystem-truth-rewrite.md`
/// "Commit Attribution — pure git walk" for the four classification rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttributionResult {
    Attributed {
        session: SessionId,
        /// Set when this commit itself touched the session's plan file
        /// (single-plan-touch case). `None` when the commit inherited
        /// attribution from a parent via walk-back.
        plan_touch: Option<PlanTouchKind>,
        /// True if this commit modified any non-`.trinity/` file.
        has_code_changes: bool,
    },
    /// Multi-plan-touch commit, or walk reached root without finding a
    /// single-plan-touch ancestor. Descendants walk through this commit
    /// transparently.
    Unattributed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanTouchKind {
    Intro,
    Revision,
    DoneMove,
}

impl PlanTouchKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PlanTouchKind::Intro => "intro",
            PlanTouchKind::Revision => "revision",
            PlanTouchKind::DoneMove => "done_move",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveEvent {
    pub ts: i64,
    pub repo: RepoRoot,
    pub session_id: Option<SessionId>,
    pub kind: &'static str,
    pub payload: serde_json::Value,
}

/// The `waiting_on` projection — the canonical per-session "who blocks
/// progress" signal surfaced in MCP context and the web UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitingOn {
    pub role: WaitingRole,
    pub reason: WaitingReason,
    pub agents: Vec<AgentLabel>,
    pub description: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitingReason {
    SessionDone,
    CommitDoneMove,
    RestoreOrCommitDoneMove,
    CommitPlanRevision,
    AddressPlanRequestChanges,
    ReadyToImplement,
    PlanNeedsInitialReview,
    PlanNeedsRereview,
    AddressImplRequestChanges,
    ReadyToFinish,
    ImplNeedsInitialReview,
    ImplNeedsRereview,
}

impl WaitingReason {
    pub fn as_str(self) -> &'static str {
        match self {
            WaitingReason::SessionDone => "session_done",
            WaitingReason::CommitDoneMove => "commit_done_move",
            WaitingReason::RestoreOrCommitDoneMove => "restore_or_commit_done_move",
            WaitingReason::CommitPlanRevision => "commit_plan_revision",
            WaitingReason::AddressPlanRequestChanges => "address_plan_request_changes",
            WaitingReason::ReadyToImplement => "ready_to_implement",
            WaitingReason::PlanNeedsInitialReview => "plan_needs_initial_review",
            WaitingReason::PlanNeedsRereview => "plan_needs_rereview",
            WaitingReason::AddressImplRequestChanges => "address_impl_request_changes",
            WaitingReason::ReadyToFinish => "ready_to_finish",
            WaitingReason::ImplNeedsInitialReview => "impl_needs_initial_review",
            WaitingReason::ImplNeedsRereview => "impl_needs_rereview",
        }
    }
}
