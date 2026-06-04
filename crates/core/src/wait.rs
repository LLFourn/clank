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
use crate::plan_view::WaitingOn;
use crate::repo_state::RepoState;
use crate::vocab::{Role, WaitingReason};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MasterNext {
    /// REQUEST_CHANGES on the latest reviewable. Address + recommit.
    Revise,
    /// Approved gate but the plan file has uncommitted edits. Commit
    /// the next revision (or stash).
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

// ── ReviewLookup trait + derive_status ──────────────────

pub trait ReviewLookup {
    fn reviews_for(&self, sha: &CommitSha) -> Vec<ReviewEntry>;
    fn worktree_status(&self, plan: &PlanKey) -> crate::vocab::PlanWorktreeStatus;
}

#[derive(Clone)]
pub struct ReviewEntry {
    pub author: AgentLabel,
    pub verdict: crate::vocab::Verdict,
}

pub struct WorkPolicy {
    pub plan_feedback: bool,
    pub adhoc_feedback: bool,
    /// Labels of agents whose APPROVE/FINISHED is required for the
    /// gate to advance. Sourced from the CLI's `agent_store` scan
    /// of `.clank/agents/*/config.json` with `role: Reviewers`.
    /// Empty = master-only repo (every commit auto-approves).
    pub expected_reviewers: Vec<AgentLabel>,
}

#[derive(Debug, Clone)]
pub struct WorkStatus {
    pub plans: Vec<PlanWorkState>,
    pub ad_hoc: Vec<AdHocWorkState>,
}

#[derive(Debug, Clone)]
pub struct PlanWorkState {
    pub plan: PlanKey,
    pub sha: CommitSha,
    pub gate: crate::vocab::CommitGateState,
    pub waiting_on: WaitingOn,
    pub touched_code: bool,
}

#[derive(Debug, Clone)]
pub struct AdHocWorkState {
    pub sha: CommitSha,
    pub gate: crate::vocab::CommitGateState,
}

pub fn compute_gate(
    reviews: &[ReviewEntry],
    expected_reviewers: &[AgentLabel],
) -> crate::vocab::CommitGateState {
    use crate::vocab::{CommitGateState, Verdict};

    // Zero-reviewer mode: master-only repo. Every commit auto-approves.
    // This MUST be checked before the "every reviewer Finished" rule,
    // which would otherwise be vacuously true on an empty set.
    if expected_reviewers.is_empty() {
        return CommitGateState::Approved;
    }

    // Filter reviews to expected reviewers. Stale entries from removed
    // reviewers (or other authors whose `.clank/agents/<label>/feedback/`
    // dir still exists) MUST NOT gate decisions. "Removed reviewers don't
    // gate" is a uniform principle, not a special case.
    let expected: std::collections::HashSet<&AgentLabel> = expected_reviewers.iter().collect();
    let reviews: Vec<&ReviewEntry> = reviews
        .iter()
        .filter(|r| expected.contains(&r.author))
        .collect();

    if reviews
        .iter()
        .any(|r| r.verdict == Verdict::RequestChanges || r.verdict == Verdict::Unmarked)
    {
        return CommitGateState::ChangesRequested;
    }

    // For "every expected reviewer posted X" checks, build a label→verdict map.
    let mut by_label: std::collections::HashMap<&AgentLabel, &Verdict> =
        std::collections::HashMap::new();
    for r in &reviews {
        by_label.insert(&r.author, &r.verdict);
    }

    let any_missing = expected_reviewers
        .iter()
        .any(|label| !by_label.contains_key(label));
    if any_missing {
        return CommitGateState::Unreviewed;
    }

    let all_finished = expected_reviewers
        .iter()
        .all(|label| matches!(by_label.get(label), Some(Verdict::Finished)));
    if all_finished {
        return CommitGateState::Finished;
    }

    // All expected reviewers signed off (Approve or Finished), but not all Finished.
    CommitGateState::Approved
}

impl RepoState {
    pub fn derive_status(&self, reviews: &impl ReviewLookup, policy: &WorkPolicy) -> WorkStatus {
        use crate::vocab::{CommitGateState, PlanWorktreeStatus};

        let mut plans = Vec::new();
        {
            for (key, ps) in &self.plans {
                let reviewable = ps.reviewable_shas();
                if reviewable.is_empty() {
                    continue;
                }
                let latest_sha = reviewable.last().unwrap().clone();
                let latest_event = ps.commits.iter().rev().find(|e| e.sha == latest_sha);
                let touched_code = latest_event.map_or(false, |e| e.touched_code);

                let entries = reviews.reviews_for(&latest_sha);
                let gate = if policy.plan_feedback {
                    compute_gate(&entries, &policy.expected_reviewers)
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
                // principle as compute_gate's filter.
                let expected: std::collections::HashSet<&AgentLabel> =
                    policy.expected_reviewers.iter().collect();
                let filtered_entries: Vec<&ReviewEntry> = entries
                    .iter()
                    .filter(|r| expected.contains(&r.author))
                    .collect();

                let worktree = reviews.worktree_status(key);
                let waiting_on = match gate {
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
                    CommitGateState::Finished => match worktree {
                        PlanWorktreeStatus::BodyDirty => WaitingOn::MasterToCommit,
                        _ => WaitingOn::MasterToFinalize,
                    },
                    CommitGateState::Approved => match worktree {
                        PlanWorktreeStatus::BodyDirty => WaitingOn::MasterToCommit,
                        _ => WaitingOn::MasterToContinue,
                    },
                    CommitGateState::Unreviewed => {
                        // Compute the set of expected reviewers that
                        // haven't posted Approve or Finished. Stale
                        // RequestChanges from removed authors don't
                        // count because the filter dropped them.
                        let approved_by: std::collections::HashSet<&AgentLabel> = filtered_entries
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
                            .expected_reviewers
                            .iter()
                            .filter(|label| !approved_by.contains(label))
                            .cloned()
                            .collect();
                        match crate::repo_state::NonEmptyVec::new(missing) {
                            Ok(missing) => WaitingOn::ReviewerApprovalsMissing { missing },
                            Err(_) => WaitingOn::FirstReview, // unreachable under new semantics
                        }
                    }
                };

                plans.push(PlanWorkState {
                    plan: key.clone(),
                    sha: latest_sha,
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
                let gate = compute_gate(&entries, &policy.expected_reviewers);
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
            match (role, &ps.waiting_on) {
                (Role::Master, WaitingOn::MasterToRevise { .. }) => {
                    out.push(WaitItem::Master {
                        plan: ps.plan.clone(),
                        sha: ps.sha.clone(),
                        next: MasterNext::Revise,
                        reason: WaitingReason::AddressCommitChanges,
                        gate: ps.gate,
                    });
                }
                (Role::Master, WaitingOn::MasterToCommit) => {
                    out.push(WaitItem::Master {
                        plan: ps.plan.clone(),
                        sha: ps.sha.clone(),
                        next: MasterNext::Commit,
                        reason: WaitingReason::CommitPlanRevision,
                        gate: ps.gate,
                    });
                }
                (Role::Master, WaitingOn::MasterToContinue) => {
                    out.push(WaitItem::Master {
                        plan: ps.plan.clone(),
                        sha: ps.sha.clone(),
                        next: MasterNext::Continue,
                        reason: WaitingReason::GateApproved,
                        gate: ps.gate,
                    });
                }
                (Role::Master, WaitingOn::MasterToFinalize) => {
                    out.push(WaitItem::Master {
                        plan: ps.plan.clone(),
                        sha: ps.sha.clone(),
                        next: MasterNext::Finalize,
                        reason: WaitingReason::ReadyToFinalize,
                        gate: ps.gate,
                    });
                }
                (Role::Reviewers, WaitingOn::FirstReview) => {
                    out.push(WaitItem::Reviewer {
                        plan: ps.plan.clone(),
                        sha: ps.sha.clone(),
                        feedback_path: format!(
                            ".clank/agents/{}/feedback/{}.md",
                            author.as_str(),
                            ps.sha.as_str()
                        ),
                    });
                }
                (Role::Reviewers, WaitingOn::ReviewerApprovalsMissing { missing })
                    if missing.as_slice().iter().any(|l| l == author) =>
                {
                    // Only emit a review item for this reviewer if THEY
                    // are in the missing set. Reviewers who have already
                    // posted APPROVE/FINISHED don't get redundant wakes.
                    out.push(WaitItem::Reviewer {
                        plan: ps.plan.clone(),
                        sha: ps.sha.clone(),
                        feedback_path: format!(
                            ".clank/agents/{}/feedback/{}.md",
                            author.as_str(),
                            ps.sha.as_str()
                        ),
                    });
                }
                _ => {}
            }
        }
        for ah in &self.ad_hoc {
            match (role, ah.gate) {
                (Role::Reviewers, crate::vocab::CommitGateState::Unreviewed) => {
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
        assert_eq!(compute_gate(&[], &[]), CommitGateState::Approved);
    }

    #[test]
    fn compute_gate_zero_reviewers_ignores_stale_request_changes() {
        // Removed reviewer's stale RC must not gate.
        let reviews = [entry(crate::vocab::Verdict::RequestChanges, "alice")];
        assert_eq!(compute_gate(&reviews, &[]), CommitGateState::Approved);
    }

    #[test]
    fn compute_gate_single_expected_approve_is_approved() {
        let reviews = [entry(crate::vocab::Verdict::Approve, "codex")];
        assert_eq!(
            compute_gate(&reviews, &[label("codex")]),
            CommitGateState::Approved
        );
    }

    #[test]
    fn compute_gate_single_expected_request_changes() {
        let reviews = [entry(crate::vocab::Verdict::RequestChanges, "codex")];
        assert_eq!(
            compute_gate(&reviews, &[label("codex")]),
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
            compute_gate(&reviews, &[label("codex"), label("ruthless")]),
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
            compute_gate(&reviews, &[label("codex"), label("ruthless")]),
            CommitGateState::ChangesRequested
        );
    }

    #[test]
    fn compute_gate_two_expected_one_missing_is_unreviewed() {
        let reviews = [entry(crate::vocab::Verdict::Approve, "codex")];
        assert_eq!(
            compute_gate(&reviews, &[label("codex"), label("ruthless")]),
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
            compute_gate(&reviews, &[label("codex"), label("ruthless")]),
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
            compute_gate(&reviews, &[label("codex"), label("ruthless")]),
            CommitGateState::Approved
        );
    }

    #[test]
    fn compute_gate_unmarked_treated_as_changes() {
        let reviews = [entry(crate::vocab::Verdict::Unmarked, "codex")];
        assert_eq!(
            compute_gate(&reviews, &[label("codex")]),
            CommitGateState::ChangesRequested
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
            compute_gate(&reviews, &[label("codex"), label("ruthless")]),
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
            compute_gate(&reviews, &[label("codex"), label("ruthless")]),
            CommitGateState::Unreviewed
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

    // ============ derive_status ad-hoc ============

    use crate::repo_state::AdHocEvent;

    struct MockReviews(Vec<(CommitSha, Vec<ReviewEntry>)>);
    impl ReviewLookup for MockReviews {
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

    fn adhoc_policy() -> WorkPolicy {
        WorkPolicy {
            plan_feedback: true,
            adhoc_feedback: true,
            expected_reviewers: Vec::new(),
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
}
