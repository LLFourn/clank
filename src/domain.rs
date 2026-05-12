//! Closed-set domain enums for the event/feedback layer. Lifecycle state
//! itself lives in `crate::lifecycle::ActivePlan`; this module only carries
//! the auxiliary enums used by the events table, feedback storage, and the
//! reviewer/master surface.

use crate::lifecycle::CommitSha;

/// Which subdirectory under `.trinity/feedback/<session_id>/` the file
/// lives in. The convention determines both the target kind it ingests
/// against and whether it's accepted in the current phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackKind {
    Plan,
    Impl,
}

impl FeedbackKind {
    pub fn as_str(self) -> &'static str {
        match self {
            FeedbackKind::Plan => "plan",
            FeedbackKind::Impl => "impl",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "plan" => Some(FeedbackKind::Plan),
            "impl" => Some(FeedbackKind::Impl),
            _ => None,
        }
    }
    pub fn target_kind(self) -> TargetKind {
        match self {
            FeedbackKind::Plan => TargetKind::PlanRevision,
            FeedbackKind::Impl => TargetKind::ImplementationCommit,
        }
    }
}

impl std::fmt::Display for FeedbackKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Derived status of a `feedback_files` row at read time. The status is
/// computed by `derive_feedback_file_status` from the row's columns and
/// the kind's expected target — never persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackFileStatus {
    Current,
    Stale,
    Missing,
    ParseError,
    NotYetIngested,
}

impl FeedbackFileStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            FeedbackFileStatus::Current => "current",
            FeedbackFileStatus::Stale => "stale",
            FeedbackFileStatus::Missing => "missing",
            FeedbackFileStatus::ParseError => "parse_error",
            FeedbackFileStatus::NotYetIngested => "not_yet_ingested",
        }
    }
}

/// Top-level session phase as reported by `get_context`. Derived from
/// the active plan's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Planning,
    Implementing,
    NoActivePlan,
}

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

/// Event kinds emitted by the apply layer and the service. The DB stores
/// the string; this enum keeps call sites honest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    // Lifecycle (emitted by apply.rs)
    PlanRevisionCreated,
    ImplRevisionCreated,
    StateTransition,
    DirtyWorktreeWarning,
    HeadResetToKnownSha,
    // Feedback (emitted by SessionService::put_feedback)
    FeedbackAdded,
    FeedbackUpdated,
    // Session/agent metadata
    AgentJoined,
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
            EventKind::HeadResetToKnownSha => "head_reset_to_known_sha",
            EventKind::FeedbackAdded => "feedback_added",
            EventKind::FeedbackUpdated => "feedback_updated",
            EventKind::AgentJoined => "agent_joined",
            EventKind::Renamed => "renamed",
            EventKind::PlanFileMissing => "plan_file_missing",
            EventKind::HumanComment => "human_comment",
        }
    }
}
