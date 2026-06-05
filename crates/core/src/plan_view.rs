//! Shared `WaitingOn` / `WorktreeFacts` types consumed by the CLI's
//! status and wfw surfaces. The gate computation that produces
//! these values lives in [`crate::wait::compute_gate`] +
//! [`crate::repo_state::RepoState::derive_status`] — one place, no
//! parallel implementation.

use serde::{Deserialize, Serialize};

use crate::ids::AgentLabel;
use crate::repo_state::NonEmptyVec;
use crate::vocab::PlanWorktreeStatus;

/// Live worktree facts the CLI computes for one plan's plan file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeFacts {
    pub status: PlanWorktreeStatus,
}

/// Projection of a CLI `BlockEntry` for use inside `derive_status`'s
/// fold output. The CLI `BlockEntry` (on-disk scan result) stays
/// as-is; this type carries only what the gate fold needs. The
/// `creator` field name (vs `BlockEntry::agent`) makes the role
/// explicit at the projection boundary: this is specifically the
/// agent that CREATED the block, not "an agent" generically.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanBlock {
    pub creator: AgentLabel,
    pub name: String,
    pub message: String,
}

/// Human-meaningful summary of "what's blocking this plan."
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WaitingOn {
    /// Latest reviewable commit lacks feedback from one or more
    /// registered reviewers. `missing` is non-empty by construction.
    /// (The `FirstReview` variant — "any reviewer eligible" — was
    /// removed when the all-reviewers gate landed: under the new
    /// semantics every commit's reviewer set is the registered
    /// `expected_reviewers` list, never "anyone who shows up".)
    ReviewerApprovalsMissing { missing: NonEmptyVec<AgentLabel> },
    /// A reviewer requested changes (or left an ambiguous verdict).
    /// Master needs to address.
    MasterToRevise {
        requesters: Vec<AgentLabel>,
        ambiguous: Vec<AgentLabel>,
    },
    /// Gate is APPROVED (not FINISHED) on the latest reviewable
    /// commit. Master keeps working — more impl, more docs, more
    /// tests, or prompting a reviewer to upgrade their APPROVE to
    /// FINISHED. `clank finish` is blocked until someone marks
    /// FINISHED.
    MasterToContinue,
    /// Gate is FINISHED on the latest reviewable commit and the
    /// plan worktree is clean — master just needs to run
    /// `clank finish`.
    MasterToFinalize,
    /// Gate is approved but the plan file has uncommitted edits.
    /// Master needs to commit the next revision.
    MasterToCommit,
    /// Plan has an open (unanswered) plan-scoped block. The block
    /// creator must clear it; reviewers and master are NOT being
    /// asked to act. Dominates all review-driven `WaitingOn`
    /// variants regardless of review state on the latest
    /// reviewable commit.
    Blocked { block: PlanBlock },
}
