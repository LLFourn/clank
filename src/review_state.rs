//! Review verdict + gate types. The gate derivation that depended on
//! SQL `FeedbackRecord` rows was deleted when the filesystem-truth
//! rewrite landed; the new derivation lives in
//! `mcp_response::derive_gate_from_feedback`, working over the in-memory
//! `Session.plan_feedback` / `impl_feedback` maps.

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewPhase {
    Plan,
    Impl,
}

impl ReviewPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            ReviewPhase::Plan => "plan",
            ReviewPhase::Impl => "impl",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "plan" => ReviewPhase::Plan,
            "impl" => ReviewPhase::Impl,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewGateState {
    NeedsReview,
    ChangesRequested,
    Ready,
}

impl ReviewGateState {
    pub fn as_str(self) -> &'static str {
        match self {
            ReviewGateState::NeedsReview => "needs_review",
            ReviewGateState::ChangesRequested => "changes_requested",
            ReviewGateState::Ready => "ready",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "needs_review" => ReviewGateState::NeedsReview,
            "changes_requested" => ReviewGateState::ChangesRequested,
            "ready" => ReviewGateState::Ready,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReviewGateDecision {
    pub phase: ReviewPhase,
    pub state: ReviewGateState,
    pub approval_rule: &'static str,
    pub participants: Vec<AgentLabel>,
    pub approvals: Vec<AgentLabel>,
    pub request_changes: Vec<AgentLabel>,
    pub unmarked: Vec<AgentLabel>,
    pub missing_approvals: Vec<AgentLabel>,
}

impl ReviewGateDecision {
    pub fn empty(phase: ReviewPhase) -> Self {
        Self {
            phase,
            state: ReviewGateState::NeedsReview,
            approval_rule: "all_participants",
            participants: Vec::new(),
            approvals: Vec::new(),
            request_changes: Vec::new(),
            unmarked: Vec::new(),
            missing_approvals: Vec::new(),
        }
    }
}
