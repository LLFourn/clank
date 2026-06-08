//! Gate computation and agent-perspective wait surface.
//!
//! - [`compute_gate`] — the single gate-state function.
//!   Maps a commit's review set to one of
//!   [`CommitGateState::{Unreviewed, Approved, Finished,
//!   ChangesRequested}`](crate::vocab::CommitGateState).
//! - [`RepoState::derive_status`] (impl on `RepoState`) — folds
//!   `compute_gate` over every plan and produces the
//!   role-flavored [`WaitItem`]s `clank wfw` emits.
//! - [`detect_finished`] — pure comparison between a startup
//!   snapshot of watched plans and the current `RepoState`.
//!   Emits `WaitItem::Finished` for every watched plan that
//!   newly transitioned into `finished_plans` since startup.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::ids::{AgentLabel, CommitSha, PlanKey};
use crate::plan_view::{PlanBlock, WaitingOn};
use crate::repo_state::RepoState;
use crate::vocab::{Role, WaitingReason};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MasterNext {
    /// REQUEST_CHANGES on the latest reviewable. Address + recommit.
    Revise,
    /// Plan file has uncommitted edits. Commit before any further
    /// work or review motion can land — uncommitted plan changes
    /// supersede whatever the last committed version's gate state
    /// says.
    Commit,
    /// APPROVED (not FINISHED) on the latest reviewable. Master
    /// keeps working — more impl, more docs, more tests, or
    /// prompting the reviewer to upgrade to FINISHED.
    Continue,
    /// FINISHED on the latest reviewable. Run `clank finish`.
    Finalize,
}

/// Flat tagged list `wfw` returns from one refold round.
/// Actionable items (`Master`, `Reviewer`) and terminal events
/// (`Finished`) sit at the same level. Empty list at the call
/// site means "no outcome, keep blocking."
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WaitItem {
    Master {
        plan: PlanKey,
        sha: CommitSha,
        next: MasterNext,
        reason: WaitingReason,
        gate: crate::vocab::CommitGateState,
    },
    Reviewer {
        plan: PlanKey,
        sha: CommitSha,
        feedback_path: String,
    },
    Finished {
        plan: PlanKey,
        finalized_at: CommitSha,
    },
    Idle {
        prompt: String,
    },
    AdHocReview {
        sha: CommitSha,
        feedback_path: String,
    },
    AdHocRevise {
        sha: CommitSha,
    },
    PromoteFromQueue {
        name: String,
        priority: u16,
    },
    Blocked {
        agent: String,
        name: String,
        plan: Option<String>,
        question: String,
    },
    Unblocked {
        name: String,
        plan: Option<String>,
        answer: String,
    },
}

/// Snapshot taken once at `wfw` startup. `detect_finished` compares
/// the current state against this to decide which watched plans
/// newly transitioned to finished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupSnapshot {
    /// Plan keys this wfw is responsible for. With `--plan foo`,
    /// `{foo}`. Without `--plan`, the active plan keys at startup.
    pub watched: BTreeSet<PlanKey>,
    /// Finalize SHAs already present at startup, keyed by plan.
    /// A watched plan is "newly finished" iff its current
    /// `FinishedPlan` entry has a `finalized_at` NOT in this set.
    /// The set-per-plan stays correct across re-intro /
    /// re-finalize cycles.
    pub finished_at_startup: BTreeMap<PlanKey, BTreeSet<CommitSha>>,
}

impl StartupSnapshot {
    /// Build from initial fold + the resolved `--plan` filter.
    /// Without a filter the watched set is every active plan at
    /// startup; with one it's the singleton.
    pub fn capture(state: &RepoState, plan_filter: Option<&PlanKey>) -> Self {
        let watched: BTreeSet<PlanKey> = match plan_filter {
            Some(k) => std::iter::once(k.clone()).collect(),
            None => state.plans.keys().cloned().collect(),
        };
        let mut finished_at_startup: BTreeMap<PlanKey, BTreeSet<CommitSha>> = BTreeMap::new();
        for fp in &state.finished_plans {
            finished_at_startup
                .entry(fp.plan.clone())
                .or_default()
                .insert(fp.finalized_at.clone());
        }
        Self {
            watched,
            finished_at_startup,
        }
    }
}

// ── PlanStateLookup trait + derive_status ──────────────────

/// Filesystem-projection adapter for one `derive_status` fold round.
/// Surfaces both review feedback and pending blocks for the per-plan
/// gate computation. Renamed from `ReviewLookup` in
/// `status-blocks-dominate-gate` once `blocks_for` joined the trait
/// surface — "review lookup" was a name lie after the extension.
pub trait PlanStateLookup {
    fn reviews_for(&self, sha: &CommitSha) -> Vec<ReviewEntry>;
    fn worktree_status(&self, plan: &PlanKey) -> crate::vocab::PlanWorktreeStatus;
    /// Pending (unanswered) plan-scoped blocks for the given plan.
    /// Default impl returns `vec![]` so existing test mocks keep
    /// compiling without block awareness.
    fn blocks_for(&self, _plan: &PlanKey) -> Vec<PlanBlock> {
        Vec::new()
    }
}

#[derive(Clone)]
pub struct ReviewEntry {
    pub author: AgentLabel,
    pub verdict: crate::vocab::Verdict,
}

pub struct WorkPolicy {
    pub plan_feedback: bool,
    pub adhoc_feedback: bool,
    /// Commit-tier reviewers: review every commit on a plan
    /// branch. Master waits for all of them per-commit. Plan:
    /// `teams-based-agent-registration` — replaces the prior
    /// single-list `expected_reviewers` field.
    pub commit_reviewers: Vec<AgentLabel>,
    /// Gate-tier reviewers: only review at gate-transition
    /// moments (when commit-reviewers are all positive). Used
    /// for architectural / structural checks at structural
    /// moments rather than per-commit churn.
    pub gate_reviewers: Vec<AgentLabel>,
}

#[derive(Debug, Clone)]
pub struct WorkStatus {
    pub plans: Vec<PlanWorkState>,
    pub ad_hoc: Vec<AdHocWorkState>,
}

#[derive(Debug, Clone)]
pub struct PlanWorkState {
    pub plan: PlanKey,
    /// `Some(sha)` for plans with a reviewable commit (the normal
    /// case). `None` for blocked plans that have no reviewable
    /// commit yet (e.g. an intro-only plan with an open block).
    /// Renderer omits the `latest reviewable:` line when this is
    /// None.
    pub sha: Option<CommitSha>,
    pub gate: crate::vocab::CommitGateState,
    pub waiting_on: WaitingOn,
    pub touched_code: bool,
}

#[derive(Debug, Clone)]
pub struct AdHocWorkState {
    pub sha: CommitSha,
    pub gate: crate::vocab::CommitGateState,
}

/// Compute the gate state for one commit given its review
/// entries and the two-tier reviewer split from the team
/// composition.
///
/// Plan: `teams-based-agent-registration`. The five-state
/// machine extends the prior shape with `ApprovedPendingGate`:
/// commit-reviewers all approved but gate-reviewers haven't
/// voted yet. Gate-reviewers wake during this state; master
/// sleeps.
///
/// Empty-set rule (the "devolve" property from the plan body):
/// when `commit_reviewers` is empty, "all commit-reviewers
/// approved" is vacuously true on the empty set, so the state
/// machine transitions to `ApprovedPendingGate` immediately on
/// the first commit. Gate-reviewers fire per-commit. No special
/// case in code — set semantics produce it.
pub fn compute_gate(
    reviews: &[ReviewEntry],
    commit_reviewers: &[AgentLabel],
    gate_reviewers: &[AgentLabel],
) -> crate::vocab::CommitGateState {
    use crate::vocab::{CommitGateState, Verdict};

    // Zero-reviewer mode (master-only repo): preserve
    // pre-plan behavior — gate stays Approved (NOT
    // Finished). Master keeps working and runs `clank
    // finish` when they decide; auto-Finished would noise
    // every commit with "ready to finalize." Plan body
    // initially said Finished here; that was wrong — fixed
    // during implementation.
    if commit_reviewers.is_empty() && gate_reviewers.is_empty() {
        return CommitGateState::Approved;
    }

    // Filter reviews to expected reviewers across BOTH tiers.
    // Stale entries from removed reviewers (or other authors
    // whose feedback dir still exists) MUST NOT gate decisions.
    let expected: std::collections::HashSet<&AgentLabel> = commit_reviewers
        .iter()
        .chain(gate_reviewers.iter())
        .collect();
    let reviews: Vec<&ReviewEntry> = reviews
        .iter()
        .filter(|r| expected.contains(&r.author))
        .collect();

    // ChangesRequested: any reviewer (commit OR gate) voted
    // Request-Changes or Unmarked. Trumps everything.
    if reviews
        .iter()
        .any(|r| r.verdict == Verdict::RequestChanges || r.verdict == Verdict::Unmarked)
    {
        return CommitGateState::ChangesRequested;
    }

    // Build label → verdict map for the "all approved/finished"
    // checks.
    let by_label: std::collections::HashMap<&AgentLabel, &Verdict> =
        reviews.iter().map(|r| (&r.author, &r.verdict)).collect();

    let positive = |label: &AgentLabel| -> bool {
        matches!(
            by_label.get(label),
            Some(Verdict::Approve | Verdict::Finished)
        )
    };

    let all_commit_positive = commit_reviewers.iter().all(positive);
    if !all_commit_positive {
        return CommitGateState::Unreviewed;
    }

    // All commit-reviewers signed off (approve/finished). Now
    // check gate-reviewers — they only contribute once
    // commit-reviewers are unanimous.
    let all_gate_positive = gate_reviewers.iter().all(positive);
    if !all_gate_positive {
        return CommitGateState::ApprovedPendingGate;
    }

    // Both tiers positive. Decide Finished vs Approved by
    // whether every reviewer is at Finished verdict.
    let finished =
        |label: &AgentLabel| -> bool { matches!(by_label.get(label), Some(Verdict::Finished)) };
    let all_finished = commit_reviewers.iter().all(finished) && gate_reviewers.iter().all(finished);
    if all_finished {
        CommitGateState::Finished
    } else {
        CommitGateState::Approved
    }
}

impl RepoState {
    pub fn derive_status(&self, reviews: &impl PlanStateLookup, policy: &WorkPolicy) -> WorkStatus {
        use crate::vocab::{CommitGateState, PlanWorktreeStatus};

        let mut plans = Vec::new();
        {
            for (key, ps) in &self.plans {
                // Block precedence: pending plan-scoped blocks
                // dominate the review gate AND surface plans that
                // have no reviewable commit yet (intro-only with
                // an open block). Block check runs BEFORE the
                // empty-reviewable early-continue.
                let mut pending_blocks = reviews.blocks_for(key);
                if !pending_blocks.is_empty() {
                    // Lexicographic tie-breaker by (creator, name)
                    // matches scan_blocks's sort order. The first
                    // pending block surfaces on the gate line.
                    pending_blocks.sort_by(|a, b| {
                        a.creator
                            .as_str()
                            .cmp(b.creator.as_str())
                            .then_with(|| a.name.cmp(&b.name))
                    });
                    let block = pending_blocks.into_iter().next().unwrap();
                    let reviewable = ps.reviewable_shas();
                    let sha = reviewable.last().cloned();
                    let latest_event = sha
                        .as_ref()
                        .and_then(|s| ps.commits.iter().rev().find(|e| &e.sha == s));
                    let touched_code = latest_event.is_some_and(|e| e.touched_code);
                    plans.push(PlanWorkState {
                        plan: key.clone(),
                        sha,
                        gate: CommitGateState::Blocked,
                        waiting_on: WaitingOn::Blocked { block },
                        touched_code,
                    });
                    continue;
                }

                let reviewable = ps.reviewable_shas();
                if reviewable.is_empty() {
                    continue;
                }
                let latest_sha = reviewable.last().unwrap().clone();
                let latest_event = ps.commits.iter().rev().find(|e| e.sha == latest_sha);
                let touched_code = latest_event.map_or(false, |e| e.touched_code);

                let entries = reviews.reviews_for(&latest_sha);
                let gate = if policy.plan_feedback {
                    compute_gate(&entries, &policy.commit_reviewers, &policy.gate_reviewers)
                } else {
                    // When plan review is disabled, treat as approved
                    // so master isn't blocked.
                    if entries
                        .iter()
                        .any(|r| r.verdict == crate::vocab::Verdict::RequestChanges)
                    {
                        CommitGateState::ChangesRequested
                    } else {
                        CommitGateState::Approved
                    }
                };

                // Filter to expected reviewers for any downstream uses
                // (MasterToRevise payload, missing-set computation). Same
                // principle as compute_gate's filter. Both tiers.
                let expected: std::collections::HashSet<&AgentLabel> = policy
                    .commit_reviewers
                    .iter()
                    .chain(policy.gate_reviewers.iter())
                    .collect();
                let filtered_entries: Vec<&ReviewEntry> = entries
                    .iter()
                    .filter(|r| expected.contains(&r.author))
                    .collect();

                let worktree = reviews.worktree_status(key);
                // Worktree-first dispatch: an uncommitted plan edit
                // is author intent that supersedes the gate's view of
                // the last committed version. Routing the master to
                // anything other than Commit is asking them to make
                // decisions on a version they're already replacing,
                // AND wakes reviewers to a doc that's about to change.
                //
                // Block precedence still wins (handled by the
                // `continue` above), so this only runs for unblocked
                // plans.
                //
                // Plan: wfw-master-returns-on-dirty-plan-any-gate.
                let waiting_on = if worktree == PlanWorktreeStatus::BodyDirty {
                    WaitingOn::MasterToCommit
                } else {
                    match gate {
                        CommitGateState::Blocked => {
                            // Unreachable: the block-precedence check above
                            // already consumed any blocked plan via the
                            // continue. compute_gate cannot return Blocked
                            // (it's a per-commit review verdict; Blocked is
                            // a plan-level state). This arm exists only to
                            // satisfy match exhaustiveness.
                            unreachable!(
                                "compute_gate cannot return Blocked; plan-level Blocked is handled above"
                            );
                        }
                        CommitGateState::ChangesRequested => {
                            let requesters = filtered_entries
                                .iter()
                                .filter(|r| r.verdict == crate::vocab::Verdict::RequestChanges)
                                .map(|r| r.author.clone())
                                .collect();
                            let ambiguous = filtered_entries
                                .iter()
                                .filter(|r| r.verdict == crate::vocab::Verdict::Unmarked)
                                .map(|r| r.author.clone())
                                .collect();
                            WaitingOn::MasterToRevise {
                                requesters,
                                ambiguous,
                            }
                        }
                        CommitGateState::Finished => WaitingOn::MasterToFinalize,
                        CommitGateState::Approved => WaitingOn::MasterToContinue,
                        CommitGateState::ApprovedPendingGate => {
                            // All commit-reviewers signed off; gate-
                            // reviewers haven't all voted yet. Master
                            // sleeps; gate-reviewers wake.
                            let approved_by: std::collections::HashSet<&AgentLabel> =
                                filtered_entries
                                    .iter()
                                    .filter(|r| {
                                        matches!(
                                            r.verdict,
                                            crate::vocab::Verdict::Approve
                                                | crate::vocab::Verdict::Finished
                                        )
                                    })
                                    .map(|r| &r.author)
                                    .collect();
                            let missing_gate: Vec<AgentLabel> = policy
                                .gate_reviewers
                                .iter()
                                .filter(|label| !approved_by.contains(label))
                                .cloned()
                                .collect();
                            let missing = crate::repo_state::NonEmptyVec::new(missing_gate)
                                .expect(
                                    "ApprovedPendingGate state implies a non-empty missing gate-reviewer set",
                                );
                            WaitingOn::GateReviewersMissing { missing }
                        }
                        CommitGateState::Unreviewed => {
                            // Compute the set of commit-reviewers that
                            // haven't posted Approve or Finished. Stale
                            // RequestChanges from removed authors don't
                            // count because the filter dropped them.
                            let approved_by: std::collections::HashSet<&AgentLabel> =
                                filtered_entries
                                    .iter()
                                    .filter(|r| {
                                        matches!(
                                            r.verdict,
                                            crate::vocab::Verdict::Approve
                                                | crate::vocab::Verdict::Finished
                                        )
                                    })
                                    .map(|r| &r.author)
                                    .collect();
                            let missing: Vec<AgentLabel> = policy
                                .commit_reviewers
                                .iter()
                                .filter(|label| !approved_by.contains(label))
                                .cloned()
                                .collect();
                            // `missing` is non-empty here: Unreviewed is
                            // reached only when at least one
                            // commit-reviewer hasn't posted positive.
                            let missing = crate::repo_state::NonEmptyVec::new(missing).expect(
                                "Unreviewed gate state implies a non-empty missing commit-reviewer set",
                            );
                            WaitingOn::ReviewerApprovalsMissing { missing }
                        }
                    }
                };

                plans.push(PlanWorkState {
                    plan: key.clone(),
                    sha: Some(latest_sha),
                    gate,
                    waiting_on,
                    touched_code,
                });
            }
        }

        let mut ad_hoc = Vec::new();
        if policy.adhoc_feedback {
            if let Some(event) = self.ad_hoc.last() {
                let entries = reviews.reviews_for(&event.sha);
                let gate = compute_gate(&entries, &policy.commit_reviewers, &policy.gate_reviewers);
                ad_hoc.push(AdHocWorkState {
                    sha: event.sha.clone(),
                    gate,
                });
            }
        }

        WorkStatus { plans, ad_hoc }
    }
}

impl WorkStatus {
    pub fn work_for(&self, author: &AgentLabel, role: Role) -> Vec<WaitItem> {
        let mut out = Vec::new();
        for ps in &self.plans {
            // Safe: only non-blocked plans reach the WaitItem-emitting
            // arms below, and the block-precedence path in
            // `derive_status` guarantees those have Some(sha). The
            // Blocked first-arm catches any blocked plan before the
            // unwrap is reached.
            let sha_for_item = || {
                ps.sha
                    .clone()
                    .expect("non-blocked plan must have a reviewable sha")
            };
            match (role, &ps.waiting_on) {
                // blocked plans emit no work for any role —
                // block-creator clears the block out-of-band
                (_, WaitingOn::Blocked { .. }) => {}
                (Role::Master, WaitingOn::MasterToRevise { .. }) => {
                    out.push(WaitItem::Master {
                        plan: ps.plan.clone(),
                        sha: sha_for_item(),
                        next: MasterNext::Revise,
                        reason: WaitingReason::AddressCommitChanges,
                        gate: ps.gate,
                    });
                }
                (Role::Master, WaitingOn::MasterToCommit) => {
                    out.push(WaitItem::Master {
                        plan: ps.plan.clone(),
                        sha: sha_for_item(),
                        next: MasterNext::Commit,
                        reason: WaitingReason::CommitPlanRevision,
                        gate: ps.gate,
                    });
                }
                (Role::Master, WaitingOn::MasterToContinue) => {
                    out.push(WaitItem::Master {
                        plan: ps.plan.clone(),
                        sha: sha_for_item(),
                        next: MasterNext::Continue,
                        reason: WaitingReason::GateApproved,
                        gate: ps.gate,
                    });
                }
                (Role::Master, WaitingOn::MasterToFinalize) => {
                    out.push(WaitItem::Master {
                        plan: ps.plan.clone(),
                        sha: sha_for_item(),
                        next: MasterNext::Finalize,
                        reason: WaitingReason::ReadyToFinalize,
                        gate: ps.gate,
                    });
                }
                (Role::Reviewer, WaitingOn::ReviewerApprovalsMissing { missing })
                    if missing.as_slice().iter().any(|l| l == author) =>
                {
                    // Only emit a review item for this reviewer if THEY
                    // are in the missing set. Reviewers who have already
                    // posted APPROVE/FINISHED don't get redundant wakes.
                    let sha = sha_for_item();
                    out.push(WaitItem::Reviewer {
                        plan: ps.plan.clone(),
                        feedback_path: format!(
                            ".clank/agents/{}/feedback/{}.md",
                            author.as_str(),
                            sha.as_str()
                        ),
                        sha,
                    });
                }
                (Role::Reviewer, WaitingOn::GateReviewersMissing { missing })
                    if missing.as_slice().iter().any(|l| l == author) =>
                {
                    // Gate-tier reviewer wake (codex 8cb01b6 catch):
                    // ApprovedPendingGate means commit-reviewers are
                    // all positive; gate-reviewers who haven't voted
                    // yet need to weigh in. Same shape as the
                    // commit-tier wake above — emit a Reviewer item
                    // when THIS reviewer is in the missing set.
                    // Plan: `teams-based-agent-registration`.
                    let sha = sha_for_item();
                    out.push(WaitItem::Reviewer {
                        plan: ps.plan.clone(),
                        feedback_path: format!(
                            ".clank/agents/{}/feedback/{}.md",
                            author.as_str(),
                            sha.as_str()
                        ),
                        sha,
                    });
                }
                _ => {}
            }
        }
        for ah in &self.ad_hoc {
            match (role, ah.gate) {
                (Role::Reviewer, crate::vocab::CommitGateState::Unreviewed) => {
                    out.push(WaitItem::AdHocReview {
                        sha: ah.sha.clone(),
                        feedback_path: format!(
                            ".clank/agents/{}/feedback/{}.md",
                            author.as_str(),
                            ah.sha.as_str()
                        ),
                    });
                }
                (Role::Master, crate::vocab::CommitGateState::ChangesRequested) => {
                    out.push(WaitItem::AdHocRevise {
                        sha: ah.sha.clone(),
                    });
                }
                _ => {}
            }
        }
        out
    }
}

/// Plan-key-ordered `Finished` items for every watched plan that
/// transitioned into `finished_plans` since the snapshot. Re-intro /
/// re-finalize cycles produce a new item iff the `finalized_at`
/// SHA wasn't in the snapshot's per-plan set.
pub fn detect_finished(snapshot: &StartupSnapshot, current: &RepoState) -> Vec<WaitItem> {
    let mut out: BTreeMap<PlanKey, CommitSha> = BTreeMap::new();
    for fp in &current.finished_plans {
        if !snapshot.watched.contains(&fp.plan) {
            continue;
        }
        let known = snapshot
            .finished_at_startup
            .get(&fp.plan)
            .map(|set| set.contains(&fp.finalized_at))
            .unwrap_or(false);
        if known {
            continue;
        }
        // If a plan finalized multiple times since startup (extreme
        // edge case), surface the latest one. `finished_plans` is
        // append-only so the last entry per plan wins.
        out.insert(fp.plan.clone(), fp.finalized_at.clone());
    }
    out.into_iter()
        .map(|(plan, finalized_at)| WaitItem::Finished { plan, finalized_at })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo_state::{FinishedPlan, PlanState};
    use crate::vocab::{CommitGateState, PlanWorktreeStatus};

    fn plan(s: &str) -> PlanKey {
        PlanKey::parse(s).unwrap()
    }
    fn sha(s: &str) -> CommitSha {
        CommitSha::parse(&format!("{s:0<40}")).unwrap()
    }
    fn label(s: &str) -> AgentLabel {
        AgentLabel::parse(s).unwrap()
    }

    fn entry(verdict: crate::vocab::Verdict, who: &str) -> ReviewEntry {
        ReviewEntry {
            author: label(who),
            verdict,
        }
    }

    #[test]
    fn compute_gate_zero_reviewers_is_approved_even_with_no_reviews() {
        // Master-only repo: every commit auto-approves.
        assert_eq!(compute_gate(&[], &[], &[]), CommitGateState::Approved);
    }

    #[test]
    fn compute_gate_zero_reviewers_ignores_stale_request_changes() {
        // Removed reviewer's stale RC must not gate.
        let reviews = [entry(crate::vocab::Verdict::RequestChanges, "alice")];
        assert_eq!(compute_gate(&reviews, &[], &[]), CommitGateState::Approved);
    }

    #[test]
    fn compute_gate_single_expected_approve_is_approved() {
        let reviews = [entry(crate::vocab::Verdict::Approve, "codex")];
        assert_eq!(
            compute_gate(&reviews, &[label("codex")], &[]),
            CommitGateState::Approved
        );
    }

    #[test]
    fn compute_gate_single_expected_request_changes() {
        let reviews = [entry(crate::vocab::Verdict::RequestChanges, "codex")];
        assert_eq!(
            compute_gate(&reviews, &[label("codex")], &[]),
            CommitGateState::ChangesRequested
        );
    }

    #[test]
    fn compute_gate_two_expected_both_approve_is_approved() {
        let reviews = [
            entry(crate::vocab::Verdict::Approve, "codex"),
            entry(crate::vocab::Verdict::Approve, "ruthless"),
        ];
        assert_eq!(
            compute_gate(&reviews, &[label("codex"), label("ruthless")], &[]),
            CommitGateState::Approved
        );
    }

    #[test]
    fn compute_gate_two_expected_one_request_changes_blocks() {
        let reviews = [
            entry(crate::vocab::Verdict::Approve, "codex"),
            entry(crate::vocab::Verdict::RequestChanges, "ruthless"),
        ];
        assert_eq!(
            compute_gate(&reviews, &[label("codex"), label("ruthless")], &[]),
            CommitGateState::ChangesRequested
        );
    }

    #[test]
    fn compute_gate_two_expected_one_missing_is_unreviewed() {
        let reviews = [entry(crate::vocab::Verdict::Approve, "codex")];
        assert_eq!(
            compute_gate(&reviews, &[label("codex"), label("ruthless")], &[]),
            CommitGateState::Unreviewed
        );
    }

    #[test]
    fn compute_gate_two_expected_both_finished_is_finished() {
        let reviews = [
            entry(crate::vocab::Verdict::Finished, "codex"),
            entry(crate::vocab::Verdict::Finished, "ruthless"),
        ];
        assert_eq!(
            compute_gate(&reviews, &[label("codex"), label("ruthless")], &[]),
            CommitGateState::Finished
        );
    }

    #[test]
    fn compute_gate_one_finished_one_approve_is_approved_not_finished() {
        // The critical contract: ALL must Finish, not just one.
        let reviews = [
            entry(crate::vocab::Verdict::Finished, "codex"),
            entry(crate::vocab::Verdict::Approve, "ruthless"),
        ];
        assert_eq!(
            compute_gate(&reviews, &[label("codex"), label("ruthless")], &[]),
            CommitGateState::Approved
        );
    }

    #[test]
    fn compute_gate_unmarked_treated_as_changes() {
        let reviews = [entry(crate::vocab::Verdict::Unmarked, "codex")];
        assert_eq!(
            compute_gate(&reviews, &[label("codex")], &[]),
            CommitGateState::ChangesRequested
        );
    }

    // ── teams-based-agent-registration: two-tier compute_gate
    //    edge cases (ruthless pin 1 — defends compute_gate's
    //    PRODUCTION of the new states; the work_for tests above
    //    test RECEPTION but bypass compute_gate by constructing
    //    WorkStatus directly).

    #[test]
    fn compute_gate_empty_commit_nonempty_gate_returns_approved_pending_gate_immediately() {
        // The "devolve" property: empty commit_reviewers means
        // "all commit approved" is vacuously true → state
        // transitions to ApprovedPendingGate immediately on
        // first commit (no reviews yet). Gate-reviewer fires
        // per-commit, functionally as if they were commit-tier.
        // No special case in code — set semantics produce it.
        assert_eq!(
            compute_gate(&[], &[], &[label("ruthless")]),
            CommitGateState::ApprovedPendingGate
        );
    }

    #[test]
    fn compute_gate_empty_both_returns_approved_not_finished() {
        // Master-only repo: empty commit + empty gate → Approved
        // (NOT Finished). Preserves pre-plan behavior; lloyd
        // 2026-06-08 directive (codex 8cb01b6 catch on the
        // earlier plan-body drift).
        assert_eq!(compute_gate(&[], &[], &[]), CommitGateState::Approved);
    }

    #[test]
    fn compute_gate_gate_reviewer_rc_trumps_state() {
        // Gate-tier reviewer's Request-Changes → ChangesRequested
        // regardless of commit-tier verdicts. The "any reviewer
        // RC" rule applies uniformly across both tiers.
        let reviews = [
            entry(crate::vocab::Verdict::Finished, "codex"),
            entry(crate::vocab::Verdict::RequestChanges, "ruthless"),
        ];
        assert_eq!(
            compute_gate(&reviews, &[label("codex")], &[label("ruthless")]),
            CommitGateState::ChangesRequested
        );
    }

    #[test]
    fn compute_gate_gate_reviewer_approves_returns_approved_not_finished() {
        // All reviewers across both tiers signed off, but ≥1
        // only Approved (not Finished) → state = Approved (NOT
        // Finished). Same shape as single-tier "one approve, one
        // finished" but with the second positive coming from
        // gate-tier.
        let reviews = [
            entry(crate::vocab::Verdict::Finished, "codex"),
            entry(crate::vocab::Verdict::Approve, "ruthless"),
        ];
        assert_eq!(
            compute_gate(&reviews, &[label("codex")], &[label("ruthless")]),
            CommitGateState::Approved
        );
    }

    #[test]
    fn compute_gate_gate_reviewer_vote_before_commit_keeps_unreviewed() {
        // Edge case 5 from the plan body: gate-reviewer who
        // watches early may vote before commit-reviewers do.
        // Their verdict is stored, but the state machine reads
        // commit-reviewers FIRST. The latent gate-reviewer vote
        // is ignored until commit-reviewers all positive.
        let reviews = [entry(crate::vocab::Verdict::Approve, "ruthless")];
        assert_eq!(
            compute_gate(&reviews, &[label("codex")], &[label("ruthless")]),
            CommitGateState::Unreviewed
        );
    }

    #[test]
    fn compute_gate_two_tier_all_finished_returns_finished() {
        // Both tiers all Finished → state = Finished. Completes
        // the cross-tier verdict matrix.
        let reviews = [
            entry(crate::vocab::Verdict::Finished, "codex"),
            entry(crate::vocab::Verdict::Finished, "ruthless"),
        ];
        assert_eq!(
            compute_gate(&reviews, &[label("codex")], &[label("ruthless")]),
            CommitGateState::Finished
        );
    }

    #[test]
    fn compute_gate_filters_stale_request_changes_from_removed_reviewer() {
        // Symmetric case: alice was removed but the dir still exists
        // and emits a stale RC. Both expected reviewers APPROVED.
        // Gate must be Approved (not ChangesRequested from alice).
        let reviews = [
            entry(crate::vocab::Verdict::RequestChanges, "alice"), // removed
            entry(crate::vocab::Verdict::Approve, "codex"),
            entry(crate::vocab::Verdict::Approve, "ruthless"),
        ];
        assert_eq!(
            compute_gate(&reviews, &[label("codex"), label("ruthless")], &[]),
            CommitGateState::Approved
        );
    }

    #[test]
    fn compute_gate_filter_drops_stale_finished_from_removed_when_someone_missing() {
        // Two expected reviewers; one APPROVE'd, one missing.
        // A removed-reviewer FINISHED must not push gate to Finished.
        let reviews = [
            entry(crate::vocab::Verdict::Finished, "alice"), // removed
            entry(crate::vocab::Verdict::Approve, "codex"),
            // ruthless missing
        ];
        assert_eq!(
            compute_gate(&reviews, &[label("codex"), label("ruthless")], &[]),
            CommitGateState::Unreviewed
        );
    }

    // ============ work_for: per-author reviewer wake ============

    fn work_status_with_one_plan(waiting_on: WaitingOn) -> WorkStatus {
        WorkStatus {
            plans: vec![PlanWorkState {
                plan: plan("a.md"),
                sha: Some(sha("aaaa")),
                gate: CommitGateState::Unreviewed,
                waiting_on,
                touched_code: false,
            }],
            ad_hoc: Vec::new(),
        }
    }

    fn missing(labels: &[&str]) -> WaitingOn {
        let missing = labels.iter().map(|l| label(l)).collect();
        let missing = crate::repo_state::NonEmptyVec::new(missing).unwrap();
        WaitingOn::ReviewerApprovalsMissing { missing }
    }

    fn gate_missing(labels: &[&str]) -> WaitingOn {
        let missing = labels.iter().map(|l| label(l)).collect();
        let missing = crate::repo_state::NonEmptyVec::new(missing).unwrap();
        WaitingOn::GateReviewersMissing { missing }
    }

    #[test]
    fn work_for_missing_reviewer_gets_review_item() {
        // Two registered reviewers; neither has reviewed yet. Both
        // are in the missing set; each gets a review item.
        let ws = work_status_with_one_plan(missing(&["codex", "ruthless"]));
        let codex_work = ws.work_for(&label("codex"), Role::Reviewer);
        assert_eq!(codex_work.len(), 1, "codex should get a review item");
        assert!(matches!(codex_work[0], WaitItem::Reviewer { .. }));

        let ruthless_work = ws.work_for(&label("ruthless"), Role::Reviewer);
        assert_eq!(ruthless_work.len(), 1, "ruthless should get a review item");
    }

    #[test]
    fn work_for_missing_gate_reviewer_gets_review_item() {
        // Plan: teams-based-agent-registration. ApprovedPendingGate
        // state means commit-tier reviewers all approved; the
        // listed gate-tier reviewers need to weigh in. Each
        // missing gate-reviewer should get a Reviewer item via
        // work_for (codex 8cb01b6 catch).
        let ws = work_status_with_one_plan(gate_missing(&["ruthless"]));
        let ruthless_work = ws.work_for(&label("ruthless"), Role::Reviewer);
        assert_eq!(
            ruthless_work.len(),
            1,
            "ruthless is the missing gate-reviewer; should get a review item"
        );
        assert!(matches!(ruthless_work[0], WaitItem::Reviewer { .. }));
    }

    #[test]
    fn work_for_non_missing_gate_reviewer_gets_no_item() {
        // Symmetric to the commit-tier "already approved" rule:
        // if a gate-reviewer has already posted positive (so
        // isn't in the missing set), no redundant wake.
        let ws = work_status_with_one_plan(gate_missing(&["ruthless"]));
        let other_work = ws.work_for(&label("codex"), Role::Reviewer);
        assert!(
            other_work.is_empty(),
            "codex isn't in the missing gate-reviewer set; should not be woken"
        );
    }

    #[test]
    fn work_for_master_in_approved_pending_gate_state_gets_no_item() {
        // The whole point of ApprovedPendingGate: master sleeps
        // while gate-reviewers weigh in. Even though there's a
        // `master` role asking for work, no Master item should
        // emit because the gate hasn't transitioned to Approved
        // yet.
        let ws = work_status_with_one_plan(gate_missing(&["ruthless"]));
        let master_work = ws.work_for(&label("lloyd"), Role::Master);
        assert!(
            master_work.is_empty(),
            "master should sleep while gate-reviewers haven't voted; got {master_work:?}"
        );
    }

    #[test]
    fn work_for_already_approved_reviewer_gets_no_item() {
        // The load-bearing UX guarantee: a reviewer who has already
        // posted APPROVE/FINISHED does NOT get a redundant wake.
        // Only `ruthless` is in `missing` (codex already approved).
        let ws = work_status_with_one_plan(missing(&["ruthless"]));
        let codex_work = ws.work_for(&label("codex"), Role::Reviewer);
        assert!(
            codex_work.is_empty(),
            "codex already approved; should not be woken again"
        );
        let ruthless_work = ws.work_for(&label("ruthless"), Role::Reviewer);
        assert_eq!(
            ruthless_work.len(),
            1,
            "ruthless is the missing reviewer; should get the item"
        );
    }

    #[test]
    fn work_for_stale_reviewer_not_in_expected_gets_no_item() {
        // alice is no longer expected (her dir might still exist on
        // disk but she's not in the missing set). work_for returns
        // nothing for her.
        let ws = work_status_with_one_plan(missing(&["codex"]));
        let alice_work = ws.work_for(&label("alice"), Role::Reviewer);
        assert!(
            alice_work.is_empty(),
            "alice is not in missing; should not be woken"
        );
    }

    #[test]
    fn work_for_master_unaffected_by_missing_reviewers() {
        // Reviewers gate uses ReviewerApprovalsMissing; master's role
        // does not emit a Reviewer item, and no Master variant fires
        // for this gate state. Verify master gets nothing here.
        let ws = work_status_with_one_plan(missing(&["codex"]));
        let master_work = ws.work_for(&label("lloyd"), Role::Master);
        assert!(
            master_work.is_empty(),
            "master has no work while reviewers are still owed"
        );
    }

    // ============ detect_finished ============

    fn state_with(finished: Vec<FinishedPlan>) -> RepoState {
        let mut s = RepoState::default();
        s.finished_plans = finished;
        s
    }

    fn finished(stem: &str, intro_hex: &str, finalized_hex: &str) -> FinishedPlan {
        FinishedPlan {
            plan: plan(stem),
            intro: sha(intro_hex),
            finalized_at: sha(finalized_hex),
        }
    }

    fn snap(watched: &[&str], finished_at_startup: &[(&str, &str)]) -> StartupSnapshot {
        let mut s = StartupSnapshot {
            watched: watched.iter().map(|k| plan(k)).collect(),
            finished_at_startup: BTreeMap::new(),
        };
        for (k, sh) in finished_at_startup {
            s.finished_at_startup
                .entry(plan(k))
                .or_default()
                .insert(sha(sh));
        }
        s
    }

    #[test]
    fn watched_plan_newly_finished_emits_one_item() {
        let s = state_with(vec![finished("a", "1111", "2222")]);
        let snap = snap(&["a"], &[]);
        let out = detect_finished(&snap, &s);
        assert_eq!(out.len(), 1);
        assert!(matches!(
            &out[0],
            WaitItem::Finished { plan: p, finalized_at }
                if p == &plan("a") && finalized_at == &sha("2222")
        ));
    }

    #[test]
    fn watched_plan_finalize_already_known_emits_no_item() {
        let s = state_with(vec![finished("a", "1111", "2222")]);
        let snap = snap(&["a"], &[("a", "2222")]);
        assert!(detect_finished(&snap, &s).is_empty());
    }

    #[test]
    fn re_finalize_with_new_sha_emits_item() {
        // Plan a was finalized at 2222 at startup; it then got
        // re-introduced and re-finalized at 3333. The snapshot's
        // per-plan set tracks "what was known," so the new sha
        // emits a fresh notice.
        let s = state_with(vec![
            finished("a", "1111", "2222"),
            finished("a", "4444", "3333"),
        ]);
        let snap = snap(&["a"], &[("a", "2222")]);
        let out = detect_finished(&snap, &s);
        assert_eq!(out.len(), 1);
        assert!(matches!(
            &out[0],
            WaitItem::Finished { finalized_at, .. } if finalized_at == &sha("3333")
        ));
    }

    #[test]
    fn unwatched_plan_finished_emits_no_item() {
        let s = state_with(vec![finished("b", "1111", "2222")]);
        let snap = snap(&["a"], &[]);
        assert!(detect_finished(&snap, &s).is_empty());
    }

    #[test]
    fn multiple_watched_plans_finished_yields_plan_key_order() {
        let s = state_with(vec![
            finished("z", "1111", "2222"),
            finished("a", "3333", "4444"),
            finished("m", "5555", "6666"),
        ]);
        let snap = snap(&["a", "m", "z"], &[]);
        let out = detect_finished(&snap, &s);
        let plans: Vec<&str> = out
            .iter()
            .map(|i| match i {
                WaitItem::Finished { plan, .. } => plan.as_str(),
                _ => panic!("expected Finished"),
            })
            .collect();
        assert_eq!(plans, vec!["a", "m", "z"]);
    }

    #[test]
    fn snapshot_capture_watched_from_active_plans_when_no_filter() {
        let mut state = RepoState::default();
        state.plans.insert(plan("a"), PlanState::default());
        state.plans.insert(plan("b"), PlanState::default());
        let snap = StartupSnapshot::capture(&state, None);
        assert_eq!(snap.watched.len(), 2);
        assert!(snap.watched.contains(&plan("a")));
        assert!(snap.watched.contains(&plan("b")));
    }

    #[test]
    fn snapshot_capture_watched_is_singleton_with_filter() {
        let mut state = RepoState::default();
        state.plans.insert(plan("a"), PlanState::default());
        state.plans.insert(plan("b"), PlanState::default());
        let snap = StartupSnapshot::capture(&state, Some(&plan("a")));
        assert_eq!(snap.watched, std::iter::once(plan("a")).collect());
    }

    // ============ derive_status ad-hoc + blocks ============

    use crate::repo_state::AdHocEvent;

    struct MockReviews(Vec<(CommitSha, Vec<ReviewEntry>)>);
    impl PlanStateLookup for MockReviews {
        fn reviews_for(&self, sha: &CommitSha) -> Vec<ReviewEntry> {
            self.0
                .iter()
                .filter(|(s, _)| s == sha)
                .flat_map(|(_, entries)| entries.clone())
                .collect()
        }
        fn worktree_status(&self, _plan: &PlanKey) -> PlanWorktreeStatus {
            PlanWorktreeStatus::Clean
        }
    }

    /// `PlanStateLookup` mock that surfaces a configured list of
    /// blocks for each plan (and otherwise no reviews).
    struct MockBlocks(std::collections::BTreeMap<PlanKey, Vec<PlanBlock>>);
    impl PlanStateLookup for MockBlocks {
        fn reviews_for(&self, _sha: &CommitSha) -> Vec<ReviewEntry> {
            Vec::new()
        }
        fn worktree_status(&self, _plan: &PlanKey) -> PlanWorktreeStatus {
            PlanWorktreeStatus::Clean
        }
        fn blocks_for(&self, plan: &PlanKey) -> Vec<PlanBlock> {
            self.0.get(plan).cloned().unwrap_or_default()
        }
    }

    fn mkblock(creator: &str, name: &str, msg: &str) -> PlanBlock {
        PlanBlock {
            creator: label(creator),
            name: name.to_string(),
            message: msg.to_string(),
        }
    }

    fn plan_policy() -> WorkPolicy {
        WorkPolicy {
            plan_feedback: true,
            adhoc_feedback: false,
            commit_reviewers: vec![label("codex"), label("ruthless")],
            gate_reviewers: vec![],
        }
    }

    #[test]
    fn compute_gate_unaffected_by_blocks() {
        // compute_gate is per-commit pure; blocks live a layer up
        // in derive_status. compute_gate's behavior is unchanged.
        assert_eq!(compute_gate(&[], &[], &[]), CommitGateState::Approved);
        let reviews = [entry(crate::vocab::Verdict::Approve, "codex")];
        assert_eq!(
            compute_gate(&reviews, &[label("codex")], &[]),
            CommitGateState::Approved
        );
    }

    fn make_plan_with_one_reviewable(commit_sha: CommitSha) -> PlanState {
        use crate::repo_state::PlanTimelineEvent;
        let mut ps = PlanState::default();
        ps.commits.push(PlanTimelineEvent {
            sha: commit_sha.clone(),
            ts: 1,
            touched_plan: false,
            touched_code: true,
        });
        ps
    }

    #[test]
    fn derive_status_plan_with_pending_block_returns_blocked_gate() {
        // A plan with a reviewable commit + a pending block lands
        // in PlanWorkState with gate=Blocked and waiting_on=Blocked.
        let mut state = RepoState::default();
        state
            .plans
            .insert(plan("foo"), make_plan_with_one_reviewable(sha("aaaa")));
        let blocks = std::collections::BTreeMap::from([(
            plan("foo"),
            vec![mkblock("claude", "halt", "checking the design")],
        )]);
        let status = state.derive_status(&MockBlocks(blocks), &plan_policy());
        assert_eq!(status.plans.len(), 1);
        let ps = &status.plans[0];
        assert_eq!(ps.gate, CommitGateState::Blocked);
        match &ps.waiting_on {
            WaitingOn::Blocked { block } => {
                assert_eq!(block.creator.as_str(), "claude");
                assert_eq!(block.name, "halt");
                assert_eq!(block.message, "checking the design");
            }
            other => panic!("expected WaitingOn::Blocked; got: {other:?}"),
        }
        // sha is still Some — the plan has a reviewable commit.
        assert_eq!(
            ps.sha.as_ref().map(|s| s.as_str()),
            Some(sha("aaaa").as_str())
        );
    }

    #[test]
    fn derive_status_intro_only_plan_with_block_surfaces_with_none_sha() {
        // Pin 2: a plan with NO reviewable commit + open block
        // still surfaces in per-plan output (was invisible
        // pre-change). sha is None.
        let mut state = RepoState::default();
        state.plans.insert(plan("foo"), PlanState::default());
        let blocks = std::collections::BTreeMap::from([(
            plan("foo"),
            vec![mkblock("claude", "halt", "intro only")],
        )]);
        let status = state.derive_status(&MockBlocks(blocks), &plan_policy());
        assert_eq!(status.plans.len(), 1);
        let ps = &status.plans[0];
        assert_eq!(ps.gate, CommitGateState::Blocked);
        assert!(
            ps.sha.is_none(),
            "intro-only blocked plan: sha must be None"
        );
    }

    #[test]
    fn derive_status_plan_with_no_block_uses_review_gate() {
        // Empty blocks → review gate path (Unreviewed in this case
        // since no reviews were provided).
        let mut state = RepoState::default();
        state
            .plans
            .insert(plan("foo"), make_plan_with_one_reviewable(sha("aaaa")));
        let status = state.derive_status(&MockBlocks(Default::default()), &plan_policy());
        assert_eq!(status.plans.len(), 1);
        assert_eq!(status.plans[0].gate, CommitGateState::Unreviewed);
    }

    #[test]
    fn derive_status_picks_first_pending_block_when_multiple() {
        // Tie-breaker pinned in Phase 2: lex-first by (creator, name)
        // matches scan_blocks's sort. The first pending block surfaces
        // on the gate line.
        let mut state = RepoState::default();
        state
            .plans
            .insert(plan("foo"), make_plan_with_one_reviewable(sha("aaaa")));
        let blocks = std::collections::BTreeMap::from([(
            plan("foo"),
            vec![
                // Order them in the WRONG order so we can verify
                // derive_status sorts. Expected first: (codex, alpha).
                mkblock("codex", "zebra", "z"),
                mkblock("claude", "yak", "y"),
                mkblock("codex", "alpha", "a"),
            ],
        )]);
        let status = state.derive_status(&MockBlocks(blocks), &plan_policy());
        let block_creator = match &status.plans[0].waiting_on {
            WaitingOn::Blocked { block } => block.creator.as_str().to_string(),
            other => panic!("expected Blocked; got: {other:?}"),
        };
        let block_name = match &status.plans[0].waiting_on {
            WaitingOn::Blocked { block } => block.name.clone(),
            _ => unreachable!(),
        };
        // Lex by (creator, name): claude < codex, so claude/yak wins.
        assert_eq!(block_creator, "claude");
        assert_eq!(block_name, "yak");
    }

    #[test]
    fn work_for_blocked_plan_emits_no_master_or_reviewer_items() {
        // Phase 5 pin: blocked plans emit no work for any role.
        // Block-creator clears the block out-of-band.
        let mut state = RepoState::default();
        state
            .plans
            .insert(plan("foo"), make_plan_with_one_reviewable(sha("aaaa")));
        let blocks = std::collections::BTreeMap::from([(
            plan("foo"),
            vec![mkblock("claude", "halt", "wait")],
        )]);
        let status = state.derive_status(&MockBlocks(blocks), &plan_policy());
        // Master gets nothing.
        let master_items = status.work_for(&label("claude"), Role::Master);
        assert_eq!(master_items.len(), 0);
        // Reviewer gets nothing — even if they haven't reviewed yet.
        let reviewer_items = status.work_for(&label("codex"), Role::Reviewer);
        assert_eq!(reviewer_items.len(), 0);
    }

    fn adhoc_policy() -> WorkPolicy {
        WorkPolicy {
            plan_feedback: true,
            adhoc_feedback: true,
            commit_reviewers: Vec::new(),
            gate_reviewers: Vec::new(),
        }
    }

    #[test]
    fn only_latest_adhoc_commit_produces_work() {
        let mut state = RepoState::default();
        state.ad_hoc.push(AdHocEvent {
            sha: sha("1111"),
            ts: 1,
            touched_code: true,
        });
        state.ad_hoc.push(AdHocEvent {
            sha: sha("2222"),
            ts: 2,
            touched_code: true,
        });
        let reviews = MockReviews(vec![(
            sha("1111"),
            vec![ReviewEntry {
                author: label("codex"),
                verdict: crate::vocab::Verdict::RequestChanges,
            }],
        )]);
        let status = state.derive_status(&reviews, &adhoc_policy());
        assert_eq!(status.ad_hoc.len(), 1);
        assert_eq!(status.ad_hoc[0].sha, sha("2222"));
    }

    #[test]
    fn approve_on_latest_adhoc_clears_all_work() {
        let mut state = RepoState::default();
        state.ad_hoc.push(AdHocEvent {
            sha: sha("1111"),
            ts: 1,
            touched_code: true,
        });
        state.ad_hoc.push(AdHocEvent {
            sha: sha("2222"),
            ts: 2,
            touched_code: true,
        });
        let reviews = MockReviews(vec![
            (
                sha("1111"),
                vec![ReviewEntry {
                    author: label("codex"),
                    verdict: crate::vocab::Verdict::RequestChanges,
                }],
            ),
            (
                sha("2222"),
                vec![ReviewEntry {
                    author: label("codex"),
                    verdict: crate::vocab::Verdict::Approve,
                }],
            ),
        ]);
        let status = state.derive_status(&reviews, &adhoc_policy());
        let work = status.work_for(&label("master"), Role::Master);
        assert!(
            work.is_empty(),
            "approve on latest should produce no master work; got {work:?}"
        );
    }

    // ====== wfw-master-returns-on-dirty-plan-any-gate ======
    // Plan: dirty plan file pre-empts all gate-driven decisions
    // for the master, regardless of gate state.

    /// `PlanStateLookup` mock that combines reviews + a fixed
    /// `BodyDirty` worktree status for ALL plans. The reviews mock
    /// is empty by default but can be populated via `with_reviews`.
    struct MockDirty {
        reviews: Vec<(CommitSha, Vec<ReviewEntry>)>,
        blocks: std::collections::BTreeMap<PlanKey, Vec<PlanBlock>>,
    }

    impl MockDirty {
        fn new() -> Self {
            Self {
                reviews: Vec::new(),
                blocks: std::collections::BTreeMap::new(),
            }
        }
        fn with_reviews(mut self, reviews: Vec<(CommitSha, Vec<ReviewEntry>)>) -> Self {
            self.reviews = reviews;
            self
        }
        fn with_block(mut self, plan_key: PlanKey, blocks: Vec<PlanBlock>) -> Self {
            self.blocks.insert(plan_key, blocks);
            self
        }
    }

    impl PlanStateLookup for MockDirty {
        fn reviews_for(&self, sha: &CommitSha) -> Vec<ReviewEntry> {
            self.reviews
                .iter()
                .filter(|(s, _)| s == sha)
                .flat_map(|(_, e)| e.clone())
                .collect()
        }
        fn worktree_status(&self, _plan: &PlanKey) -> PlanWorktreeStatus {
            PlanWorktreeStatus::BodyDirty
        }
        fn blocks_for(&self, plan: &PlanKey) -> Vec<PlanBlock> {
            self.blocks.get(plan).cloned().unwrap_or_default()
        }
    }

    #[test]
    fn master_with_dirty_plan_and_unreviewed_gate_returns_commit() {
        // Pre-fix: Unreviewed + BodyDirty produced
        // ReviewerApprovalsMissing — master got NO work item; both
        // sides were waiting for each other. Post-fix: worktree-first
        // dispatch routes master to MasterToCommit.
        let mut state = RepoState::default();
        state
            .plans
            .insert(plan("foo"), make_plan_with_one_reviewable(sha("aaaa")));
        // No reviews → Unreviewed gate.
        let status = state.derive_status(&MockDirty::new(), &plan_policy());
        let work = status.work_for(&label("lloyd"), Role::Master);
        assert_eq!(work.len(), 1, "master must get exactly one item");
        match &work[0] {
            WaitItem::Master {
                next: MasterNext::Commit,
                reason: WaitingReason::CommitPlanRevision,
                ..
            } => {}
            other => panic!("expected Master {{ next: Commit, .. }}; got: {other:?}"),
        }
    }

    #[test]
    fn master_with_dirty_plan_and_changes_requested_returns_commit() {
        // The architectural mismatch this plan fixes: the previous
        // behavior routed master to Revise even though the worktree
        // already had a new revision in flight.
        let mut state = RepoState::default();
        state
            .plans
            .insert(plan("foo"), make_plan_with_one_reviewable(sha("aaaa")));
        let reviews = vec![(
            sha("aaaa"),
            vec![ReviewEntry {
                author: label("codex"),
                verdict: crate::vocab::Verdict::RequestChanges,
            }],
        )];
        let mock = MockDirty::new().with_reviews(reviews);
        let status = state.derive_status(&mock, &plan_policy());
        let work = status.work_for(&label("lloyd"), Role::Master);
        assert_eq!(work.len(), 1);
        match &work[0] {
            WaitItem::Master {
                next: MasterNext::Commit,
                ..
            } => {}
            WaitItem::Master {
                next: MasterNext::Revise,
                ..
            } => panic!("dirty plan must override Revise — that's the architectural mismatch"),
            other => panic!("expected Master Commit; got: {other:?}"),
        }
    }

    #[test]
    fn master_with_dirty_plan_and_approved_gate_returns_commit() {
        // Regression fence — Approved+Dirty already produced Commit
        // via the pre-fix per-arm check. Defends against re-introducing
        // the bug when the per-arm checks are removed.
        let mut state = RepoState::default();
        state
            .plans
            .insert(plan("foo"), make_plan_with_one_reviewable(sha("aaaa")));
        let reviews = vec![(
            sha("aaaa"),
            vec![
                ReviewEntry {
                    author: label("codex"),
                    verdict: crate::vocab::Verdict::Approve,
                },
                ReviewEntry {
                    author: label("ruthless"),
                    verdict: crate::vocab::Verdict::Approve,
                },
            ],
        )];
        let mock = MockDirty::new().with_reviews(reviews);
        let status = state.derive_status(&mock, &plan_policy());
        let work = status.work_for(&label("lloyd"), Role::Master);
        assert_eq!(work.len(), 1);
        assert!(matches!(
            work[0],
            WaitItem::Master {
                next: MasterNext::Commit,
                ..
            }
        ));
    }

    #[test]
    fn master_with_dirty_plan_and_blocked_plan_returns_blocked() {
        // Block precedence wins. The plan-level Blocked state is
        // resolved in derive_status BEFORE the worktree-first
        // dispatch, so a blocked plan with BodyDirty still surfaces
        // as Blocked, and work_for emits no Master Commit item.
        let mut state = RepoState::default();
        state
            .plans
            .insert(plan("foo"), make_plan_with_one_reviewable(sha("aaaa")));
        let mock =
            MockDirty::new().with_block(plan("foo"), vec![mkblock("claude", "halt", "hold")]);
        let status = state.derive_status(&mock, &plan_policy());
        assert_eq!(status.plans[0].gate, CommitGateState::Blocked);
        // Blocked plans emit no work for any role.
        let master_work = status.work_for(&label("lloyd"), Role::Master);
        let reviewer_work = status.work_for(&label("codex"), Role::Reviewer);
        assert!(
            master_work.is_empty(),
            "blocked plan must not emit master work; got {master_work:?}"
        );
        assert!(
            reviewer_work.is_empty(),
            "blocked plan must not emit reviewer work; got {reviewer_work:?}"
        );
    }

    #[test]
    fn reviewer_does_not_get_work_on_master_dirty_plan() {
        // Reviewer-side consequence (documented in plan body): a
        // dirty plan file routes the master to Commit but does NOT
        // wake reviewers. Their review effort on the committed
        // version would be at risk of being thrown away the moment
        // master commits the revision.
        let mut state = RepoState::default();
        state
            .plans
            .insert(plan("foo"), make_plan_with_one_reviewable(sha("aaaa")));
        // No reviews → Unreviewed gate. Pre-fix this would have
        // woken reviewers via ReviewerApprovalsMissing.
        let status = state.derive_status(&MockDirty::new(), &plan_policy());
        let codex_work = status.work_for(&label("codex"), Role::Reviewer);
        let ruthless_work = status.work_for(&label("ruthless"), Role::Reviewer);
        assert!(
            codex_work.is_empty(),
            "dirty plan must not wake reviewer codex; got {codex_work:?}"
        );
        assert!(
            ruthless_work.is_empty(),
            "dirty plan must not wake reviewer ruthless; got {ruthless_work:?}"
        );
    }
}
