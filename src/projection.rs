//! Pure projections used by MCP, HTTP, and SSE: posture derivation,
//! plan worktree status from hash comparisons, gate derivation, and
//! the `waiting_on` value.
//!
//! All consumers (MCP `work_context`, HTTP routes, SSE payload
//! construction, `wait_for_work` matching) call into these functions.
//!
//! Each function takes `&Plan` + `&RepoState`: the plan's timeline
//! supplies the chronological SHA list, and `RepoState::gate_for(sha)`
//! returns the per-commit gate from the authoritative
//! `RepoState.commits` map (Phase 2 of `commit-first-review-model`).

use crate::lifecycle::ContentHash;
use crate::repo_state::{
    CommitKind, Plan, PlanWorktreeStatus, Posture, WaitingOn, WaitingReason, WaitingRole,
};
use crate::review_state::{CommitGate, CommitGateState};
use trinity_core::PlanLifecycle;

use crate::lifecycle::{CommitSha, PlanKey};

/// True iff `plan_key` appears in `state.fold.finished_plans`. The
/// new fold's authoritative answer to "is this plan finished?";
/// callers should prefer this over `Plan::is_frozen` so the legacy
/// timeline-based check can retire.
pub fn is_plan_finished(state: &crate::repo_state::RepoState, plan_key: &PlanKey) -> bool {
    state
        .fold
        .finished_plans
        .iter()
        .any(|f| &f.plan == plan_key)
}

/// `PlanLifecycle` derived from the new fold: `Finished` when the
/// plan is in `state.fold.finished_plans`, otherwise `Active`.
pub fn plan_lifecycle(state: &crate::repo_state::RepoState, plan_key: &PlanKey) -> PlanLifecycle {
    if is_plan_finished(state, plan_key) {
        PlanLifecycle::Finished
    } else {
        PlanLifecycle::Active
    }
}

/// Repo-relative path for a plan: `.trinity/plans/<key>.md`. The
/// only on-disk shape we accept for plan files.
pub fn plan_path_for(plan_key: &PlanKey) -> String {
    format!(".trinity/plans/{}.md", plan_key.as_str())
}

/// The intro commit's SHA for a plan, or `None` if the plan has no
/// folded events yet. Pulled from `state.fold.plans[key].commits[0]`.
pub fn plan_intro_sha<'a>(
    state: &'a crate::repo_state::RepoState,
    plan_key: &PlanKey,
) -> Option<&'a CommitSha> {
    state
        .fold
        .plans
        .get(plan_key)
        .and_then(|ps| ps.commits.first())
        .map(|e| &e.sha)
}

/// `plan_worktree_status` from hash comparisons. Pure.
pub fn plan_worktree_status(
    head_blob_hash: Option<&ContentHash>,
    worktree_body_hash: Option<&ContentHash>,
) -> PlanWorktreeStatus {
    match (head_blob_hash, worktree_body_hash) {
        (Some(h), Some(w)) if h == w => PlanWorktreeStatus::Clean,
        (Some(_), Some(_)) => PlanWorktreeStatus::BodyDirty,
        (Some(_), None) => PlanWorktreeStatus::PlanFileMissing,
        // Plan has no HEAD blob (shouldn't happen for tracked plans).
        (None, _) => PlanWorktreeStatus::Clean,
    }
}

/// Current posture for a plan: `Planning` while the master is
/// iterating on the plan body (`PlanOnly | Mixed` latest reviewable
/// commit, or nothing reviewable yet), `Implementing` once the master
/// is shipping code (`CodeOnly`). Reverse-scan over `plan.timeline`.
pub fn current_posture(plan: &Plan, _state: &crate::repo_state::RepoState) -> Posture {
    match plan.latest_reviewable_event() {
        Some(event) if event.kind() == CommitKind::CodeOnly => Posture::Implementing,
        _ => Posture::Planning,
    }
}

/// Compose the `waiting_on` value for a plan. Top rows
/// (worktree-status-driven) preempt the gate-driven row. The caller
/// passes the latest reviewable commit's gate (see
/// `latest_reviewable_commit_gate_for`).
pub fn waiting_on(
    is_finished: bool,
    worktree_status: PlanWorktreeStatus,
    gate: Option<&CommitGate>,
) -> WaitingOn {
    if is_finished {
        return make(
            WaitingRole::None,
            WaitingReason::SessionFinished,
            Vec::new(),
        );
    }
    match worktree_status {
        PlanWorktreeStatus::PlanFileMissing => {
            // Unreachable in normal flow: an active plan with its
            // working-tree file missing is hidden via `Plan::is_visible`
            // before any caller reaches `waiting_on`. If this fires,
            // a surface forgot to filter.
            unreachable!(
                "active plan with missing worktree file must be hidden via Plan::is_visible \
                 before reaching waiting_on"
            )
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
    waiting_from_gate(gate)
}

/// Latest reviewable commit's `CommitGate` for one plan. Reverse-scan
/// over `plan.timeline` for the SHA; Phase 2 of
/// `commit-first-review-model` puts the gate on `RepoState.commits`,
/// so the lookup threads through `state.gate_for(sha)`.
pub fn latest_reviewable_commit_gate_for<'a>(
    plan: &Plan,
    state: &'a crate::repo_state::RepoState,
) -> Option<&'a CommitGate> {
    let sha = plan.latest_reviewable_event()?.sha();
    state.gate_for(sha)
}

/// Latest commit whose `CommitKind` is reviewable for this plan.
/// Reverse-scan over `plan.timeline`.
pub fn latest_reviewable_commit_for(plan: &Plan) -> Option<CommitSha> {
    plan.latest_reviewable_event().map(|e| e.sha().clone())
}

fn waiting_from_gate(gate: Option<&CommitGate>) -> WaitingOn {
    let Some(gate) = gate else {
        // No gate yet (a plan with no reviewable commit). Initial-review
        // case with no participants — description prose says "awaiting
        // initial review."
        return make(
            WaitingRole::Reviewers,
            WaitingReason::CommitNeedsReview,
            Vec::new(),
        );
    };

    match gate.state {
        CommitGateState::ChangesRequested => make(
            WaitingRole::Master,
            WaitingReason::AddressCommitChanges,
            gate.requesters.clone(),
        ),
        CommitGateState::Approved => make(
            WaitingRole::Master,
            WaitingReason::ReadyToStartImplementation,
            Vec::new(),
        ),
        CommitGateState::Unreviewed => {
            let agents = if gate.participants.is_empty() {
                Vec::new()
            } else {
                gate.missing.clone()
            };
            make(
                WaitingRole::Reviewers,
                WaitingReason::CommitNeedsReview,
                agents,
            )
        }
    }
}

/// Most recent activity timestamp for one plan. The fold maintains
/// this incrementally on `Plan.last_activity_ts` (max over the plan's
/// attributed commit author_ts and feedback created_at). O(1) read.
pub fn last_activity_ts_for(plan: &Plan) -> i64 {
    plan.last_activity_ts
}

/// Plan-file path at any commit. With `done/` retired, the plan file
/// no longer moves over its history; this always returns the plan's
/// canonical path.
pub fn plan_path_at(plan: &Plan, _target_sha: &CommitSha) -> Option<std::path::PathBuf> {
    Some(std::path::PathBuf::from(&plan.plan_path))
}

/// All plan-touching commits for this plan, in chronological order.
/// Filter over `plan.timeline`.
pub fn all_plan_revisions(plan: &Plan, _state: &crate::repo_state::RepoState) -> Vec<CommitSha> {
    plan.timeline
        .iter()
        .filter(|e| {
            matches!(
                e.kind(),
                CommitKind::PlanOnly | CommitKind::Mixed | CommitKind::MultiPlan
            )
        })
        .map(|e| e.sha().clone())
        .collect()
}

/// All implementation commits attributed to this plan, in
/// chronological order. Filter over `plan.timeline`.
pub fn all_implementation_commits(
    plan: &Plan,
    _state: &crate::repo_state::RepoState,
) -> Vec<CommitSha> {
    plan.timeline
        .iter()
        .filter(|e| matches!(e.kind(), CommitKind::CodeOnly | CommitKind::Mixed))
        .map(|e| e.sha().clone())
        .collect()
}

/// Plan-side review gate: the latest reviewable plan-touching commit
/// (`PlanOnly` or `Mixed`) for this plan. Returns `None` if no such
/// commit exists. Used by the legacy plan/impl-tagged wire shape;
/// the new shape uses `latest_reviewable_commit_gate_for` directly.
pub fn plan_gate_for<'a>(
    plan: &Plan,
    state: &'a crate::repo_state::RepoState,
) -> Option<&'a CommitGate> {
    let sha = plan
        .timeline
        .iter()
        .rev()
        .find(|e| matches!(e.kind(), CommitKind::PlanOnly | CommitKind::Mixed))?
        .sha();
    state.gate_for(sha)
}

/// Implementation-side review gate: the latest reviewable code-bearing
/// commit (`CodeOnly` or `Mixed`) attributed to this plan.
pub fn impl_gate_for<'a>(
    plan: &Plan,
    state: &'a crate::repo_state::RepoState,
) -> Option<&'a CommitGate> {
    let sha = plan
        .timeline
        .iter()
        .rev()
        .find(|e| matches!(e.kind(), CommitKind::CodeOnly | CommitKind::Mixed))?
        .sha();
    state.gate_for(sha)
}

fn make(
    role: WaitingRole,
    reason: WaitingReason,
    agents: Vec<crate::lifecycle::AgentLabel>,
) -> WaitingOn {
    let description = description_for(role, reason, &agents);
    WaitingOn {
        role,
        reason,
        agents,
        description,
    }
}

/// Canonical human-readable description for a `(role, reason)` pair.
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
        (None, SessionFinished) => "Plan is finished.".to_string(),
        (Master, CommitPlanRevision) => "Plan has uncommitted changes. \
             Commit the revision to release any blocked reviews."
            .to_string(),
        (Master, AddressCommitChanges) => {
            if agents.is_empty() {
                "Review requested changes on the latest commit. Address them and commit."
                    .to_string()
            } else {
                format!(
                    "Review requested changes ({}) on the latest commit. Address them and commit.",
                    agent_list()
                )
            }
        }
        (Master, ReadyToStartImplementation) => {
            "Latest commit approved. Continue with the next commit or finalize the plan."
                .to_string()
        }
        (Reviewers, CommitNeedsReview) => {
            if agents.is_empty() {
                "Latest commit awaiting an initial review.".to_string()
            } else {
                format!("Latest commit awaiting review from {}.", agent_list())
            }
        }
        // Defensive — shouldn't be reachable since make() pairs role with
        // a compatible reason. If it ever fires, we surface enough to debug.
        (role, reason) => format!("{} / {}", role.as_str(), reason.as_str()),
    }
}

/// Classify a single commit's relevance to one plan. The fold stored
/// the kind on the timeline event when it appended it; this just looks
/// it up. Returns `Unattributed` when the commit isn't on this plan's
/// timeline at all.
pub fn commit_kind_for(plan: &Plan, sha: &CommitSha) -> CommitKind {
    plan.event_for(sha)
        .map(|e| e.kind())
        .unwrap_or(CommitKind::Unattributed)
}
