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
    AttributionResult, CommitKind, Feedback, Phase, Plan, PlanTouchKind, PlanWorktreeStatus,
    Verdict, WaitingOn, WaitingReason, WaitingRole,
};
use crate::review_state::{CommitGate, CommitGateState, ReviewGateDecision, ReviewGateState};

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

/// Compose the `waiting_on` value for a session. Top rows
/// (worktree-status-driven) preempt the gate-driven row. The caller
/// passes the latest reviewable commit's gate (via
/// `latest_reviewable_commit_gate_for` or one of the legacy `plan_gate_for` /
/// `impl_gate_for` wrappers that funnel into the same store).
pub fn waiting_on(
    is_done: bool,
    worktree_status: PlanWorktreeStatus,
    gate: Option<&ReviewGateDecision>,
) -> WaitingOn {
    if is_done {
        return make(WaitingRole::None, WaitingReason::SessionDone, Vec::new());
    }
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
    waiting_from_gate(gate)
}

/// Latest reviewable commit's gate for one plan, projected into the
/// legacy `ReviewGateDecision` shape. Skips `MultiPlan`, `DoneMove`,
/// `Unattributed` — those don't drive `waiting_on`. Returns `None`
/// when no reviewable commit exists yet.
pub fn latest_reviewable_commit_gate_for(
    plan_key: &crate::lifecycle::PlanKey,
    commits: &BTreeMap<CommitSha, CommitGate>,
    commit_order: &[CommitSha],
    plan_touches: &BTreeMap<CommitSha, Vec<(crate::lifecycle::PlanKey, PlanTouchKind)>>,
    attribution: &BTreeMap<CommitSha, AttributionResult>,
) -> Option<ReviewGateDecision> {
    let target = latest_reviewable_commit_for(plan_key, commit_order, plan_touches, attribution)?;
    let gate = commits.get(&target)?;
    Some(commit_gate_to_review_decision(gate))
}

/// Latest commit whose `CommitKind` is reviewable (`PlanOnly`,
/// `CodeOnly`, or `Mixed`) for this plan, walking `commit_order`
/// newest-first.
pub fn latest_reviewable_commit_for(
    plan_key: &crate::lifecycle::PlanKey,
    commit_order: &[CommitSha],
    plan_touches: &BTreeMap<CommitSha, Vec<(crate::lifecycle::PlanKey, PlanTouchKind)>>,
    attribution: &BTreeMap<CommitSha, AttributionResult>,
) -> Option<CommitSha> {
    for sha in commit_order.iter().rev() {
        let kind = commit_kind_for(plan_key, sha, plan_touches, attribution);
        if kind.is_reviewable() {
            return Some(sha.clone());
        }
    }
    None
}

fn waiting_from_gate(gate: Option<&ReviewGateDecision>) -> WaitingOn {
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
        ReviewGateState::ChangesRequested => make(
            WaitingRole::Master,
            WaitingReason::AddressCommitChanges,
            gate.request_changes.clone(),
        ),
        ReviewGateState::Ready => make(
            WaitingRole::Master,
            WaitingReason::ReadyToMoveForward,
            Vec::new(),
        ),
        ReviewGateState::NeedsReview => {
            let agents = if gate.participants.is_empty() {
                Vec::new()
            } else {
                gate.missing_approvals.clone()
            };
            make(
                WaitingRole::Reviewers,
                WaitingReason::CommitNeedsReview,
                agents,
            )
        }
    }
}

/// Most recent activity timestamp for one plan, computed as the
/// maximum of:
///
/// - author_ts of the newest commit attributed to (or touching) this
///   plan,
/// - mtime of the newest feedback file (plan / impl / held),
/// - author_ts of the plan_intro commit.
///
/// Used by `/api/plans` to sort the homepage by recency. Pure;
/// sans-IO. Returns `0` when `commit_meta` lacks an entry for
/// `plan_intro` AND there's no feedback. In practice `git_io::snapshot`
/// backfills off-first-parent intros so this fallback is rare for
/// committed plans, but the function is defensive so callers don't
/// have to special-case malformed snapshots.
pub fn last_activity_ts_for(
    plan_key: &crate::lifecycle::PlanKey,
    plan_intro: &CommitSha,
    commits: &BTreeMap<CommitSha, CommitGate>,
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
    for gate in commits.values() {
        for fb in gate.feedback.values() {
            max_ts = max_ts.max(fb.created_at);
        }
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

/// Latest commit whose `CommitKind` is `PlanOnly` or `Mixed` for this
/// plan, walking `commit_order` newest-first. Skips `MultiPlan`,
/// `DoneMove`, and `Unattributed` so the gate routes to the latest
/// commit that actually carries a reviewable plan touch. Returns
/// `None` if no such commit exists.
fn latest_reviewable_plan_commit_for(
    plan_key: &crate::lifecycle::PlanKey,
    commit_order: &[CommitSha],
    plan_touches: &BTreeMap<
        CommitSha,
        Vec<(crate::lifecycle::PlanKey, crate::repo_state::PlanTouchKind)>,
    >,
    attribution: &BTreeMap<CommitSha, AttributionResult>,
) -> Option<CommitSha> {
    for sha in commit_order.iter().rev() {
        let kind = commit_kind_for(plan_key, sha, plan_touches, attribution);
        if matches!(kind, CommitKind::PlanOnly | CommitKind::Mixed) {
            return Some(sha.clone());
        }
    }
    None
}

/// Latest commit whose `CommitKind` is `CodeOnly` or `Mixed` for this
/// plan, walking `commit_order` newest-first. Used by the impl gate
/// bridge so a `MultiPlan` code commit doesn't route to a
/// non-existent gate entry.
fn latest_reviewable_impl_commit_for(
    plan_key: &crate::lifecycle::PlanKey,
    commit_order: &[CommitSha],
    plan_touches: &BTreeMap<
        CommitSha,
        Vec<(crate::lifecycle::PlanKey, crate::repo_state::PlanTouchKind)>,
    >,
    attribution: &BTreeMap<CommitSha, AttributionResult>,
) -> Option<CommitSha> {
    for sha in commit_order.iter().rev() {
        let kind = commit_kind_for(plan_key, sha, plan_touches, attribution);
        if matches!(kind, CommitKind::CodeOnly | CommitKind::Mixed) {
            return Some(sha.clone());
        }
    }
    None
}

/// Plan-phase review gate. Projects the `CommitGate` for the latest
/// reviewable plan-touching commit (`PlanOnly` or `Mixed`) into a
/// `ReviewGateDecision`. Skips `MultiPlan` / `DoneMove` touches so
/// the gate routes to a real reviewable target. Returns `None` when
/// no reviewable plan commit exists yet.
pub fn plan_gate_for(
    plan: &Plan,
    state: &crate::repo_state::RepoState,
) -> Option<ReviewGateDecision> {
    plan_gate_for_parts(
        &plan.id,
        &plan.commits,
        &state.commit_order,
        &state.plan_touches,
        &state.attribution,
    )
}

/// Same as `plan_gate_for` but over primitive inputs.
pub fn plan_gate_for_parts(
    plan_key: &crate::lifecycle::PlanKey,
    commits: &BTreeMap<CommitSha, CommitGate>,
    commit_order: &[CommitSha],
    plan_touches: &BTreeMap<
        CommitSha,
        Vec<(crate::lifecycle::PlanKey, crate::repo_state::PlanTouchKind)>,
    >,
    attribution: &BTreeMap<CommitSha, AttributionResult>,
) -> Option<ReviewGateDecision> {
    let current_target =
        latest_reviewable_plan_commit_for(plan_key, commit_order, plan_touches, attribution)?;
    let gate = commits.get(&current_target)?;
    Some(commit_gate_to_review_decision(gate))
}

/// Implementation-phase review gate. Projects the `CommitGate` for
/// the latest reviewable code-bearing commit (`CodeOnly` or `Mixed`)
/// attributed to this plan. Skips `MultiPlan` so a multi-plan code
/// commit doesn't route to a missing gate entry.
pub fn impl_gate_for(
    plan: &Plan,
    state: &crate::repo_state::RepoState,
) -> Option<ReviewGateDecision> {
    impl_gate_for_parts(
        &plan.id,
        &plan.commits,
        &state.commit_order,
        &state.plan_touches,
        &state.attribution,
    )
}

/// Same as `impl_gate_for` but over primitive inputs.
pub fn impl_gate_for_parts(
    plan_key: &crate::lifecycle::PlanKey,
    commits: &BTreeMap<CommitSha, CommitGate>,
    commit_order: &[CommitSha],
    plan_touches: &BTreeMap<
        CommitSha,
        Vec<(crate::lifecycle::PlanKey, crate::repo_state::PlanTouchKind)>,
    >,
    attribution: &BTreeMap<CommitSha, AttributionResult>,
) -> Option<ReviewGateDecision> {
    let current_target =
        latest_reviewable_impl_commit_for(plan_key, commit_order, plan_touches, attribution)?;
    let gate = commits.get(&current_target)?;
    Some(commit_gate_to_review_decision(gate))
}

/// Project a `CommitGate` into the legacy `ReviewGateDecision` shape
/// for callers that still consume the latter. Bridge between the
/// commit-keyed storage (`Plan.commits`) and the per-phase gate
/// representation that `waiting_on` and the existing MCP response
/// shape expect. Phase 2.3 will collapse callers onto `CommitGate`
/// directly and delete this bridge.
fn commit_gate_to_review_decision(gate: &CommitGate) -> ReviewGateDecision {
    ReviewGateDecision {
        state: match gate.state {
            CommitGateState::ChangesRequested => ReviewGateState::ChangesRequested,
            CommitGateState::Approved => ReviewGateState::Ready,
            CommitGateState::Unreviewed => ReviewGateState::NeedsReview,
        },
        approval_rule: "all_participants",
        participants: gate.participants.clone(),
        approvals: gate.approvers.clone(),
        request_changes: gate.requesters.clone(),
        unmarked: gate.ambiguous.clone(),
        missing_approvals: gate.missing.clone(),
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
        AddressCommitChanges => "address_commit_changes",
        ReadyToMoveForward => "move_forward",
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
        (Master, ReadyToMoveForward) => {
            "Latest commit approved. Continue with the next commit or move the plan to `done/`."
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

/// Classify a single commit's relevance to one plan under the
/// commit-centric review model. Pure; sans-IO. See plan
/// §"`commit_kind` classification" for the decision table.
///
/// The single-plan invariant: a commit with `plan_touches` covering
/// two or more distinct plans is `MultiPlan` for every plan in the
/// touched set, and `Unattributed` for plans not in the set.
/// `DoneMove` takes precedence over everything else for the plan
/// being moved — even if the rename commit carries code changes, the
/// lifecycle event wins.
pub fn commit_kind_for(
    plan_key: &crate::lifecycle::PlanKey,
    sha: &CommitSha,
    plan_touches: &BTreeMap<CommitSha, Vec<(crate::lifecycle::PlanKey, PlanTouchKind)>>,
    attribution: &BTreeMap<CommitSha, AttributionResult>,
) -> CommitKind {
    let touches = plan_touches.get(sha);
    // Hot path: nearly every commit has 0 or 1 plan_touches, so
    // short-circuit before building any set.
    let distinct_plans_touched = match touches {
        None => 0,
        Some(ts) if ts.len() <= 1 => ts.len(),
        Some(ts) => ts
            .iter()
            .map(|(k, _)| k)
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
    };
    let our_touch = touches.and_then(|ts| {
        ts.iter()
            .find(|(k, _)| k == plan_key)
            .map(|(_, kind)| *kind)
    });

    if let Some(PlanTouchKind::DoneMove) = our_touch {
        return CommitKind::DoneMove;
    }

    if our_touch.is_some() {
        if distinct_plans_touched >= 2 {
            return CommitKind::MultiPlan;
        }
        let has_code = matches!(
            attribution.get(sha),
            Some(AttributionResult::Attributed {
                session,
                has_code_changes: true,
                ..
            }) if session == plan_key
        );
        return if has_code {
            CommitKind::Mixed
        } else {
            CommitKind::PlanOnly
        };
    }

    match attribution.get(sha) {
        Some(AttributionResult::Attributed {
            session,
            has_code_changes: true,
            ..
        }) if session == plan_key => CommitKind::CodeOnly,
        _ => CommitKind::Unattributed,
    }
}

/// Build the per-commit `CommitGate` map for one plan under the
/// cumulative-participant rule.
///
/// Walk `commit_order` chronologically. Maintain a cumulative set of
/// plan-wide participants — anyone who has left feedback on any
/// earlier reviewable commit for this plan. For each reviewable
/// commit (`PlanOnly` | `CodeOnly` | `Mixed`):
///
/// 1. Union the feedback authors on this SHA (from
///    `plan_feedback ∪ impl_feedback`) into the participant set.
/// 2. Split this-SHA responders by `Verdict`: APPROVE → approvers,
///    REQUEST_CHANGES → requesters, Unmarked → ambiguous.
/// 3. `missing` = participants \ (approvers ∪ requesters ∪ ambiguous).
/// 4. State: `ChangesRequested` if any requester or ambiguous;
///    else `Approved` if at least one approver and zero missing;
///    else `Unreviewed`.
///
/// `DoneMove` / `MultiPlan` / `Unattributed` commits are skipped —
/// they never get a gate entry AND never contribute to the
/// cumulative participant set (a reviewer leaving feedback on an
/// unreviewable commit doesn't get auto-enrolled as a participant
/// for the rest of the plan's history).
pub fn build_commit_gates(
    plan_key: &crate::lifecycle::PlanKey,
    commit_order: &[CommitSha],
    plan_touches: &BTreeMap<CommitSha, Vec<(crate::lifecycle::PlanKey, PlanTouchKind)>>,
    attribution: &BTreeMap<CommitSha, AttributionResult>,
    feedback: &BTreeMap<(CommitSha, AgentLabel), Feedback>,
) -> BTreeMap<CommitSha, CommitGate> {
    let mut participants: Vec<AgentLabel> = Vec::new();
    let mut out = BTreeMap::new();

    for sha in commit_order {
        let kind = commit_kind_for(plan_key, sha, plan_touches, attribution);
        if !kind.is_reviewable() {
            continue;
        }

        let mut approvers: Vec<AgentLabel> = Vec::new();
        let mut requesters: Vec<AgentLabel> = Vec::new();
        let mut ambiguous: Vec<AgentLabel> = Vec::new();
        let mut commit_feedback: BTreeMap<AgentLabel, Feedback> = BTreeMap::new();
        for ((target, author), fb) in feedback {
            if target == sha {
                commit_feedback.insert(author.clone(), fb.clone());
            }
        }

        for (author, fb) in &commit_feedback {
            if !participants.contains(author) {
                participants.push(author.clone());
            }
            match fb.verdict {
                Verdict::Approve => {
                    if !approvers.contains(author) {
                        approvers.push(author.clone());
                    }
                }
                Verdict::RequestChanges => {
                    if !requesters.contains(author) {
                        requesters.push(author.clone());
                    }
                }
                Verdict::Unmarked => {
                    if !ambiguous.contains(author) {
                        ambiguous.push(author.clone());
                    }
                }
            }
        }

        let missing: Vec<AgentLabel> = participants
            .iter()
            .filter(|p| !approvers.contains(p) && !requesters.contains(p) && !ambiguous.contains(p))
            .cloned()
            .collect();

        let state = if !requesters.is_empty() || !ambiguous.is_empty() {
            CommitGateState::ChangesRequested
        } else if !approvers.is_empty() && missing.is_empty() {
            CommitGateState::Approved
        } else {
            CommitGateState::Unreviewed
        };

        out.insert(
            sha.clone(),
            CommitGate {
                state,
                participants: participants.clone(),
                approvers,
                requesters,
                ambiguous,
                missing,
                feedback: commit_feedback,
            },
        );
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::lifecycle::{AgentLabel, PlanKey};
    use crate::repo_state::PlanTouchKind;
    use crate::review_state::{ReviewGateDecision, ReviewGateState};
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
        state: ReviewGateState,
        participants: Vec<AgentLabel>,
        approvals: Vec<AgentLabel>,
        request_changes: Vec<AgentLabel>,
        missing_approvals: Vec<AgentLabel>,
    ) -> ReviewGateDecision {
        ReviewGateDecision {
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
        let w = waiting_on(true, PlanWorktreeStatus::Clean, None);
        assert_eq!(w.role, WaitingRole::None);
        assert_eq!(w.reason, WaitingReason::SessionDone);
    }

    #[test]
    fn waiting_commit_done_move_preempts_gate() {
        let g = gate(
            ReviewGateState::Ready,
            agents(&["alice"]),
            agents(&["alice"]),
            Vec::new(),
            Vec::new(),
        );
        let w = waiting_on(false, PlanWorktreeStatus::DoneMovePending, Some(&g));
        assert_eq!(w.role, WaitingRole::Master);
        assert_eq!(w.reason, WaitingReason::CommitDoneMove);
    }

    #[test]
    fn waiting_restore_or_commit_done_move() {
        let w = waiting_on(false, PlanWorktreeStatus::MissingActivePlanFile, None);
        assert_eq!(w.role, WaitingRole::Master);
        assert_eq!(w.reason, WaitingReason::RestoreOrCommitDoneMove);
    }

    #[test]
    fn waiting_commit_plan_revision_preempts_gate() {
        let g = gate(
            ReviewGateState::Ready,
            agents(&["alice"]),
            agents(&["alice"]),
            Vec::new(),
            Vec::new(),
        );
        let w = waiting_on(false, PlanWorktreeStatus::BodyDirty, Some(&g));
        assert_eq!(w.role, WaitingRole::Master);
        assert_eq!(w.reason, WaitingReason::CommitPlanRevision);
    }

    #[test]
    fn waiting_plan_initial_review_no_participants() {
        let g = gate(
            ReviewGateState::NeedsReview,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );
        let w = waiting_on(false, PlanWorktreeStatus::Clean, Some(&g));
        assert_eq!(w.role, WaitingRole::Reviewers);
        assert_eq!(w.reason, WaitingReason::CommitNeedsReview);
        assert!(w.agents.is_empty());
    }

    #[test]
    fn waiting_plan_rereview_with_stale_participants() {
        let g = gate(
            ReviewGateState::NeedsReview,
            agents(&["alice", "bob"]),
            Vec::new(),
            Vec::new(),
            agents(&["alice", "bob"]),
        );
        let w = waiting_on(false, PlanWorktreeStatus::Clean, Some(&g));
        assert_eq!(w.role, WaitingRole::Reviewers);
        assert_eq!(w.reason, WaitingReason::CommitNeedsReview);
        assert_eq!(w.agents.len(), 2);
    }

    #[test]
    fn waiting_plan_request_changes_to_master() {
        let g = gate(
            ReviewGateState::ChangesRequested,
            agents(&["alice", "bob"]),
            Vec::new(),
            agents(&["bob"]),
            Vec::new(),
        );
        let w = waiting_on(false, PlanWorktreeStatus::Clean, Some(&g));
        assert_eq!(w.role, WaitingRole::Master);
        assert_eq!(w.reason, WaitingReason::AddressCommitChanges);
        assert_eq!(w.agents, agents(&["bob"]));
    }

    #[test]
    fn waiting_plan_ready_to_implement() {
        let g = gate(
            ReviewGateState::Ready,
            agents(&["alice"]),
            agents(&["alice"]),
            Vec::new(),
            Vec::new(),
        );
        let w = waiting_on(false, PlanWorktreeStatus::Clean, Some(&g));
        assert_eq!(w.role, WaitingRole::Master);
        assert_eq!(w.reason, WaitingReason::ReadyToMoveForward);
    }

    #[test]
    fn waiting_impl_initial_review() {
        let g = gate(
            ReviewGateState::NeedsReview,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );
        let w = waiting_on(false, PlanWorktreeStatus::Clean, Some(&g));
        assert_eq!(w.role, WaitingRole::Reviewers);
        assert_eq!(w.reason, WaitingReason::CommitNeedsReview);
    }

    #[test]
    fn waiting_impl_request_changes() {
        let g = gate(
            ReviewGateState::ChangesRequested,
            agents(&["alice"]),
            Vec::new(),
            agents(&["alice"]),
            Vec::new(),
        );
        let w = waiting_on(false, PlanWorktreeStatus::Clean, Some(&g));
        assert_eq!(w.role, WaitingRole::Master);
        assert_eq!(w.reason, WaitingReason::AddressCommitChanges);
        assert_eq!(w.agents, agents(&["alice"]));
    }

    #[test]
    fn waiting_impl_ready_to_finish() {
        let g = gate(
            ReviewGateState::Ready,
            agents(&["alice"]),
            agents(&["alice"]),
            Vec::new(),
            Vec::new(),
        );
        let w = waiting_on(false, PlanWorktreeStatus::Clean, Some(&g));
        assert_eq!(w.role, WaitingRole::Master);
        assert_eq!(w.reason, WaitingReason::ReadyToMoveForward);
    }

    #[test]
    fn description_includes_agent_list_when_present() {
        let g = gate(
            ReviewGateState::ChangesRequested,
            agents(&["alice", "bob"]),
            Vec::new(),
            agents(&["bob"]),
            Vec::new(),
        );
        let w = waiting_on(false, PlanWorktreeStatus::Clean, Some(&g));
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

    fn gate_with_feedback(items: &[(&str, i64)]) -> CommitGate {
        let mut feedback = BTreeMap::new();
        for (author, created_at) in items {
            feedback.insert(
                AgentLabel::from((*author).to_string()),
                Feedback {
                    path: std::path::PathBuf::from("/fake"),
                    body: String::new(),
                    verdict: crate::repo_state::Verdict::Unmarked,
                    created_at: *created_at,
                },
            );
        }
        CommitGate {
            state: CommitGateState::Unreviewed,
            participants: Vec::new(),
            approvers: Vec::new(),
            requesters: Vec::new(),
            ambiguous: Vec::new(),
            missing: Vec::new(),
            feedback,
        }
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
        let mut commits = BTreeMap::new();
        commits.insert(cs("c1"), gate_with_feedback(&[("codex", 3_000)]));
        let ts = last_activity_ts_for(
            &pk("foo"),
            &cs("c1"),
            &commits,
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

    // -------- commit_kind_for --------

    fn touches_one(plan: &str, kind: PlanTouchKind) -> Vec<(PlanKey, PlanTouchKind)> {
        vec![(pk(plan), kind)]
    }

    fn attributed(plan: &str, kind: Option<PlanTouchKind>, code: bool) -> AttributionResult {
        AttributionResult::Attributed {
            session: pk(plan),
            plan_touch: kind,
            has_code_changes: code,
        }
    }

    #[test]
    fn commit_kind_plan_only() {
        let mut touches = BTreeMap::new();
        touches.insert(cs("a"), touches_one("foo", PlanTouchKind::Revision));
        let mut attribution = BTreeMap::new();
        attribution.insert(
            cs("a"),
            attributed("foo", Some(PlanTouchKind::Revision), false),
        );
        assert_eq!(
            commit_kind_for(&pk("foo"), &cs("a"), &touches, &attribution),
            CommitKind::PlanOnly
        );
    }

    #[test]
    fn commit_kind_mixed_when_plan_touch_plus_code() {
        let mut touches = BTreeMap::new();
        touches.insert(cs("a"), touches_one("foo", PlanTouchKind::Revision));
        let mut attribution = BTreeMap::new();
        attribution.insert(
            cs("a"),
            attributed("foo", Some(PlanTouchKind::Revision), true),
        );
        assert_eq!(
            commit_kind_for(&pk("foo"), &cs("a"), &touches, &attribution),
            CommitKind::Mixed
        );
    }

    #[test]
    fn commit_kind_code_only_when_attribution_walks_back() {
        // No plan_touch on this commit, attribution inherited from
        // an ancestor that touched foo's plan file.
        let touches = BTreeMap::new();
        let mut attribution = BTreeMap::new();
        attribution.insert(cs("a"), attributed("foo", None, true));
        assert_eq!(
            commit_kind_for(&pk("foo"), &cs("a"), &touches, &attribution),
            CommitKind::CodeOnly
        );
    }

    #[test]
    fn commit_kind_done_move_takes_precedence_over_code() {
        // Even if the rename commit somehow carries code changes,
        // DoneMove wins for the plan being moved.
        let mut touches = BTreeMap::new();
        touches.insert(cs("a"), touches_one("foo", PlanTouchKind::DoneMove));
        let mut attribution = BTreeMap::new();
        attribution.insert(
            cs("a"),
            attributed("foo", Some(PlanTouchKind::DoneMove), true),
        );
        assert_eq!(
            commit_kind_for(&pk("foo"), &cs("a"), &touches, &attribution),
            CommitKind::DoneMove
        );
    }

    #[test]
    fn commit_kind_multi_plan_for_touched_plan() {
        // A single commit touching foo.md AND bar.md → MultiPlan for
        // both, regardless of attribution.
        let mut touches = BTreeMap::new();
        touches.insert(
            cs("a"),
            vec![
                (pk("foo"), PlanTouchKind::Revision),
                (pk("bar"), PlanTouchKind::Revision),
            ],
        );
        let mut attribution = BTreeMap::new();
        attribution.insert(cs("a"), AttributionResult::Unattributed);
        assert_eq!(
            commit_kind_for(&pk("foo"), &cs("a"), &touches, &attribution),
            CommitKind::MultiPlan
        );
        assert_eq!(
            commit_kind_for(&pk("bar"), &cs("a"), &touches, &attribution),
            CommitKind::MultiPlan
        );
    }

    #[test]
    fn commit_kind_unattributed_for_unrelated_plan() {
        // baz isn't touched and the commit is attributed to foo.
        let mut touches = BTreeMap::new();
        touches.insert(cs("a"), touches_one("foo", PlanTouchKind::Revision));
        let mut attribution = BTreeMap::new();
        attribution.insert(
            cs("a"),
            attributed("foo", Some(PlanTouchKind::Revision), false),
        );
        assert_eq!(
            commit_kind_for(&pk("baz"), &cs("a"), &touches, &attribution),
            CommitKind::Unattributed
        );
    }

    #[test]
    fn commit_kind_unattributed_when_no_touch_and_no_attribution() {
        let touches = BTreeMap::new();
        let attribution = BTreeMap::new();
        assert_eq!(
            commit_kind_for(&pk("foo"), &cs("a"), &touches, &attribution),
            CommitKind::Unattributed
        );
    }

    #[test]
    fn commit_kind_trinity_feedback_only_is_unattributed() {
        // A commit that only writes review files under `.trinity/feedback/`
        // (or `.trinity/cache/`) must never appear as work-to-be-reviewed.
        // `git_io`'s diff walk excludes `.trinity/` from
        // `has_non_plan_code_changes`, so attribution carries
        // `has_code_changes: false`. Locking this in protects against a
        // future change to that filter silently flipping the kind to
        // `CodeOnly`.
        let touches = BTreeMap::new();
        let mut attribution = BTreeMap::new();
        attribution.insert(cs("a"), attributed("foo", None, false));
        assert_eq!(
            commit_kind_for(&pk("foo"), &cs("a"), &touches, &attribution),
            CommitKind::Unattributed
        );
    }

    // -------- build_commit_gates --------

    fn feedback_with(verdict: Verdict) -> Feedback {
        Feedback {
            path: PathBuf::from("/tmp/fb"),
            body: String::new(),
            verdict,
            created_at: 0,
        }
    }

    fn al(s: &str) -> AgentLabel {
        AgentLabel::from(s.to_string())
    }

    #[test]
    fn gates_cumulative_participants_across_commits() {
        // commit A (plan_only) ← codex APPROVE
        // commit B (code_only) ← alice REQUEST_CHANGES
        // commit C (code_only) ← alice APPROVE   (codex hasn't voted on C)
        //   → gate(C): participants={codex, alice}, missing={codex},
        //     state=Unreviewed
        let mut touches = BTreeMap::new();
        touches.insert(cs("a"), touches_one("foo", PlanTouchKind::Intro));
        let mut attribution = BTreeMap::new();
        attribution.insert(
            cs("a"),
            attributed("foo", Some(PlanTouchKind::Intro), false),
        );
        attribution.insert(cs("b"), attributed("foo", None, true));
        attribution.insert(cs("c"), attributed("foo", None, true));
        let mut feedback = BTreeMap::new();
        feedback.insert((cs("a"), al("codex")), feedback_with(Verdict::Approve));
        feedback.insert(
            (cs("b"), al("alice")),
            feedback_with(Verdict::RequestChanges),
        );
        feedback.insert((cs("c"), al("alice")), feedback_with(Verdict::Approve));

        let gates = build_commit_gates(
            &pk("foo"),
            &[cs("a"), cs("b"), cs("c")],
            &touches,
            &attribution,
            &feedback,
        );

        // gate(A): codex approved, only participant → Approved.
        let g_a = gates.get(&cs("a")).expect("gate for a");
        assert_eq!(g_a.state, CommitGateState::Approved);
        assert_eq!(g_a.participants, vec![al("codex")]);
        assert_eq!(g_a.approvers, vec![al("codex")]);

        // gate(B): alice requested changes; codex inherits as participant.
        let g_b = gates.get(&cs("b")).expect("gate for b");
        assert_eq!(g_b.state, CommitGateState::ChangesRequested);
        assert!(g_b.participants.contains(&al("codex")));
        assert!(g_b.participants.contains(&al("alice")));
        assert_eq!(g_b.requesters, vec![al("alice")]);
        assert_eq!(g_b.missing, vec![al("codex")]);

        // gate(C): alice approved; codex missing → Unreviewed.
        let g_c = gates.get(&cs("c")).expect("gate for c");
        assert_eq!(g_c.state, CommitGateState::Unreviewed);
        assert_eq!(g_c.approvers, vec![al("alice")]);
        assert_eq!(g_c.missing, vec![al("codex")]);
    }

    #[test]
    fn gates_ambiguous_verdict_blocks_approval() {
        // codex drops an Unmarked file on commit A → gate is
        // ChangesRequested, not Approved.
        let mut touches = BTreeMap::new();
        touches.insert(cs("a"), touches_one("foo", PlanTouchKind::Intro));
        let mut attribution = BTreeMap::new();
        attribution.insert(
            cs("a"),
            attributed("foo", Some(PlanTouchKind::Intro), false),
        );
        let mut plan_feedback = BTreeMap::new();
        plan_feedback.insert((cs("a"), al("codex")), feedback_with(Verdict::Unmarked));

        let gates = build_commit_gates(
            &pk("foo"),
            &[cs("a")],
            &touches,
            &attribution,
            &plan_feedback,
        );

        let g = gates.get(&cs("a")).expect("gate for a");
        assert_eq!(g.state, CommitGateState::ChangesRequested);
        assert_eq!(g.ambiguous, vec![al("codex")]);
        assert!(g.approvers.is_empty());
        assert!(g.missing.is_empty(), "ambiguous counts as having voted");
    }

    #[test]
    fn gates_skip_done_move_and_multi_plan() {
        // Reviewable: a (PlanOnly), c (CodeOnly).
        // Unreviewable: b (MultiPlan), d (DoneMove).
        let mut touches = BTreeMap::new();
        touches.insert(cs("a"), touches_one("foo", PlanTouchKind::Intro));
        touches.insert(
            cs("b"),
            vec![
                (pk("foo"), PlanTouchKind::Revision),
                (pk("bar"), PlanTouchKind::Revision),
            ],
        );
        touches.insert(cs("d"), touches_one("foo", PlanTouchKind::DoneMove));
        let mut attribution = BTreeMap::new();
        attribution.insert(
            cs("a"),
            attributed("foo", Some(PlanTouchKind::Intro), false),
        );
        attribution.insert(cs("b"), AttributionResult::Unattributed);
        attribution.insert(cs("c"), attributed("foo", None, true));
        attribution.insert(
            cs("d"),
            attributed("foo", Some(PlanTouchKind::DoneMove), false),
        );

        // Reviewer leaves feedback on the unreviewable commits — those
        // votes must NOT enroll them as cumulative participants.
        let mut plan_feedback = BTreeMap::new();
        plan_feedback.insert((cs("b"), al("rogue")), feedback_with(Verdict::Approve));
        plan_feedback.insert((cs("d"), al("rogue")), feedback_with(Verdict::Approve));

        let gates = build_commit_gates(
            &pk("foo"),
            &[cs("a"), cs("b"), cs("c"), cs("d")],
            &touches,
            &attribution,
            &plan_feedback,
        );

        assert!(gates.contains_key(&cs("a")));
        assert!(!gates.contains_key(&cs("b")), "multi_plan has no gate");
        assert!(gates.contains_key(&cs("c")));
        assert!(!gates.contains_key(&cs("d")), "done_move has no gate");

        // gate(C) sees zero participants — `rogue` reviewed only
        // unreviewable commits and so never enters the participant set.
        let g_c = &gates[&cs("c")];
        assert!(
            g_c.participants.is_empty(),
            "rogue on unreviewable commits doesn't enroll: {:?}",
            g_c.participants
        );
        assert_eq!(g_c.state, CommitGateState::Unreviewed);
    }

    #[test]
    fn gates_request_changes_overrides_approve() {
        // Two reviewers on the same SHA: one APPROVE, one
        // REQUEST_CHANGES → ChangesRequested.
        let mut touches = BTreeMap::new();
        touches.insert(cs("a"), touches_one("foo", PlanTouchKind::Intro));
        let mut attribution = BTreeMap::new();
        attribution.insert(
            cs("a"),
            attributed("foo", Some(PlanTouchKind::Intro), false),
        );
        let mut plan_feedback = BTreeMap::new();
        plan_feedback.insert((cs("a"), al("alice")), feedback_with(Verdict::Approve));
        plan_feedback.insert((cs("a"), al("bob")), feedback_with(Verdict::RequestChanges));

        let gates = build_commit_gates(
            &pk("foo"),
            &[cs("a")],
            &touches,
            &attribution,
            &plan_feedback,
        );

        let g = &gates[&cs("a")];
        assert_eq!(g.state, CommitGateState::ChangesRequested);
        assert_eq!(g.approvers, vec![al("alice")]);
        assert_eq!(g.requesters, vec![al("bob")]);
    }

    #[test]
    fn gates_participants_carry_across_no_feedback_commit() {
        // A (plan_only) ← codex APPROVE → B (code_only, zero feedback)
        // gate(B): codex still expected; state = Unreviewed; missing = [codex].
        // Locks in: participants accumulate across reviewable commits
        // even when the next commit has no votes yet.
        let mut touches = BTreeMap::new();
        touches.insert(cs("a"), touches_one("foo", PlanTouchKind::Intro));
        let mut attribution = BTreeMap::new();
        attribution.insert(
            cs("a"),
            attributed("foo", Some(PlanTouchKind::Intro), false),
        );
        attribution.insert(cs("b"), attributed("foo", None, true));
        let mut plan_feedback = BTreeMap::new();
        plan_feedback.insert((cs("a"), al("codex")), feedback_with(Verdict::Approve));

        let gates = build_commit_gates(
            &pk("foo"),
            &[cs("a"), cs("b")],
            &touches,
            &attribution,
            &plan_feedback,
        );

        let g_b = gates.get(&cs("b")).expect("gate for b");
        assert_eq!(g_b.state, CommitGateState::Unreviewed);
        assert_eq!(g_b.participants, vec![al("codex")]);
        assert_eq!(g_b.missing, vec![al("codex")]);
        assert!(g_b.approvers.is_empty());
    }

    #[test]
    fn gates_participants_carry_across_unattributed_gap() {
        // A (plan_only) ← codex APPROVE → B (Unattributed, skipped) →
        // C (code_only, zero feedback).
        // gate(C): codex still in participants; the Unattributed gap
        // must not drop accumulated state. Future-proofs against
        // someone "resetting" participants on each skip.
        let mut touches = BTreeMap::new();
        touches.insert(cs("a"), touches_one("foo", PlanTouchKind::Intro));
        let mut attribution = BTreeMap::new();
        attribution.insert(
            cs("a"),
            attributed("foo", Some(PlanTouchKind::Intro), false),
        );
        attribution.insert(cs("b"), AttributionResult::Unattributed);
        attribution.insert(cs("c"), attributed("foo", None, true));
        let mut plan_feedback = BTreeMap::new();
        plan_feedback.insert((cs("a"), al("codex")), feedback_with(Verdict::Approve));

        let gates = build_commit_gates(
            &pk("foo"),
            &[cs("a"), cs("b"), cs("c")],
            &touches,
            &attribution,
            &plan_feedback,
        );

        assert!(!gates.contains_key(&cs("b")), "Unattributed has no gate");
        let g_c = gates.get(&cs("c")).expect("gate for c");
        assert_eq!(g_c.participants, vec![al("codex")]);
        assert_eq!(g_c.missing, vec![al("codex")]);
        assert_eq!(g_c.state, CommitGateState::Unreviewed);
    }

    #[test]
    fn gates_feedback_map_carries_verdicts_and_bodies() {
        // CommitGate.feedback is the canonical home for verdict-bearing
        // files on a commit. MCP / UI consumers read body + path +
        // verdict from here; the gate's vec fields are display-list
        // shorthand.
        let mut touches = BTreeMap::new();
        touches.insert(cs("a"), touches_one("foo", PlanTouchKind::Intro));
        let mut attribution = BTreeMap::new();
        attribution.insert(
            cs("a"),
            attributed("foo", Some(PlanTouchKind::Intro), false),
        );
        let mut plan_feedback = BTreeMap::new();
        plan_feedback.insert(
            (cs("a"), al("codex")),
            Feedback {
                path: PathBuf::from("/abs/codex.md"),
                body: "APPROVE\n\nlooks good".to_string(),
                verdict: Verdict::Approve,
                created_at: 100,
            },
        );

        let gates = build_commit_gates(
            &pk("foo"),
            &[cs("a")],
            &touches,
            &attribution,
            &plan_feedback,
        );

        let g = gates.get(&cs("a")).expect("gate for a");
        let fb = g.feedback.get(&al("codex")).expect("codex feedback");
        assert_eq!(fb.verdict, Verdict::Approve);
        assert_eq!(fb.body, "APPROVE\n\nlooks good");
        assert_eq!(fb.path, PathBuf::from("/abs/codex.md"));
        assert_eq!(fb.created_at, 100);
    }

    #[test]
    fn gates_feedback_map_carries_impl_feedback_too() {
        // Symmetric to the previous test but on the impl_feedback
        // path — both legacy maps must round-trip body/path/etc into
        // CommitGate.feedback.
        let touches = BTreeMap::new();
        let mut attribution = BTreeMap::new();
        attribution.insert(cs("a"), attributed("foo", None, true));
        let mut impl_feedback = BTreeMap::new();
        impl_feedback.insert(
            (cs("a"), al("alice")),
            Feedback {
                path: PathBuf::from("/abs/alice.md"),
                body: "REQUEST_CHANGES\n\nfix it".to_string(),
                verdict: Verdict::RequestChanges,
                created_at: 250,
            },
        );

        let gates = build_commit_gates(
            &pk("foo"),
            &[cs("a")],
            &touches,
            &attribution,
            &impl_feedback,
        );

        let g = gates.get(&cs("a")).expect("gate for a");
        let fb = g.feedback.get(&al("alice")).expect("alice feedback");
        assert_eq!(fb.verdict, Verdict::RequestChanges);
        assert_eq!(fb.body, "REQUEST_CHANGES\n\nfix it");
        assert_eq!(fb.path, PathBuf::from("/abs/alice.md"));
        assert_eq!(fb.created_at, 250);
    }

    #[test]
    fn plan_gate_skips_multi_plan_touch_to_earlier_reviewable() {
        // Commit A: PlanOnly intro (reviewable, codex approved).
        // Commit B: MultiPlan touch (touches foo + bar, non-reviewable).
        // Naive "latest plan touch" routing would land on B and find
        // no gate entry in Plan.commits, falling through to "initial
        // review" — wrong. The fix walks newest-first looking for a
        // PlanOnly/Mixed commit, so the gate stays on A's Approved.
        let mut touches = BTreeMap::new();
        touches.insert(cs("a"), touches_one("foo", PlanTouchKind::Intro));
        touches.insert(
            cs("b"),
            vec![
                (pk("foo"), PlanTouchKind::Revision),
                (pk("bar"), PlanTouchKind::Revision),
            ],
        );
        let mut attribution = BTreeMap::new();
        attribution.insert(
            cs("a"),
            attributed("foo", Some(PlanTouchKind::Intro), false),
        );
        attribution.insert(cs("b"), AttributionResult::Unattributed);

        let mut commits = BTreeMap::new();
        commits.insert(
            cs("a"),
            CommitGate {
                state: CommitGateState::Approved,
                participants: vec![al("codex")],
                approvers: vec![al("codex")],
                requesters: Vec::new(),
                ambiguous: Vec::new(),
                missing: Vec::new(),
                feedback: BTreeMap::new(),
            },
        );

        let gate = plan_gate_for_parts(
            &pk("foo"),
            &commits,
            &[cs("a"), cs("b")],
            &touches,
            &attribution,
        )
        .expect("plan gate should resolve to A");
        assert_eq!(gate.state, ReviewGateState::Ready);
        assert_eq!(gate.approvals, vec![al("codex")]);
    }

    #[test]
    fn plan_gate_skips_done_move_to_earlier_reviewable() {
        // A (PlanOnly, Approved) → B (DoneMove). Plan gate should
        // resolve to A's gate, not return None.
        let mut touches = BTreeMap::new();
        touches.insert(cs("a"), touches_one("foo", PlanTouchKind::Intro));
        touches.insert(cs("b"), touches_one("foo", PlanTouchKind::DoneMove));
        let mut attribution = BTreeMap::new();
        attribution.insert(
            cs("a"),
            attributed("foo", Some(PlanTouchKind::Intro), false),
        );
        attribution.insert(
            cs("b"),
            attributed("foo", Some(PlanTouchKind::DoneMove), false),
        );

        let mut commits = BTreeMap::new();
        commits.insert(
            cs("a"),
            CommitGate {
                state: CommitGateState::Approved,
                participants: vec![al("codex")],
                approvers: vec![al("codex")],
                requesters: Vec::new(),
                ambiguous: Vec::new(),
                missing: Vec::new(),
                feedback: BTreeMap::new(),
            },
        );

        let gate = plan_gate_for_parts(
            &pk("foo"),
            &commits,
            &[cs("a"), cs("b")],
            &touches,
            &attribution,
        )
        .expect("plan gate should resolve to A");
        assert_eq!(gate.state, ReviewGateState::Ready);
    }
}
