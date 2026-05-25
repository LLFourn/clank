//! Agent-perspective wait surface for `clank wfw`.
//!
//! Two things:
//!
//! 1. `derive_work` — pure filter over `[PlanView]` for the
//!    calling agent's role + label. Emits `WaitItem::Master` and
//!    `WaitItem::Reviewer` only.
//! 2. `detect_finished` — pure comparison between a startup
//!    snapshot of "plans this wfw is watching" and the current
//!    `RepoState`. Emits `WaitItem::Finished` for every watched
//!    plan that newly transitioned into `finished_plans` since
//!    startup.
//!
//! Both produce `Vec<WaitItem>`; `wfw` concatenates them and
//! emits whatever's non-empty as one round's outcome. Empty means
//! "keep blocking."

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::ids::{AgentLabel, CommitSha, PlanKey};
use crate::plan_view::{PlanView, WaitingOn};
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
    /// Approved plan-only commit. Write the implementation under
    /// the `[<stem>]` commit prefix.
    Implement,
    /// Approved code-touching commit. Run `clank finish`.
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

fn compute_gate(reviews: &[ReviewEntry]) -> crate::vocab::CommitGateState {
    use crate::vocab::CommitGateState;
    let has_approve = reviews
        .iter()
        .any(|r| r.verdict == crate::vocab::Verdict::Approve);
    let has_changes = reviews.iter().any(|r| {
        r.verdict == crate::vocab::Verdict::RequestChanges
            || r.verdict == crate::vocab::Verdict::Unmarked
    });
    if has_changes {
        CommitGateState::ChangesRequested
    } else if has_approve {
        CommitGateState::Approved
    } else {
        CommitGateState::Unreviewed
    }
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
                    compute_gate(&entries)
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

                let worktree = reviews.worktree_status(key);
                let waiting_on = match gate {
                    CommitGateState::ChangesRequested => {
                        let requesters = entries
                            .iter()
                            .filter(|r| r.verdict == crate::vocab::Verdict::RequestChanges)
                            .map(|r| r.author.clone())
                            .collect();
                        let ambiguous = entries
                            .iter()
                            .filter(|r| r.verdict == crate::vocab::Verdict::Unmarked)
                            .map(|r| r.author.clone())
                            .collect();
                        WaitingOn::MasterToRevise {
                            requesters,
                            ambiguous,
                        }
                    }
                    CommitGateState::Approved => match worktree {
                        PlanWorktreeStatus::BodyDirty => WaitingOn::MasterToCommit,
                        _ => {
                            if touched_code {
                                WaitingOn::MasterToFinalize
                            } else {
                                WaitingOn::MasterToImplement
                            }
                        }
                    },
                    CommitGateState::Unreviewed => WaitingOn::FirstReview,
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
                let gate = compute_gate(&entries);
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
                (Role::Master, WaitingOn::MasterToImplement) => {
                    out.push(WaitItem::Master {
                        plan: ps.plan.clone(),
                        sha: ps.sha.clone(),
                        next: MasterNext::Implement,
                        reason: WaitingReason::ReadyToStartImplementation,
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

// ── Legacy derive_work (kept for now) ──────────────────

pub fn derive_work(views: &[PlanView], author: &AgentLabel, role: Role) -> Vec<WaitItem> {
    let mut out = Vec::new();
    for view in views {
        match (role, &view.waiting_on) {
            (Role::Master, WaitingOn::MasterToRevise { .. }) => {
                out.push(master(
                    view,
                    MasterNext::Revise,
                    WaitingReason::AddressCommitChanges,
                ));
            }
            (Role::Master, WaitingOn::MasterToCommit) => {
                out.push(master(
                    view,
                    MasterNext::Commit,
                    WaitingReason::CommitPlanRevision,
                ));
            }
            (Role::Master, WaitingOn::MasterToImplement) => {
                out.push(master(
                    view,
                    MasterNext::Implement,
                    WaitingReason::ReadyToStartImplementation,
                ));
            }
            (Role::Master, WaitingOn::MasterToFinalize) => {
                out.push(master(
                    view,
                    MasterNext::Finalize,
                    WaitingReason::ReadyToFinalize,
                ));
            }
            (Role::Reviewers, WaitingOn::FirstReview) => {
                out.push(reviewer_item(view, author));
            }
            (Role::Reviewers, WaitingOn::ReviewerApprovalsMissing { missing })
                if missing.iter().any(|a| a == author) =>
            {
                out.push(reviewer_item(view, author));
            }
            _ => {}
        }
    }
    out
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

fn master(view: &PlanView, next: MasterNext, reason: WaitingReason) -> WaitItem {
    WaitItem::Master {
        plan: view.plan.clone(),
        sha: view.latest_reviewable_sha.clone(),
        next,
        reason,
        gate: view.gate_state,
    }
}

fn reviewer_item(view: &PlanView, author: &AgentLabel) -> WaitItem {
    use crate::feedback_view::{filename_mode, filename_stem};
    let mode = filename_mode(&view.reviewable_shas);
    let stem = filename_stem(&view.latest_reviewable_sha, mode);
    let path = format!(
        ".clank/agents/{}/feedback/{}.md",
        author.as_str(),
        stem,
    );
    WaitItem::Reviewer {
        plan: view.plan.clone(),
        sha: view.latest_reviewable_sha.clone(),
        feedback_path: path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo_state::{FinishedPlan, NonEmptyVec, PlanState};
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

    fn view(plan_key: &str, sha_hex: &str, waiting: WaitingOn) -> PlanView {
        let latest = sha(sha_hex);
        PlanView {
            plan: plan(plan_key),
            latest_reviewable_sha: latest.clone(),
            reviewable_shas: vec![latest],
            gate_state: CommitGateState::Unreviewed,
            waiting_on: waiting,
            worktree_status: PlanWorktreeStatus::Clean,
            last_activity_ts: 0,
        }
    }

    // ============ derive_work ============

    #[test]
    fn master_picks_master_flavored_plans() {
        let views = vec![
            view("a", "aaaa", WaitingOn::MasterToFinalize),
            view("b", "bbbb", WaitingOn::FirstReview),
        ];
        let out = derive_work(&views, &label("master"), Role::Master);
        assert_eq!(out.len(), 1);
        assert!(matches!(
            out[0],
            WaitItem::Master {
                next: MasterNext::Finalize,
                reason: WaitingReason::ReadyToFinalize,
                ..
            }
        ));
    }

    #[test]
    fn master_to_implement_emits_implement_with_ready_to_start_implementation() {
        let views = vec![view("a", "aaaa", WaitingOn::MasterToImplement)];
        let out = derive_work(&views, &label("anybody"), Role::Master);
        assert_eq!(out.len(), 1);
        assert!(matches!(
            out[0],
            WaitItem::Master {
                next: MasterNext::Implement,
                reason: WaitingReason::ReadyToStartImplementation,
                ..
            }
        ));
    }

    #[test]
    fn reviewer_picks_first_review_unconditionally() {
        let views = vec![view("a", "aaaa", WaitingOn::FirstReview)];
        let out = derive_work(&views, &label("anyone"), Role::Reviewers);
        assert_eq!(out.len(), 1);
        match &out[0] {
            WaitItem::Reviewer { feedback_path, .. } => {
                assert!(
                    feedback_path.starts_with(".clank/agents/anyone/feedback/"),
                    "got {feedback_path}"
                );
                assert!(feedback_path.ends_with(".md"));
            }
            other => panic!("expected Reviewer, got {other:?}"),
        }
    }

    #[test]
    fn reviewer_path_uses_short_sha_for_unique_prefixes() {
        // Build a view with a real 40-char latest sha + a
        // singleton reviewable scope. Short mode applies.
        let plan_key = plan("p");
        let latest = CommitSha::parse("abcdef0111111111111111111111111111111111").unwrap();
        let v = PlanView {
            plan: plan_key,
            latest_reviewable_sha: latest.clone(),
            reviewable_shas: vec![latest],
            gate_state: CommitGateState::Unreviewed,
            waiting_on: WaitingOn::FirstReview,
            worktree_status: PlanWorktreeStatus::Clean,
            last_activity_ts: 0,
        };
        let out = derive_work(std::slice::from_ref(&v), &label("alice"), Role::Reviewers);
        match &out[0] {
            WaitItem::Reviewer { feedback_path, .. } => {
                assert_eq!(feedback_path, ".clank/agents/alice/feedback/abcdef0.md");
            }
            other => panic!("expected Reviewer, got {other:?}"),
        }
    }

    #[test]
    fn reviewer_path_uses_full_sha_for_colliding_short_prefix() {
        let plan_key = plan("p");
        let a = CommitSha::parse("abcdef0111111111111111111111111111111111").unwrap();
        let b = CommitSha::parse("abcdef0222222222222222222222222222222222").unwrap();
        let v = PlanView {
            plan: plan_key,
            latest_reviewable_sha: b.clone(),
            reviewable_shas: vec![a, b.clone()],
            gate_state: CommitGateState::Unreviewed,
            waiting_on: WaitingOn::FirstReview,
            worktree_status: PlanWorktreeStatus::Clean,
            last_activity_ts: 0,
        };
        let out = derive_work(std::slice::from_ref(&v), &label("alice"), Role::Reviewers);
        match &out[0] {
            WaitItem::Reviewer { feedback_path, .. } => {
                assert_eq!(
                    feedback_path,
                    &format!(".clank/agents/alice/feedback/{}.md", b.as_str())
                );
            }
            other => panic!("expected Reviewer, got {other:?}"),
        }
    }

    #[test]
    fn reviewer_picks_missing_only_when_author_in_set() {
        let missing = NonEmptyVec::new(vec![label("alice"), label("bob")]).unwrap();
        let views = vec![view(
            "a",
            "aaaa",
            WaitingOn::ReviewerApprovalsMissing { missing },
        )];

        let bob = derive_work(&views, &label("bob"), Role::Reviewers);
        assert_eq!(bob.len(), 1);

        let carol = derive_work(&views, &label("carol"), Role::Reviewers);
        assert!(carol.is_empty());
    }

    #[test]
    fn master_skips_reviewer_states() {
        let views = vec![view("a", "aaaa", WaitingOn::FirstReview)];
        let out = derive_work(&views, &label("master"), Role::Master);
        assert!(out.is_empty());
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
