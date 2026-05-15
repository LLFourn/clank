//! Pure projections used by MCP, HTTP, and SSE: phase derivation, plan
//! worktree status from hash comparisons, gate derivation, and the
//! `waiting_on` value.
//!
//! All consumers (MCP `get_context`, HTTP routes, SSE payload construction,
//! `wait_for_work` matching, tests) call into these functions so the
//! derivation logic stays in one place and can't drift between surfaces.

use std::path::Path;

use crate::lifecycle::{AgentLabel, ContentHash, is_done_plan_path};
use crate::repo_state::{
    AttributionResult, Feedback, Phase, Plan, PlanWorktreeStatus, WaitingOn, WaitingReason,
    WaitingRole,
};
use crate::review_state::{ReviewGateDecision, ReviewGateState, ReviewPhase};

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
pub fn phase(plan: &Plan, attribution: &BTreeMap<CommitSha, AttributionResult>) -> Phase {
    phase_for(&plan.plan_path, &plan.id, attribution)
}

/// Phase derivation from primitive inputs. Used by callers that hold a
/// snapshot (e.g. `ui_response`) rather than a `&Plan`.
pub fn phase_for(
    plan_path: &Path,
    plan_key: &crate::lifecycle::PlanKey,
    attribution: &BTreeMap<CommitSha, AttributionResult>,
) -> Phase {
    if is_done_plan_path(plan_path) {
        return Phase::Done;
    }
    for attr in attribution.values() {
        if let AttributionResult::Attributed {
            session: sid,
            has_code_changes: true,
            ..
        } = attr
            && sid == plan_key
        {
            return Phase::Implementing;
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
            return make(
                WaitingRole::Master,
                WaitingReason::CommitDoneMove,
                Vec::new(),
            );
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
        ReviewGateState::ChangesRequested => {
            make(WaitingRole::Master, rc_reason, gate.request_changes.clone())
        }
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

/// Most recent activity timestamp for one plan, computed as the
/// maximum of:
///
/// - author_ts of the newest commit attributed to (or touching) this
///   plan,
/// - mtime of the newest feedback file (plan / impl / held),
/// - author_ts of the plan_intro commit (always defined).
///
/// Used by `/api/plans` to sort the homepage by recency. Pure;
/// sans-IO. Returns `0` only when none of the inputs carries a
/// timestamp — should not happen for committed plans since
/// plan_intro always has metadata.
#[allow(clippy::too_many_arguments)]
pub fn last_activity_ts_for(
    plan_key: &crate::lifecycle::PlanKey,
    plan_intro: &CommitSha,
    plan_feedback: &BTreeMap<(CommitSha, AgentLabel), Feedback>,
    impl_feedback: &BTreeMap<(CommitSha, AgentLabel), Feedback>,
    held_plan_feedback: &[crate::repo_state::HeldFeedback],
    commit_order: &[CommitSha],
    plan_touches: &BTreeMap<
        CommitSha,
        Vec<(crate::lifecycle::PlanKey, crate::repo_state::PlanTouchKind)>,
    >,
    attribution: &BTreeMap<CommitSha, AttributionResult>,
    commit_meta: &BTreeMap<CommitSha, crate::disk_snapshot::CommitMetaEntry>,
) -> i64 {
    let mut max_ts: i64 = 0;
    for sha in commit_order {
        let touches = plan_touches
            .get(sha)
            .is_some_and(|t| t.iter().any(|(k, _)| k == plan_key));
        let code = matches!(
            attribution.get(sha),
            Some(AttributionResult::Attributed { session, has_code_changes: true, .. })
                if session == plan_key
        );
        if !touches && !code {
            continue;
        }
        if let Some(meta) = commit_meta.get(sha) {
            max_ts = max_ts.max(meta.author_ts);
        }
    }
    for fb in plan_feedback.values() {
        max_ts = max_ts.max(fb.created_at);
    }
    for fb in impl_feedback.values() {
        max_ts = max_ts.max(fb.created_at);
    }
    for held in held_plan_feedback {
        max_ts = max_ts.max(held.created_at);
    }
    if let Some(intro_meta) = commit_meta.get(plan_intro) {
        max_ts = max_ts.max(intro_meta.author_ts);
    }
    max_ts
}

/// Repo-relative path of the plan file at the given commit's tree.
///
/// Walks `plan_touches` along `commit_order` from the start to (and
/// including) `target_sha`, toggling between active and done on every
/// `DoneMove` touch. `DoneMove` is direction-agnostic at the producer
/// (`git_io::classify_plan_touch` emits it for either direction), so
/// the toggle is symmetric — an active→done→active round trip ends at
/// active. Used by revision/diff routes to look up historical blobs
/// without assuming the plan file lived at its current path for the
/// whole history.
///
/// **Producer invariant:** `plan_touches[sha]` carries at most one
/// `(plan_key, DoneMove)` entry per commit. Git's rename detection
/// pairs one source to one destination per commit, so the upstream
/// `parse_diff_tree` walk in `git_io` cannot emit duplicates today.
/// If a future producer relaxes that, the XOR will silently cancel.
///
/// Returns `None` if `target_sha` isn't in `commit_order`.
pub fn plan_path_at(
    plan_key: &crate::lifecycle::PlanKey,
    target_sha: &CommitSha,
    commit_order: &[CommitSha],
    plan_touches: &BTreeMap<
        CommitSha,
        Vec<(crate::lifecycle::PlanKey, crate::repo_state::PlanTouchKind)>,
    >,
) -> Option<std::path::PathBuf> {
    let target_pos = commit_order.iter().position(|c| c == target_sha)?;
    let stem = plan_key.as_str();
    let mut is_done = false;
    for sha in &commit_order[..=target_pos] {
        if let Some(touches) = plan_touches.get(sha) {
            for (k, kind) in touches {
                if k == plan_key && matches!(kind, crate::repo_state::PlanTouchKind::DoneMove) {
                    is_done = !is_done;
                }
            }
        }
    }
    Some(if is_done {
        std::path::PathBuf::from(format!(".trinity/plans/done/{stem}.md"))
    } else {
        std::path::PathBuf::from(format!(".trinity/plans/{stem}.md"))
    })
}

/// All plan-touching commits attributed to `session`, in chronological
/// order (first-parent walk, oldest first).
pub fn all_plan_revisions(plan: &Plan, state: &crate::repo_state::RepoState) -> Vec<CommitSha> {
    all_plan_revisions_for(&plan.id, &state.commit_order, &state.plan_touches)
}

/// Same as `all_plan_revisions` but over primitive inputs (no `RepoState`).
pub fn all_plan_revisions_for(
    plan_key: &crate::lifecycle::PlanKey,
    commit_order: &[CommitSha],
    plan_touches: &BTreeMap<
        CommitSha,
        Vec<(crate::lifecycle::PlanKey, crate::repo_state::PlanTouchKind)>,
    >,
) -> Vec<CommitSha> {
    commit_order
        .iter()
        .filter(|sha| {
            plan_touches
                .get(*sha)
                .is_some_and(|touches| touches.iter().any(|(sid, _)| sid == plan_key))
        })
        .cloned()
        .collect()
}

/// All implementation commits (has_code_changes == true) attributed to
/// `session`, in chronological order.
pub fn all_implementation_commits(
    plan: &Plan,
    state: &crate::repo_state::RepoState,
) -> Vec<CommitSha> {
    all_implementation_commits_for(&plan.id, &state.commit_order, &state.attribution)
}

/// Same as `all_implementation_commits` but over primitive inputs.
pub fn all_implementation_commits_for(
    plan_key: &crate::lifecycle::PlanKey,
    commit_order: &[CommitSha],
    attribution: &BTreeMap<CommitSha, AttributionResult>,
) -> Vec<CommitSha> {
    commit_order
        .iter()
        .filter(|sha| {
            matches!(
                attribution.get(*sha),
                Some(AttributionResult::Attributed {
                    session: sid,
                    has_code_changes: true,
                    ..
                }) if sid == plan_key
            )
        })
        .cloned()
        .collect()
}

pub fn latest_plan_touching_commit(
    plan: &Plan,
    state: &crate::repo_state::RepoState,
) -> Option<CommitSha> {
    all_plan_revisions(plan, state).into_iter().last()
}

pub fn latest_plan_touching_commit_for(
    plan_key: &crate::lifecycle::PlanKey,
    commit_order: &[CommitSha],
    plan_touches: &BTreeMap<
        CommitSha,
        Vec<(crate::lifecycle::PlanKey, crate::repo_state::PlanTouchKind)>,
    >,
) -> Option<CommitSha> {
    all_plan_revisions_for(plan_key, commit_order, plan_touches)
        .into_iter()
        .last()
}

pub fn latest_impl_commit(plan: &Plan, state: &crate::repo_state::RepoState) -> Option<CommitSha> {
    all_implementation_commits(plan, state).into_iter().last()
}

pub fn latest_impl_commit_for(
    plan_key: &crate::lifecycle::PlanKey,
    commit_order: &[CommitSha],
    attribution: &BTreeMap<CommitSha, AttributionResult>,
) -> Option<CommitSha> {
    all_implementation_commits_for(plan_key, commit_order, attribution)
        .into_iter()
        .last()
}

/// Plan-phase review gate built from the session's `plan_feedback`
/// targeting the current plan commit. Returns `None` if the session has
/// no plan-touching commits yet.
pub fn plan_gate_for(
    plan: &Plan,
    state: &crate::repo_state::RepoState,
) -> Option<ReviewGateDecision> {
    plan_gate_for_parts(
        &plan.id,
        &plan.plan_feedback,
        &state.commit_order,
        &state.plan_touches,
    )
}

/// Same as `plan_gate_for` but over primitive inputs.
pub fn plan_gate_for_parts(
    plan_key: &crate::lifecycle::PlanKey,
    plan_feedback: &BTreeMap<(CommitSha, AgentLabel), Feedback>,
    commit_order: &[CommitSha],
    plan_touches: &BTreeMap<
        CommitSha,
        Vec<(crate::lifecycle::PlanKey, crate::repo_state::PlanTouchKind)>,
    >,
) -> Option<ReviewGateDecision> {
    let current_target = latest_plan_touching_commit_for(plan_key, commit_order, plan_touches)?;
    Some(derive_gate_from_feedback(
        ReviewPhase::Plan,
        &current_target,
        plan_feedback,
    ))
}

/// Implementation-phase review gate built from the session's
/// `impl_feedback` targeting the latest impl commit. Returns `None` if
/// the session has no impl commits yet.
pub fn impl_gate_for(
    plan: &Plan,
    state: &crate::repo_state::RepoState,
) -> Option<ReviewGateDecision> {
    impl_gate_for_parts(
        &plan.id,
        &plan.impl_feedback,
        &state.commit_order,
        &state.attribution,
    )
}

/// Same as `impl_gate_for` but over primitive inputs.
pub fn impl_gate_for_parts(
    plan_key: &crate::lifecycle::PlanKey,
    impl_feedback: &BTreeMap<(CommitSha, AgentLabel), Feedback>,
    commit_order: &[CommitSha],
    attribution: &BTreeMap<CommitSha, AttributionResult>,
) -> Option<ReviewGateDecision> {
    let current_target = latest_impl_commit_for(plan_key, commit_order, attribution)?;
    Some(derive_gate_from_feedback(
        ReviewPhase::Impl,
        &current_target,
        impl_feedback,
    ))
}

/// Build a `ReviewGateDecision` directly from the session's in-memory
/// feedback map, using SHA-anchored verdicts.
fn derive_gate_from_feedback(
    phase: ReviewPhase,
    current_target: &CommitSha,
    feedback: &BTreeMap<(CommitSha, AgentLabel), Feedback>,
) -> ReviewGateDecision {
    let mut participants: Vec<AgentLabel> = Vec::new();
    let mut approvals: Vec<AgentLabel> = Vec::new();
    let mut request_changes: Vec<AgentLabel> = Vec::new();
    let mut unmarked: Vec<AgentLabel> = Vec::new();
    let mut current_approvers: std::collections::BTreeSet<AgentLabel> =
        std::collections::BTreeSet::new();
    let mut current_blockers: std::collections::BTreeSet<AgentLabel> =
        std::collections::BTreeSet::new();

    for ((target_sha, author), fb) in feedback {
        if matches!(
            fb.verdict,
            crate::repo_state::Verdict::Approve | crate::repo_state::Verdict::RequestChanges
        ) && !participants.contains(author)
        {
            participants.push(author.clone());
        }
        if target_sha == current_target {
            match fb.verdict {
                crate::repo_state::Verdict::Approve => {
                    current_approvers.insert(author.clone());
                }
                crate::repo_state::Verdict::RequestChanges => {
                    current_blockers.insert(author.clone());
                }
                crate::repo_state::Verdict::Unmarked => {
                    if !unmarked.contains(author) {
                        unmarked.push(author.clone());
                    }
                }
            }
        }
    }

    approvals.extend(current_approvers);
    request_changes.extend(current_blockers);

    let missing: Vec<AgentLabel> = participants
        .iter()
        .filter(|p| !approvals.contains(p) && !request_changes.contains(p))
        .cloned()
        .collect();

    let state = if !request_changes.is_empty() {
        ReviewGateState::ChangesRequested
    } else if !approvals.is_empty() && missing.is_empty() {
        ReviewGateState::Ready
    } else {
        ReviewGateState::NeedsReview
    };

    ReviewGateDecision {
        phase,
        state,
        approval_rule: "all_participants",
        participants,
        approvals,
        request_changes,
        unmarked,
        missing_approvals: missing,
    }
}

/// Map a `WaitingReason` to the caller-facing action verb that names
/// what the agent should actually do. Shared by MCP responses, the
/// `wait_for_work` `work` field, and the web UI so the vocabulary stays
/// in one place. All imperatives — read as "(go) X".
pub fn expected_action(reason: WaitingReason) -> &'static str {
    use WaitingReason::*;
    match reason {
        SessionDone => "none",
        CommitDoneMove => "commit_done_move",
        RestoreOrCommitDoneMove => "restore_or_commit_done_move",
        CommitPlanRevision => "commit_plan_revision",
        AddressPlanRequestChanges => "address_plan_request_changes",
        ReadyToImplement => "implement_and_commit",
        PlanNeedsInitialReview | PlanNeedsRereview => "review_plan",
        AddressImplRequestChanges => "address_impl_request_changes",
        ReadyToFinish => "move_to_done",
        ImplNeedsInitialReview | ImplNeedsRereview => "review_impl",
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
        (Master, CommitDoneMove) => "Plan was moved to `done/` but the move isn't committed yet. \
             Stage and commit to retire the session."
            .to_string(),
        (Master, RestoreOrCommitDoneMove) => "Active plan file is missing from the working tree. \
             Either restore it (`git checkout -- <path>`) or move it to `done/` and commit."
            .to_string(),
        (Master, CommitPlanRevision) => "Plan has uncommitted changes. \
             Commit the revision; held plan reviews will release."
            .to_string(),
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
            "Plan approved. Ready to start implementation; make the first impl commit.".to_string()
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

    use crate::lifecycle::{AgentLabel, PlanKey};
    use crate::repo_state::PlanTouchKind;
    use crate::review_state::{ReviewGateDecision, ReviewGateState, ReviewPhase};
    use std::path::PathBuf;

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
        let w = waiting_on(
            Phase::Implementing,
            PlanWorktreeStatus::Clean,
            None,
            Some(&g),
        );
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
        let w = waiting_on(
            Phase::Implementing,
            PlanWorktreeStatus::Clean,
            None,
            Some(&g),
        );
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
        let w = waiting_on(
            Phase::Implementing,
            PlanWorktreeStatus::Clean,
            None,
            Some(&g),
        );
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

    // -------- plan_path_at --------

    fn cs(s: &str) -> CommitSha {
        CommitSha::from(s)
    }

    fn pk(s: &str) -> PlanKey {
        PlanKey::from(s)
    }

    #[test]
    fn plan_path_at_returns_active_before_any_done_move() {
        let order = vec![cs("c1"), cs("c2")];
        let mut touches: BTreeMap<CommitSha, Vec<(PlanKey, PlanTouchKind)>> = BTreeMap::new();
        touches.insert(cs("c1"), vec![(pk("foo"), PlanTouchKind::Intro)]);
        touches.insert(cs("c2"), vec![(pk("foo"), PlanTouchKind::Revision)]);
        let p = plan_path_at(&pk("foo"), &cs("c2"), &order, &touches).unwrap();
        assert_eq!(p, PathBuf::from(".trinity/plans/foo.md"));
    }

    #[test]
    fn plan_path_at_flips_to_done_at_done_move_commit() {
        let order = vec![cs("c1"), cs("c2"), cs("c3")];
        let mut touches: BTreeMap<CommitSha, Vec<(PlanKey, PlanTouchKind)>> = BTreeMap::new();
        touches.insert(cs("c1"), vec![(pk("foo"), PlanTouchKind::Intro)]);
        touches.insert(cs("c3"), vec![(pk("foo"), PlanTouchKind::DoneMove)]);

        assert_eq!(
            plan_path_at(&pk("foo"), &cs("c1"), &order, &touches).unwrap(),
            PathBuf::from(".trinity/plans/foo.md"),
        );
        assert_eq!(
            plan_path_at(&pk("foo"), &cs("c2"), &order, &touches).unwrap(),
            PathBuf::from(".trinity/plans/foo.md"),
        );
        assert_eq!(
            plan_path_at(&pk("foo"), &cs("c3"), &order, &touches).unwrap(),
            PathBuf::from(".trinity/plans/done/foo.md"),
        );
    }

    #[test]
    fn plan_path_at_unknown_sha_returns_none() {
        let order = vec![cs("c1")];
        let touches: BTreeMap<CommitSha, Vec<(PlanKey, PlanTouchKind)>> = BTreeMap::new();
        assert!(plan_path_at(&pk("foo"), &cs("zzz"), &order, &touches).is_none());
    }

    #[test]
    fn plan_path_at_toggles_on_each_done_move() {
        // The producer emits `DoneMove` for either direction. Round-trip
        // (active→done→active) must end at active.
        let order = vec![cs("c1"), cs("c2"), cs("c3"), cs("c4")];
        let mut touches: BTreeMap<CommitSha, Vec<(PlanKey, PlanTouchKind)>> = BTreeMap::new();
        touches.insert(cs("c1"), vec![(pk("foo"), PlanTouchKind::Intro)]);
        touches.insert(cs("c2"), vec![(pk("foo"), PlanTouchKind::DoneMove)]);
        touches.insert(cs("c4"), vec![(pk("foo"), PlanTouchKind::DoneMove)]);

        assert_eq!(
            plan_path_at(&pk("foo"), &cs("c2"), &order, &touches).unwrap(),
            PathBuf::from(".trinity/plans/done/foo.md"),
        );
        assert_eq!(
            plan_path_at(&pk("foo"), &cs("c3"), &order, &touches).unwrap(),
            PathBuf::from(".trinity/plans/done/foo.md"),
        );
        assert_eq!(
            plan_path_at(&pk("foo"), &cs("c4"), &order, &touches).unwrap(),
            PathBuf::from(".trinity/plans/foo.md"),
            "second DoneMove (done→active) must toggle back to active",
        );
    }

    #[test]
    fn plan_path_at_ignores_other_plans_done_move() {
        let order = vec![cs("c1"), cs("c2")];
        let mut touches: BTreeMap<CommitSha, Vec<(PlanKey, PlanTouchKind)>> = BTreeMap::new();
        touches.insert(cs("c1"), vec![(pk("foo"), PlanTouchKind::Intro)]);
        touches.insert(cs("c2"), vec![(pk("bar"), PlanTouchKind::DoneMove)]);
        assert_eq!(
            plan_path_at(&pk("foo"), &cs("c2"), &order, &touches).unwrap(),
            PathBuf::from(".trinity/plans/foo.md"),
        );
    }

    // -------- last_activity_ts_for --------

    fn meta(ts: i64, subject: &str) -> crate::disk_snapshot::CommitMetaEntry {
        crate::disk_snapshot::CommitMetaEntry {
            author_ts: ts,
            subject: subject.to_string(),
        }
    }

    fn fb(target: &str, author: &str, created_at: i64) -> ((CommitSha, AgentLabel), Feedback) {
        (
            (cs(target), AgentLabel::from(author)),
            Feedback {
                path: std::path::PathBuf::from("/fake"),
                body: String::new(),
                verdict: crate::repo_state::Verdict::Unmarked,
                created_at,
            },
        )
    }

    #[test]
    fn last_activity_ts_only_intro_commit_uses_intro_ts() {
        let mut commit_meta = BTreeMap::new();
        commit_meta.insert(cs("c1"), meta(1_000, "intro"));
        let mut touches: BTreeMap<CommitSha, Vec<(PlanKey, PlanTouchKind)>> = BTreeMap::new();
        touches.insert(cs("c1"), vec![(pk("foo"), PlanTouchKind::Intro)]);
        let ts = last_activity_ts_for(
            &pk("foo"),
            &cs("c1"),
            &BTreeMap::new(),
            &BTreeMap::new(),
            &[],
            &[cs("c1")],
            &touches,
            &BTreeMap::new(),
            &commit_meta,
        );
        assert_eq!(ts, 1_000);
    }

    #[test]
    fn last_activity_ts_picks_newer_of_commit_or_feedback() {
        let mut commit_meta = BTreeMap::new();
        commit_meta.insert(cs("c1"), meta(1_000, "intro"));
        commit_meta.insert(cs("c2"), meta(2_000, "rev"));
        let mut touches: BTreeMap<CommitSha, Vec<(PlanKey, PlanTouchKind)>> = BTreeMap::new();
        touches.insert(cs("c1"), vec![(pk("foo"), PlanTouchKind::Intro)]);
        touches.insert(cs("c2"), vec![(pk("foo"), PlanTouchKind::Revision)]);
        let mut plan_fb = BTreeMap::new();
        let (k, v) = fb("c1", "codex", 3_000);
        plan_fb.insert(k, v);
        let ts = last_activity_ts_for(
            &pk("foo"),
            &cs("c1"),
            &plan_fb,
            &BTreeMap::new(),
            &[],
            &[cs("c1"), cs("c2")],
            &touches,
            &BTreeMap::new(),
            &commit_meta,
        );
        assert_eq!(
            ts, 3_000,
            "feedback mtime should win when newer than commits"
        );
    }

    #[test]
    fn last_activity_ts_ignores_commits_for_other_plans() {
        let mut commit_meta = BTreeMap::new();
        commit_meta.insert(cs("c1"), meta(1_000, "foo intro"));
        commit_meta.insert(cs("c2"), meta(5_000, "bar intro"));
        let mut touches: BTreeMap<CommitSha, Vec<(PlanKey, PlanTouchKind)>> = BTreeMap::new();
        touches.insert(cs("c1"), vec![(pk("foo"), PlanTouchKind::Intro)]);
        touches.insert(cs("c2"), vec![(pk("bar"), PlanTouchKind::Intro)]);
        let ts = last_activity_ts_for(
            &pk("foo"),
            &cs("c1"),
            &BTreeMap::new(),
            &BTreeMap::new(),
            &[],
            &[cs("c1"), cs("c2")],
            &touches,
            &BTreeMap::new(),
            &commit_meta,
        );
        assert_eq!(
            ts, 1_000,
            "bar's later commit should not bump foo's activity"
        );
    }
}
