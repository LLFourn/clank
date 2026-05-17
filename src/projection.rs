//! Pure projections used by MCP, HTTP, and SSE: posture derivation,
//! plan worktree status from hash comparisons, gate derivation, and
//! the `waiting_on` value.
//!
//! All consumers (MCP `get_context`, HTTP routes, SSE payload
//! construction, `wait_for_work` matching) call into these functions.
//!
//! Everything here is O(1) or O(plan-local) on per-plan fields the
//! fold (`disk_snapshot::apply_commit`) has already accumulated. No
//! function in this module walks a global commit list or per-commit
//! lookup map — `RepoState` doesn't carry those anymore.

use crate::lifecycle::ContentHash;
use crate::repo_state::{
    CommitKind, Plan, PlanWorktreeStatus, Posture, WaitingOn, WaitingReason, WaitingRole,
};
use crate::review_state::{CommitGate, CommitGateState};

use crate::lifecycle::CommitSha;

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
/// is shipping code (`CodeOnly`). Field read on `Plan`.
pub fn current_posture(plan: &Plan, _state: &crate::repo_state::RepoState) -> Posture {
    match plan.latest_reviewable_commit.as_ref() {
        Some(sha) => match commit_kind_for(plan, sha) {
            CommitKind::CodeOnly => Posture::Implementing,
            _ => Posture::Planning,
        },
        None => Posture::Planning,
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
        return make(WaitingRole::None, WaitingReason::SessionFinished, Vec::new());
    }
    match worktree_status {
        PlanWorktreeStatus::PlanFileMissing => {
            return make(
                WaitingRole::Master,
                WaitingReason::RestoreOrCommitPlanFile,
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
    waiting_from_gate(gate)
}

/// Latest reviewable commit's `CommitGate` for one plan. O(1) — reads
/// the cached `latest_reviewable_commit` and looks up its gate.
/// Returns `None` when no reviewable commit exists yet.
pub fn latest_reviewable_commit_gate_for(plan: &Plan) -> Option<&CommitGate> {
    plan.latest_reviewable_commit
        .as_ref()
        .and_then(|sha| plan.commits.get(sha))
}

/// Latest commit whose `CommitKind` is reviewable for this plan. O(1)
/// field read.
pub fn latest_reviewable_commit_for(plan: &Plan) -> Option<CommitSha> {
    plan.latest_reviewable_commit.clone()
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
    Some(plan.plan_path.clone())
}

/// All plan-touching commits for this plan, in chronological order.
/// Read from the per-plan field; no global scan.
pub fn all_plan_revisions(plan: &Plan, _state: &crate::repo_state::RepoState) -> Vec<CommitSha> {
    plan.plan_revisions.clone()
}

/// All implementation commits attributed to this plan, in
/// chronological order. Read from the per-plan field; no global
/// scan.
pub fn all_implementation_commits(
    plan: &Plan,
    _state: &crate::repo_state::RepoState,
) -> Vec<CommitSha> {
    plan.implementation_commits.clone()
}

/// Plan-side review gate: the latest reviewable plan-touching commit
/// (`PlanOnly` or `Mixed`) for this plan. Returns `None` if no such
/// commit exists. Used by the legacy plan/impl-tagged wire shape;
/// the new shape uses `latest_reviewable_commit_gate_for` directly.
pub fn plan_gate_for<'a>(
    plan: &'a Plan,
    _state: &crate::repo_state::RepoState,
) -> Option<&'a CommitGate> {
    // Walk plan_revisions newest-first; the first one that's also in
    // reviewable_commits is the latest reviewable plan-touch. (Some
    // plan_revisions are MultiPlan and therefore not reviewable.)
    plan.plan_revisions
        .iter()
        .rev()
        .find(|sha| plan.reviewable_commits.iter().any(|s| s == *sha))
        .and_then(|sha| plan.commits.get(sha))
}

/// Implementation-side review gate: the latest reviewable code-bearing
/// commit (`CodeOnly` or `Mixed`) attributed to this plan.
pub fn impl_gate_for<'a>(
    plan: &'a Plan,
    _state: &crate::repo_state::RepoState,
) -> Option<&'a CommitGate> {
    // All entries in implementation_commits are reviewable (CodeOnly
    // and Mixed are both reviewable). Walk newest-first.
    plan.implementation_commits
        .iter()
        .next_back()
        .and_then(|sha| plan.commits.get(sha))
}

/// Map a `WaitingReason` to the caller-facing action verb that names
/// what the agent should actually do.
pub fn expected_action(reason: WaitingReason) -> &'static str {
    use WaitingReason::*;
    match reason {
        SessionFinished => "none",
        RestoreOrCommitPlanFile => "restore_or_commit_plan_file",
        CommitPlanRevision => "commit_plan_revision",
        AddressCommitChanges => "address_commit_changes",
        ReadyToStartImplementation => "start_implementation",
        CommitNeedsReview => "review_commit",
    }
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
        (Master, RestoreOrCommitPlanFile) => "Plan file is missing from the working tree. \
             Either restore it (`git checkout -- <path>`) or commit the deletion."
            .to_string(),
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

/// Classify a single commit's relevance to one plan. Pure; derived
/// from per-plan accumulated state.
pub fn commit_kind_for(plan: &Plan, sha: &CommitSha) -> CommitKind {
    let touched = plan.plan_revisions.iter().any(|s| s == sha);
    let coded = plan.implementation_commits.iter().any(|s| s == sha);
    let reviewable = plan.reviewable_commits.iter().any(|s| s == sha);
    // A touch that's NOT in reviewable_commits = MultiPlan (the fold
    // appended to plan_revisions but skipped reviewable_commits for
    // multi-plan touches).
    if touched && !reviewable {
        return CommitKind::MultiPlan;
    }
    match (touched, coded) {
        (true, true) => CommitKind::Mixed,
        (true, false) => CommitKind::PlanOnly,
        (false, true) => CommitKind::CodeOnly,
        (false, false) => CommitKind::Unattributed,
    }
}
