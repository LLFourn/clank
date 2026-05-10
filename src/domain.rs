//! Closed-set domain enums. The DB columns store strings (with CHECK constraints
//! in the migration), and these enums provide a typed interface at the
//! Rust boundary so call sites and pattern matches don't drift into typo bugs.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkState {
    Planning,
    PlanApproved,
    ImplementationReview,
    Done,
    Archived,
}

impl WorkState {
    pub fn as_str(self) -> &'static str {
        match self {
            WorkState::Planning => "planning",
            WorkState::PlanApproved => "plan_approved",
            WorkState::ImplementationReview => "implementation_review",
            WorkState::Done => "done",
            WorkState::Archived => "archived",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "planning" => WorkState::Planning,
            "plan_approved" => WorkState::PlanApproved,
            "implementation_review" => WorkState::ImplementationReview,
            "done" => WorkState::Done,
            "archived" => WorkState::Archived,
            _ => return None,
        })
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, WorkState::Done | WorkState::Archived)
    }

    /// True iff a plan-file edit should be snapshotted as a new
    /// `plan_revision`. Once we leave the planning phase or hit a terminal
    /// state, plan-file edits become "needs human lifecycle action" warnings.
    pub fn accepts_plan_revisions(self) -> bool {
        matches!(self, WorkState::Planning | WorkState::PlanApproved)
    }

    pub fn active_artifact(self) -> TargetKind {
        match self {
            WorkState::ImplementationReview | WorkState::Done => TargetKind::ImplementationCommit,
            _ => TargetKind::PlanRevision,
        }
    }
}

impl fmt::Display for WorkState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetKind {
    PlanRevision,
    ImplementationCommit,
}

impl TargetKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TargetKind::PlanRevision => "plan_revision",
            TargetKind::ImplementationCommit => "implementation_commit",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "plan_revision" => TargetKind::PlanRevision,
            "implementation_commit" => TargetKind::ImplementationCommit,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedbackStatus {
    Pending,
    Staged,
    Delivered,
    Withdrawn,
}

impl FeedbackStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            FeedbackStatus::Pending => "pending",
            FeedbackStatus::Staged => "staged",
            FeedbackStatus::Delivered => "delivered",
            FeedbackStatus::Withdrawn => "withdrawn",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "pending" => FeedbackStatus::Pending,
            "staged" => FeedbackStatus::Staged,
            "delivered" => FeedbackStatus::Delivered,
            "withdrawn" => FeedbackStatus::Withdrawn,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentRole {
    Master,
    Reviewer,
}

impl AgentRole {
    pub fn as_str(self) -> &'static str {
        match self {
            AgentRole::Master => "master",
            AgentRole::Reviewer => "reviewer",
        }
    }
}

/// Closed set of event kinds Trinity emits in v0. The DB stores the string;
/// this enum keeps every emit/dispatch site honest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    PlanRevisionCreated,
    ImplRevisionCreated,
    AgentJoined,
    FeedbackAdded,
    HumanComment,
    Archived,
    Renamed,
    MasterEvicted,
    PlanFileMissing,
    PlanFileChangedAfterImplementation,
    PlanApproved,
    MarkedDone,
    StateTransition,
    DirtyWorktreeWarning,
    DirectiveAcked,
}

impl EventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::PlanRevisionCreated => "plan_revision_created",
            EventKind::ImplRevisionCreated => "impl_revision_created",
            EventKind::AgentJoined => "agent_joined",
            EventKind::FeedbackAdded => "feedback_added",
            EventKind::HumanComment => "human_comment",
            EventKind::Archived => "archived",
            EventKind::Renamed => "renamed",
            EventKind::MasterEvicted => "master_evicted",
            EventKind::PlanFileMissing => "plan_file_missing",
            EventKind::PlanFileChangedAfterImplementation => {
                "plan_file_changed_after_implementation"
            }
            EventKind::PlanApproved => "plan_approved",
            EventKind::MarkedDone => "marked_done",
            EventKind::StateTransition => "state_transition",
            EventKind::DirtyWorktreeWarning => "dirty_worktree_warning",
            EventKind::DirectiveAcked => "directive_acked",
        }
    }
}
