//! Review verdict + gate types. The active per-commit fold lives in
//! `projection::build_commit_gates`, operating over a single
//! commit-keyed feedback map (see plan-doc §"`CommitGate`").

use serde::Serialize;

use crate::lifecycle::AgentLabel;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewVerdict {
    Approve,
    RequestChanges,
    Unmarked,
}

impl ReviewVerdict {
    pub fn marker(self) -> &'static str {
        match self {
            ReviewVerdict::Approve => "APPROVE",
            ReviewVerdict::RequestChanges => "REQUEST_CHANGES",
            ReviewVerdict::Unmarked => "UNMARKED",
        }
    }

    pub fn css_class(self) -> &'static str {
        match self {
            ReviewVerdict::Approve => "approve",
            ReviewVerdict::RequestChanges => "request-changes",
            ReviewVerdict::Unmarked => "unmarked",
        }
    }

    pub fn is_verdict_bearing(self) -> bool {
        matches!(self, ReviewVerdict::Approve | ReviewVerdict::RequestChanges)
    }
}

/// Per-commit gate state under the commit-centric review model.
/// The authoritative store for review state — `Plan.commits` maps
/// each reviewable commit SHA to one of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CommitGateState {
    /// At least one cumulative participant is missing on this commit,
    /// OR no APPROVE has landed yet.
    Unreviewed,
    /// All cumulative participants have responded with APPROVE on
    /// this commit; no `REQUEST_CHANGES` or `Unmarked` outstanding.
    Approved,
    /// At least one responder is `REQUEST_CHANGES` or `Unmarked` on
    /// this commit. Ambiguous/unmarked counts here so the master is
    /// prompted to fix the verdict marker rather than silently
    /// stalling.
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

/// Folded review state for one commit. `participants` is the
/// cumulative plan-wide set (everyone who left feedback on any
/// earlier reviewable commit in this plan's history); `approvers` /
/// `requesters` / `ambiguous` are scoped to *this* SHA only.
/// `missing` = participants \ (approvers ∪ requesters ∪ ambiguous).
///
/// All `Vec` fields are deduped and ordered by first-seen-in-
/// `commit_order` (the construction loop in `projection::build_commit_gates`
/// enforces this; the type doesn't, in deference to the UI which
/// reads them as display lists).
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
