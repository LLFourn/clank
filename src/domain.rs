//! Closed-set domain enums for the event/feedback layer. Lifecycle state
//! itself lives in `crate::lifecycle::ActivePlan`; this module only carries
//! the auxiliary enums used by the events table, feedback storage, and the
//! reviewer/master surface.

use crate::lifecycle::CommitSha;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
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

    /// Single-byte tag for the feedback digest preimage.
    pub fn digest_byte(self) -> u8 {
        match self {
            TargetKind::PlanRevision => 1,
            TargetKind::ImplementationCommit => 2,
        }
    }
}

impl std::fmt::Display for TargetKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Typed boundary form for a feedback target. Service signatures and
/// MCP/HTTP decoders take this; raw `target_id: String` does not leak
/// past the storage edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedbackTargetRef {
    PlanRevision(i64),
    ImplementationCommit(CommitSha),
}

impl FeedbackTargetRef {
    pub fn kind(&self) -> TargetKind {
        match self {
            FeedbackTargetRef::PlanRevision(_) => TargetKind::PlanRevision,
            FeedbackTargetRef::ImplementationCommit(_) => TargetKind::ImplementationCommit,
        }
    }

    pub fn target_id_string(&self) -> String {
        match self {
            FeedbackTargetRef::PlanRevision(id) => id.to_string(),
            FeedbackTargetRef::ImplementationCommit(sha) => sha.as_str().to_string(),
        }
    }

    /// Build from the storage-edge representation. Returns `None` if
    /// `target_id` is malformed for the given kind (e.g. non-integer for
    /// `PlanRevision`). The SQL CHECK constraint should make this
    /// unreachable in practice.
    pub fn from_storage(kind: TargetKind, target_id: &str) -> Option<Self> {
        match kind {
            TargetKind::PlanRevision => target_id.parse::<i64>().ok().map(Self::PlanRevision),
            TargetKind::ImplementationCommit => {
                Some(Self::ImplementationCommit(CommitSha::from(target_id)))
            }
        }
    }
}

impl std::fmt::Display for FeedbackTargetRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FeedbackTargetRef::PlanRevision(id) => write!(f, "plan_revision {id}"),
            FeedbackTargetRef::ImplementationCommit(sha) => write!(f, "commit {}", sha.as_str()),
        }
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

/// Event kinds emitted by the apply layer and the service. The DB stores
/// the string; this enum keeps call sites honest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    // Lifecycle (emitted by apply.rs)
    PlanRevisionCreated,
    ImplRevisionCreated,
    StateTransition,
    DirtyWorktreeWarning,
    // Feedback (emitted by SessionService::put_feedback)
    FeedbackAdded,
    FeedbackUpdated,
    // Session/agent metadata (emitted by curator + master tools)
    AgentJoined,
    MasterEvicted,
    Renamed,
    PlanFileMissing,
    HumanComment,
}

impl EventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::PlanRevisionCreated => "plan_revision_created",
            EventKind::ImplRevisionCreated => "impl_revision_created",
            EventKind::StateTransition => "state_transition",
            EventKind::DirtyWorktreeWarning => "dirty_worktree_warning",
            EventKind::FeedbackAdded => "feedback_added",
            EventKind::FeedbackUpdated => "feedback_updated",
            EventKind::AgentJoined => "agent_joined",
            EventKind::MasterEvicted => "master_evicted",
            EventKind::Renamed => "renamed",
            EventKind::PlanFileMissing => "plan_file_missing",
            EventKind::HumanComment => "human_comment",
        }
    }
}
