//! Pure projections used by MCP, HTTP, and SSE: phase derivation, plan
//! worktree status from hash comparisons, and the `waiting_on` value.
//!
//! All consumers (MCP `get_context`, HTTP routes, SSE payload construction,
//! tests) call into these functions so the derivation logic stays in one
//! place and can't drift between surfaces.

use crate::lifecycle::ContentHash;
use crate::repo_state::{
    AttributionResult, Phase, PlanWorktreeStatus, Session, WaitingOn, WaitingReason, WaitingRole,
};
use crate::review_state::{ReviewGateDecision, ReviewGateState};

use std::collections::BTreeMap;

use crate::lifecycle::CommitSha;

/// `plan_worktree_status` from hash comparisons + filesystem existence.
///
/// Inputs are gathered by the IO layer (one `git show HEAD:<path>` for the
/// blob hash, one `fs::read` for the working-tree body, one `fs::exists`
/// for the done counterpart). The function itself is pure.
pub fn plan_worktree_status(
    head_blob_hash: Option<&ContentHash>,
    worktree_body_hash: Option<&ContentHash>,
    done_counterpart_exists: bool,
) -> PlanWorktreeStatus {
    match (head_blob_hash, worktree_body_hash) {
        (Some(h), Some(w)) if h == w => PlanWorktreeStatus::Clean,
        (Some(_), Some(_)) => PlanWorktreeStatus::BodyDirty,
        (Some(_), None) if done_counterpart_exists => PlanWorktreeStatus::DoneMovePending,
        (Some(_), None) => PlanWorktreeStatus::MissingActivePlanFile,
        // Session has no HEAD blob (shouldn't happen for tracked sessions).
        (None, _) => PlanWorktreeStatus::Clean,
    }
}

/// Phase per session, derived from path + attribution map.
///
/// - `Done` if the plan_path is under `.trinity/plans/done/`.
/// - `Implementing` if any attributed commit past plan_intro has code changes.
/// - `Planning` otherwise.
pub fn phase(
    session: &Session,
    attribution: &BTreeMap<CommitSha, AttributionResult>,
) -> Phase {
    use std::path::Component;
    let is_done = session
        .plan_path
        .components()
        .any(|c| matches!(c, Component::Normal(s) if s == "done"));
    if is_done {
        return Phase::Done;
    }
    for attr in attribution.values() {
        if let AttributionResult::Attributed {
            session: sid,
            has_code_changes: true,
            ..
        } = attr
        {
            if sid == &session.id {
                return Phase::Implementing;
            }
        }
    }
    Phase::Planning
}

/// Compose the `waiting_on` value for a session per the case table in the
/// plan. Top rows (worktree-status-driven) preempt gate-driven rows.
///
/// The caller supplies the already-derived phase, worktree status, and the
/// plan/impl gates (computed via `review_state::derive_gate`). This is
/// intentional: the projection is a fold over already-computed pieces, not
/// the place to recompute the gate.
pub fn waiting_on(
    phase: Phase,
    worktree_status: PlanWorktreeStatus,
    plan_gate: Option<&ReviewGateDecision>,
    impl_gate: Option<&ReviewGateDecision>,
) -> WaitingOn {
    // Top-priority: terminal done state.
    if matches!(phase, Phase::Done) {
        return make(WaitingRole::None, WaitingReason::SessionDone, Vec::new());
    }

    // Worktree-status rows preempt gate-driven rows.
    match worktree_status {
        PlanWorktreeStatus::DoneMovePending => {
            return make(WaitingRole::Master, WaitingReason::CommitDoneMove, Vec::new());
        }
        PlanWorktreeStatus::MissingActivePlanFile => {
            return make(
                WaitingRole::Master,
                WaitingReason::RestoreOrCommitDoneMove,
                Vec::new(),
            );
        }
        PlanWorktreeStatus::BodyDirty => {
            return make(
                WaitingRole::Master,
                WaitingReason::CommitPlanRevision,
                Vec::new(),
            );
        }
        PlanWorktreeStatus::Clean => {}
    }

    // Gate-driven rows.
    match phase {
        Phase::Planning => waiting_from_gate(plan_gate, GatePhase::Plan),
        Phase::Implementing => waiting_from_gate(impl_gate, GatePhase::Impl),
        Phase::Done => unreachable!("done was handled above"),
    }
}

#[derive(Debug, Clone, Copy)]
enum GatePhase {
    Plan,
    Impl,
}

fn waiting_from_gate(gate: Option<&ReviewGateDecision>, phase: GatePhase) -> WaitingOn {
    let (rc_reason, ready_reason, initial_reason, rereview_reason) = match phase {
        GatePhase::Plan => (
            WaitingReason::AddressPlanRequestChanges,
            WaitingReason::ReadyToImplement,
            WaitingReason::PlanNeedsInitialReview,
            WaitingReason::PlanNeedsRereview,
        ),
        GatePhase::Impl => (
            WaitingReason::AddressImplRequestChanges,
            WaitingReason::ReadyToFinish,
            WaitingReason::ImplNeedsInitialReview,
            WaitingReason::ImplNeedsRereview,
        ),
    };

    let Some(gate) = gate else {
        // No gate (e.g., implementing phase with no impl commits yet —
        // should be rare, but cover it as initial review).
        return make(WaitingRole::Reviewers, initial_reason, Vec::new());
    };

    match gate.state {
        ReviewGateState::ChangesRequested => make(
            WaitingRole::Master,
            rc_reason,
            gate.request_changes.clone(),
        ),
        ReviewGateState::Ready => make(WaitingRole::Master, ready_reason, Vec::new()),
        ReviewGateState::NeedsReview => {
            if gate.participants.is_empty() {
                make(WaitingRole::Reviewers, initial_reason, Vec::new())
            } else {
                make(
                    WaitingRole::Reviewers,
                    rereview_reason,
                    gate.missing_approvals.clone(),
                )
            }
        }
    }
}

fn make(role: WaitingRole, reason: WaitingReason, agents: Vec<crate::lifecycle::AgentLabel>) -> WaitingOn {
    let description = description_for(role, reason, &agents);
    WaitingOn {
        role,
        reason,
        agents,
        description,
    }
}

/// Canonical human-readable description for a `(role, reason)` pair.
/// Used identically by MCP responses and the web UI so the two never drift.
fn description_for(
    role: WaitingRole,
    reason: WaitingReason,
    agents: &[crate::lifecycle::AgentLabel],
) -> String {
    use WaitingReason::*;
    use WaitingRole::*;
    let agent_list = || -> String {
        agents
            .iter()
            .map(|a| a.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    match (role, reason) {
        (None, SessionDone) => "Session is done.".to_string(),
        (Master, CommitDoneMove) => {
            "Plan was moved to `done/` but the move isn't committed yet. \
             Stage and commit to retire the session."
                .to_string()
        }
        (Master, RestoreOrCommitDoneMove) => {
            "Active plan file is missing from the working tree. \
             Either restore it (`git checkout -- <path>`) or move it to `done/` and commit."
                .to_string()
        }
        (Master, CommitPlanRevision) => {
            "Plan has uncommitted changes. \
             Commit the revision; held plan reviews will release."
                .to_string()
        }
        (Master, AddressPlanRequestChanges) => {
            if agents.is_empty() {
                "Plan review requested changes. Address them and commit a new revision.".to_string()
            } else {
                format!(
                    "Plan review requested changes ({}). Address them and commit a new revision.",
                    agent_list()
                )
            }
        }
        (Master, ReadyToImplement) => {
            "Plan approved. Ready to start implementation; make the first impl commit."
                .to_string()
        }
        (Reviewers, PlanNeedsInitialReview) => {
            "Plan is committed and awaiting an initial review.".to_string()
        }
        (Reviewers, PlanNeedsRereview) => {
            if agents.is_empty() {
                "Plan was revised; awaiting re-review.".to_string()
            } else {
                format!(
                    "Plan was revised; awaiting re-review from {}.",
                    agent_list()
                )
            }
        }
        (Master, AddressImplRequestChanges) => {
            if agents.is_empty() {
                "Implementation review requested changes. Address them and commit.".to_string()
            } else {
                format!(
                    "Implementation review requested changes ({}). Address them and commit.",
                    agent_list()
                )
            }
        }
        (Master, ReadyToFinish) => {
            "Implementation approved. Ready to finish; move plan to `done/` and commit.".to_string()
        }
        (Reviewers, ImplNeedsInitialReview) => {
            "Implementation commit awaiting an initial review.".to_string()
        }
        (Reviewers, ImplNeedsRereview) => {
            if agents.is_empty() {
                "Implementation was revised; awaiting re-review.".to_string()
            } else {
                format!(
                    "Implementation was revised; awaiting re-review from {}.",
                    agent_list()
                )
            }
        }
        // Defensive — shouldn't be reachable since make() pairs role with
        // a compatible reason. If it ever fires, we surface enough to debug.
        (role, reason) => format!("{} / {}", role.as_str(), reason.as_str()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::lifecycle::AgentLabel;
    use crate::review_state::{ReviewGateDecision, ReviewGateState, ReviewPhase};

    fn hash(s: &str) -> ContentHash {
        ContentHash::from(format!("h:{s}"))
    }

    fn agents(labels: &[&str]) -> Vec<AgentLabel> {
        labels
            .iter()
            .map(|s| AgentLabel::from(s.to_string()))
            .collect()
    }

    fn gate(
        phase: ReviewPhase,
        state: ReviewGateState,
        participants: Vec<AgentLabel>,
        approvals: Vec<AgentLabel>,
        request_changes: Vec<AgentLabel>,
        missing_approvals: Vec<AgentLabel>,
    ) -> ReviewGateDecision {
        ReviewGateDecision {
            phase,
            state,
            approval_rule: "all_participants",
            participants,
            approvals,
            request_changes,
            unmarked: Vec::new(),
            missing_approvals,
            override_status: None,
        }
    }

    // -------- plan_worktree_status --------

    #[test]
    fn worktree_clean_when_hashes_match() {
        let h = hash("a");
        assert_eq!(
            plan_worktree_status(Some(&h), Some(&h), false),
            PlanWorktreeStatus::Clean
        );
    }

    #[test]
    fn worktree_body_dirty_when_hashes_differ() {
        let h1 = hash("a");
        let h2 = hash("b");
        assert_eq!(
            plan_worktree_status(Some(&h1), Some(&h2), false),
            PlanWorktreeStatus::BodyDirty
        );
    }

    #[test]
    fn worktree_done_move_pending_when_active_missing_done_exists() {
        let h = hash("a");
        assert_eq!(
            plan_worktree_status(Some(&h), None, true),
            PlanWorktreeStatus::DoneMovePending
        );
    }

    #[test]
    fn worktree_missing_active_when_both_missing() {
        let h = hash("a");
        assert_eq!(
            plan_worktree_status(Some(&h), None, false),
            PlanWorktreeStatus::MissingActivePlanFile
        );
    }

    // -------- waiting_on --------

    #[test]
    fn waiting_session_done_when_phase_done() {
        let w = waiting_on(Phase::Done, PlanWorktreeStatus::Clean, None, None);
        assert_eq!(w.role, WaitingRole::None);
        assert_eq!(w.reason, WaitingReason::SessionDone);
    }

    #[test]
    fn waiting_commit_done_move_preempts_gate() {
        let g = gate(
            ReviewPhase::Plan,
            ReviewGateState::Ready,
            agents(&["alice"]),
            agents(&["alice"]),
            Vec::new(),
            Vec::new(),
        );
        let w = waiting_on(
            Phase::Planning,
            PlanWorktreeStatus::DoneMovePending,
            Some(&g),
            None,
        );
        assert_eq!(w.role, WaitingRole::Master);
        assert_eq!(w.reason, WaitingReason::CommitDoneMove);
    }

    #[test]
    fn waiting_restore_or_commit_done_move() {
        let w = waiting_on(
            Phase::Planning,
            PlanWorktreeStatus::MissingActivePlanFile,
            None,
            None,
        );
        assert_eq!(w.role, WaitingRole::Master);
        assert_eq!(w.reason, WaitingReason::RestoreOrCommitDoneMove);
    }

    #[test]
    fn waiting_commit_plan_revision_preempts_gate() {
        let g = gate(
            ReviewPhase::Plan,
            ReviewGateState::Ready,
            agents(&["alice"]),
            agents(&["alice"]),
            Vec::new(),
            Vec::new(),
        );
        let w = waiting_on(
            Phase::Planning,
            PlanWorktreeStatus::BodyDirty,
            Some(&g),
            None,
        );
        assert_eq!(w.role, WaitingRole::Master);
        assert_eq!(w.reason, WaitingReason::CommitPlanRevision);
    }

    #[test]
    fn waiting_plan_initial_review_no_participants() {
        let g = gate(
            ReviewPhase::Plan,
            ReviewGateState::NeedsReview,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );
        let w = waiting_on(Phase::Planning, PlanWorktreeStatus::Clean, Some(&g), None);
        assert_eq!(w.role, WaitingRole::Reviewers);
        assert_eq!(w.reason, WaitingReason::PlanNeedsInitialReview);
        assert!(w.agents.is_empty());
    }

    #[test]
    fn waiting_plan_rereview_with_stale_participants() {
        let g = gate(
            ReviewPhase::Plan,
            ReviewGateState::NeedsReview,
            agents(&["alice", "bob"]),
            Vec::new(),
            Vec::new(),
            agents(&["alice", "bob"]),
        );
        let w = waiting_on(Phase::Planning, PlanWorktreeStatus::Clean, Some(&g), None);
        assert_eq!(w.role, WaitingRole::Reviewers);
        assert_eq!(w.reason, WaitingReason::PlanNeedsRereview);
        assert_eq!(w.agents.len(), 2);
    }

    #[test]
    fn waiting_plan_request_changes_to_master() {
        let g = gate(
            ReviewPhase::Plan,
            ReviewGateState::ChangesRequested,
            agents(&["alice", "bob"]),
            Vec::new(),
            agents(&["bob"]),
            Vec::new(),
        );
        let w = waiting_on(Phase::Planning, PlanWorktreeStatus::Clean, Some(&g), None);
        assert_eq!(w.role, WaitingRole::Master);
        assert_eq!(w.reason, WaitingReason::AddressPlanRequestChanges);
        assert_eq!(w.agents, agents(&["bob"]));
    }

    #[test]
    fn waiting_plan_ready_to_implement() {
        let g = gate(
            ReviewPhase::Plan,
            ReviewGateState::Ready,
            agents(&["alice"]),
            agents(&["alice"]),
            Vec::new(),
            Vec::new(),
        );
        let w = waiting_on(Phase::Planning, PlanWorktreeStatus::Clean, Some(&g), None);
        assert_eq!(w.role, WaitingRole::Master);
        assert_eq!(w.reason, WaitingReason::ReadyToImplement);
    }

    #[test]
    fn waiting_impl_initial_review() {
        let g = gate(
            ReviewPhase::Impl,
            ReviewGateState::NeedsReview,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );
        let w = waiting_on(Phase::Implementing, PlanWorktreeStatus::Clean, None, Some(&g));
        assert_eq!(w.role, WaitingRole::Reviewers);
        assert_eq!(w.reason, WaitingReason::ImplNeedsInitialReview);
    }

    #[test]
    fn waiting_impl_request_changes() {
        let g = gate(
            ReviewPhase::Impl,
            ReviewGateState::ChangesRequested,
            agents(&["alice"]),
            Vec::new(),
            agents(&["alice"]),
            Vec::new(),
        );
        let w = waiting_on(Phase::Implementing, PlanWorktreeStatus::Clean, None, Some(&g));
        assert_eq!(w.role, WaitingRole::Master);
        assert_eq!(w.reason, WaitingReason::AddressImplRequestChanges);
        assert_eq!(w.agents, agents(&["alice"]));
    }

    #[test]
    fn waiting_impl_ready_to_finish() {
        let g = gate(
            ReviewPhase::Impl,
            ReviewGateState::Ready,
            agents(&["alice"]),
            agents(&["alice"]),
            Vec::new(),
            Vec::new(),
        );
        let w = waiting_on(Phase::Implementing, PlanWorktreeStatus::Clean, None, Some(&g));
        assert_eq!(w.role, WaitingRole::Master);
        assert_eq!(w.reason, WaitingReason::ReadyToFinish);
    }

    #[test]
    fn description_includes_agent_list_when_present() {
        let g = gate(
            ReviewPhase::Plan,
            ReviewGateState::ChangesRequested,
            agents(&["alice", "bob"]),
            Vec::new(),
            agents(&["bob"]),
            Vec::new(),
        );
        let w = waiting_on(Phase::Planning, PlanWorktreeStatus::Clean, Some(&g), None);
        assert!(
            w.description.contains("bob"),
            "description should reference the requesting reviewer; got: {}",
            w.description
        );
    }
}
