//! Closed-set domain enums for the event/feedback layer. Lifecycle state
//! itself lives in `crate::lifecycle::ActivePlan`; this module only carries
//! the auxiliary enums used by the events table and the curator surface.

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

/// Event kinds emitted by the apply layer and the curator. The DB stores the
/// string; this enum keeps call sites honest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    // Lifecycle (emitted by apply.rs)
    PlanRevisionCreated,
    ImplRevisionCreated,
    StateTransition,
    DirtyWorktreeWarning,
    FeedbackAdded,
    // Session/agent metadata (emitted by curator + master tools)
    AgentJoined,
    MasterEvicted,
    Renamed,
    PlanFileMissing,
    HumanComment,
    DirectiveAcked,
}

impl EventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::PlanRevisionCreated => "plan_revision_created",
            EventKind::ImplRevisionCreated => "impl_revision_created",
            EventKind::StateTransition => "state_transition",
            EventKind::DirtyWorktreeWarning => "dirty_worktree_warning",
            EventKind::FeedbackAdded => "feedback_added",
            EventKind::AgentJoined => "agent_joined",
            EventKind::MasterEvicted => "master_evicted",
            EventKind::Renamed => "renamed",
            EventKind::PlanFileMissing => "plan_file_missing",
            EventKind::HumanComment => "human_comment",
            EventKind::DirectiveAcked => "directive_acked",
        }
    }
}
