//! Daemon-internal review gate type. The closed-vocab enums
//! (`Verdict`, `CommitGateState`) live in `trinity_core`; this
//! module re-exports them for ergonomic local use and owns the
//! `CommitGate` struct (which holds daemon-internal `AgentLabel`
//! identifiers, so it can't be in the wire crate as-is).

use serde::Serialize;

use crate::lifecycle::AgentLabel;

pub use trinity_core::CommitGateState;

/// Folded review state for one commit. `participants` is the
/// cumulative plan-wide set (everyone who left feedback on any
/// earlier reviewable commit in this plan's history); `approvers` /
/// `requesters` / `ambiguous` are scoped to *this* SHA only.
/// `missing` = participants \ (approvers ∪ requesters ∪ ambiguous).
///
/// All `Vec` fields are deduped and ordered by first-seen across
/// the plan's `timeline` (the fold walks events in chronological
/// order and threads the cumulative-participant carry through
/// `build_gate_step`; the type doesn't enforce order, in deference
/// to the UI which reads them as display lists).
///
/// `feedback` carries the verdict-bearing files on this commit
/// keyed by author. UI / MCP responses read bodies and rendered
/// HTML from here. Empty for commits with no feedback yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CommitGate {
    pub state: CommitGateState,
    pub participants: Vec<AgentLabel>,
    pub approvers: Vec<AgentLabel>,
    pub requesters: Vec<AgentLabel>,
    pub ambiguous: Vec<AgentLabel>,
    pub missing: Vec<AgentLabel>,
    #[serde(skip_serializing)]
    pub feedback: std::collections::BTreeMap<AgentLabel, crate::repo_state::Feedback>,
}
