use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::domain::{FeedbackTargetRef, TargetKind};
use crate::lifecycle::AgentLabel;
use crate::storage::feedback::FeedbackRecord;

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

pub fn parse_verdict(body: &str) -> ReviewVerdict {
    match body.lines().map(str::trim).find(|line| !line.is_empty()) {
        Some("APPROVE") => ReviewVerdict::Approve,
        Some("REQUEST_CHANGES") => ReviewVerdict::RequestChanges,
        _ => ReviewVerdict::Unmarked,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewPhase {
    Plan,
    Impl,
}

impl ReviewPhase {
    pub fn target_kind(self) -> TargetKind {
        match self {
            ReviewPhase::Plan => TargetKind::PlanRevision,
            ReviewPhase::Impl => TargetKind::ImplementationCommit,
        }
    }

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
    #[serde(rename = "override")]
    pub override_status: Option<ReviewGateOverride>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReviewGateOverride {
    pub actor: String,
    pub state: ReviewGateState,
    pub target_id: String,
    pub created_at: i64,
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
            override_status: None,
        }
    }
}

pub fn derive_gate(
    phase: ReviewPhase,
    current_target: Option<&FeedbackTargetRef>,
    feedback: &[FeedbackRecord],
) -> ReviewGateDecision {
    let Some(current_target) = current_target else {
        return ReviewGateDecision::empty(phase);
    };
    let target_kind = phase.target_kind();
    let current_target_id = current_target.target_id_string();

    let mut participants: BTreeSet<AgentLabel> = BTreeSet::new();
    let mut current: BTreeMap<AgentLabel, ReviewVerdict> = BTreeMap::new();
    let mut current_unmarked: BTreeSet<AgentLabel> = BTreeSet::new();

    for record in feedback
        .iter()
        .filter(|record| record.target.kind() == target_kind)
    {
        let author = record.author_label.clone();
        let verdict = parse_verdict(&record.body);
        if verdict.is_verdict_bearing() {
            participants.insert(author.clone());
        }
        if record.target.target_id_string() == current_target_id {
            if verdict == ReviewVerdict::Unmarked {
                current_unmarked.insert(author.clone());
            }
            current.insert(author, verdict);
        }
    }

    let mut approvals = Vec::new();
    let mut request_changes = Vec::new();
    let mut missing_approvals = Vec::new();
    for author in &participants {
        match current.get(author) {
            Some(ReviewVerdict::Approve) => approvals.push(author.clone()),
            Some(ReviewVerdict::RequestChanges) => {
                request_changes.push(author.clone());
                missing_approvals.push(author.clone());
            }
            Some(ReviewVerdict::Unmarked) | None => missing_approvals.push(author.clone()),
        }
    }

    let state = if !request_changes.is_empty() {
        ReviewGateState::ChangesRequested
    } else if !participants.is_empty() && missing_approvals.is_empty() {
        ReviewGateState::Ready
    } else {
        ReviewGateState::NeedsReview
    };

    ReviewGateDecision {
        phase,
        state,
        approval_rule: "all_participants",
        participants: participants.into_iter().collect(),
        approvals,
        request_changes,
        unmarked: current_unmarked.into_iter().collect(),
        missing_approvals,
        override_status: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::FeedbackTargetRef;
    use crate::lifecycle::{AgentLabel, SessionId};

    fn row(author: &str, target: FeedbackTargetRef, body: &str, id: i64) -> FeedbackRecord {
        FeedbackRecord {
            id,
            session_id: SessionId::from("s"),
            plan_id: 1,
            target,
            author_label: AgentLabel::from(author),
            body: body.to_string(),
            created_at: id,
            updated_at: id,
        }
    }

    fn labels(values: &[&str]) -> Vec<AgentLabel> {
        values.iter().copied().map(AgentLabel::from).collect()
    }

    #[test]
    fn parses_only_exact_first_line_markers() {
        assert_eq!(
            parse_verdict("\nAPPROVE\nlooks good"),
            ReviewVerdict::Approve
        );
        assert_eq!(
            parse_verdict("REQUEST_CHANGES\nfix it"),
            ReviewVerdict::RequestChanges
        );
        assert_eq!(parse_verdict("approve\n"), ReviewVerdict::Unmarked);
        assert_eq!(parse_verdict("COMMENT\n"), ReviewVerdict::Unmarked);
        assert_eq!(parse_verdict("looks good\n"), ReviewVerdict::Unmarked);
    }

    #[test]
    fn no_participants_needs_review_even_with_unmarked_feedback() {
        let current = FeedbackTargetRef::PlanRevision(10);
        let rows = vec![row("alice", current.clone(), "looks fine", 1)];
        let gate = derive_gate(ReviewPhase::Plan, Some(&current), &rows);
        assert_eq!(gate.state, ReviewGateState::NeedsReview);
        assert!(gate.participants.is_empty());
        assert_eq!(gate.unmarked, labels(&["alice"]));
    }

    #[test]
    fn single_approving_participant_is_ready() {
        let current = FeedbackTargetRef::PlanRevision(10);
        let rows = vec![row("alice", current.clone(), "APPROVE\nship", 1)];
        let gate = derive_gate(ReviewPhase::Plan, Some(&current), &rows);
        assert_eq!(gate.state, ReviewGateState::Ready);
        assert_eq!(gate.participants, labels(&["alice"]));
        assert_eq!(gate.approvals, labels(&["alice"]));
        assert!(gate.missing_approvals.is_empty());
    }

    #[test]
    fn all_verdict_participants_must_approve_current_target() {
        let old = FeedbackTargetRef::PlanRevision(9);
        let current = FeedbackTargetRef::PlanRevision(10);
        let rows = vec![
            row("alice", current.clone(), "APPROVE\nship", 1),
            row("bob", old, "APPROVE\nold", 2),
            row("carol", current.clone(), "notes only", 3),
        ];
        let gate = derive_gate(ReviewPhase::Plan, Some(&current), &rows);
        assert_eq!(gate.state, ReviewGateState::NeedsReview);
        assert_eq!(gate.participants, labels(&["alice", "bob"]));
        assert_eq!(gate.approvals, labels(&["alice"]));
        assert_eq!(gate.missing_approvals, labels(&["bob"]));
        assert_eq!(gate.unmarked, labels(&["carol"]));
    }

    #[test]
    fn request_changes_wins() {
        let current = FeedbackTargetRef::ImplementationCommit("abc".into());
        let rows = vec![
            row("alice", current.clone(), "APPROVE\nship", 1),
            row("bob", current.clone(), "REQUEST_CHANGES\nno", 2),
        ];
        let gate = derive_gate(ReviewPhase::Impl, Some(&current), &rows);
        assert_eq!(gate.state, ReviewGateState::ChangesRequested);
        assert_eq!(gate.request_changes, labels(&["bob"]));
        assert_eq!(gate.missing_approvals, labels(&["bob"]));
    }
}
