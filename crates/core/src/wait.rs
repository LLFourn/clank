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
use crate::repo_state::{TitlePrefix, parse_title_prefix};
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

/// What master should do next on a PR review (clank-pr-review-mode).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrMasterNext {
    /// Round 0: the review isn't open yet. Master drafts the initial
    /// top-level comments, then `clank pr-review propose` opens
    /// round 1 and summons reviewers. The reviewable artifact (the
    /// pending-review draft) is master's output, so master always
    /// acts first — unlike a plan, whose commit exists before review.
    Draft,
    /// A reviewer requested changes this round: integrate the
    /// feedback into the pending review, delete the reply, bump the
    /// round.
    Integrate,
    /// Both tiers FINISHED: submit the pending review.
    Submit,
    /// Commit tier approved-not-finished (no milestone). Keep
    /// refining / nudge reviewers toward FINISHED.
    Continue,
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
    /// Master's HEAD commit's `[..]` tag doesn't match the plan files
    /// its diff touched (`commit-tag-fixup-is-first-class-state`).
    /// Master must amend the message before any review motion
    /// continues. HEAD-only, adopted-gated. The three independent
    /// violation kinds are reported together so one amend fixes them
    /// all (`adhoc-commits-and-plan-tag-validation` was the original
    /// unknown-tag-only check).
    FixCommitTag {
        sha: CommitSha,
        violation: HeadTagViolation,
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
    /// A reviewer should review PR #`pr` at `round`
    /// (clank-pr-review-mode). Routed to a reviewer in the active
    /// tier who hasn't posted a current-round verdict.
    PrReviewer {
        pr: u32,
        round: u64,
    },
    /// Master's turn on PR #`pr` at `round`: integrate, submit, or
    /// keep refining (clank-pr-review-mode).
    PrMaster {
        pr: u32,
        round: u64,
        next: PrMasterNext,
    },
}

/// All facts about HEAD the invariant check needs, sans-io. The CLI
/// builds this from live git (`head_tag_violation` is pure over it);
/// tests construct it directly. `None` (no HEAD) means a fresh repo —
/// no commit to police.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadCommit {
    pub sha: CommitSha,
    pub subject: String,
    /// `T` — plans whose `.clank/plans/<x>.md` HEAD's diff touched
    /// (intro/revise/finish/delete; cross-stem renames count as a
    /// touch of both stems). The objective ground truth for which
    /// plan(s) HEAD acts on.
    pub touched: BTreeSet<PlanKey>,
    /// Repo adoption (`RepoState::adopted`). Off → clank is a guest
    /// on an un-committed `.clank`; never police commit conventions.
    pub adopted: bool,
}

/// A structured HEAD commit-tag invariant violation. The three kinds
/// are independent and reported together so one amend fixes them all.
/// At least one field is non-empty whenever a `HeadTagViolation` is
/// produced. (`commit-tag-fixup-is-first-class-state`.)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeadTagViolation {
    /// `G ⊄ active∪introduced` — tag names that resolve to no real
    /// plan (the original unknown-tag check).
    pub unknown: Vec<String>,
    /// `T ⊄ G` — plans HEAD's diff touched that the tag fails to
    /// name. Must add these to the tag.
    pub untagged_touched: Vec<PlanKey>,
    /// `G ⊋ T` on a plan-touching commit — named plans HEAD did NOT
    /// touch. A planning commit must tag EXACTLY its touched plans;
    /// these extras must be dropped (genuine code for them goes in
    /// its own commit). Empty when `T = ∅`.
    pub extra_named: Vec<PlanKey>,
}

/// HEAD-only commit-tag invariant check
/// (`commit-tag-fixup-is-first-class-state`). Let `G` be the plans the
/// `[..]` tag names and `T = head.touched`:
/// - `T ≠ ∅` (HEAD touched plan file[s]): require `G == T` exactly —
///   tag the touched plans, no extras. Splits the legit
///   `[foo] finish`/`delete` carve-out out for free (the rename/delete
///   IS a touch of `foo`, so `T={foo}=G`).
/// - `T = ∅` (pure code / ad-hoc): every name in `G` must resolve to a
///   real plan (`active ∪ introduced`); untagged is fine (ad-hoc).
///   `G == T` does NOT apply (that would ban legit implementation
///   tags).
///
/// Always: every name in `G` must resolve to a real plan.
///
/// Returns `None` (no violation) when: the repo isn't `adopted`; HEAD
/// is untagged with `T = ∅`; or `G` and `T` satisfy the rule above.
/// HEAD-only by design — history is tolerated, mistakes caught when
/// made; host conventions (`[app]`/`[ci]`) are ancestors, never HEAD.
///
/// `known_plans` is `active ∪ plans HEAD introduced` — a `[foo] intro`
/// must accept its own brand-new tag.
pub fn head_tag_violation(
    head: &HeadCommit,
    known_plans: &BTreeSet<PlanKey>,
) -> Option<HeadTagViolation> {
    if !head.adopted {
        return None;
    }
    // G = the plan set the tag names. An unparsable name (e.g. one
    // with illegal chars) can't be a real plan → unknown.
    let mut unknown: Vec<String> = Vec::new();
    let mut named: BTreeSet<PlanKey> = BTreeSet::new();
    if let Some(TitlePrefix::Plans(names)) = parse_title_prefix(&head.subject) {
        for n in names {
            match PlanKey::parse(&n) {
                Ok(k) if known_plans.contains(&k) => {
                    named.insert(k);
                }
                _ => unknown.push(n),
            }
        }
    }

    // `T ⊆ G`: every touched plan must be named.
    let untagged_touched: Vec<PlanKey> = head
        .touched
        .iter()
        .filter(|t| !named.contains(t))
        .cloned()
        .collect();

    // `G ⊆ T` only when the commit touches a plan file (strict
    // `G == T`). On a `T = ∅` (pure-code) commit, named active plans
    // are legit implementation attribution — not extras.
    let extra_named: Vec<PlanKey> = if head.touched.is_empty() {
        Vec::new()
    } else {
        named
            .iter()
            .filter(|g| !head.touched.contains(g))
            .cloned()
            .collect()
    };

    if unknown.is_empty() && untagged_touched.is_empty() && extra_named.is_empty() {
        None
    } else {
        Some(HeadTagViolation {
            unknown,
            untagged_touched,
            extra_named,
        })
    }
}

/// Snapshot taken once at `wfw` startup. `detect_finished` compares
/// the current state against this to decide which watched plans
/// newly transitioned to finished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupSnapshot {
    /// Plan keys this wfw is responsible for: the active plan keys
    /// at startup. (All team members watch every plan — there is no
    /// per-agent plan filter.)
    pub watched: BTreeSet<PlanKey>,
    /// Finalize SHAs already present at startup, keyed by plan.
    /// A watched plan is "newly finished" iff its current
    /// `FinishedPlan` entry has a `finalized_at` NOT in this set.
    /// The set-per-plan stays correct across re-intro /
    /// re-finalize cycles.
    pub finished_at_startup: BTreeMap<PlanKey, BTreeSet<CommitSha>>,
}

impl StartupSnapshot {
    /// Build from the initial fold. The watched set is every active
    /// plan at startup (all team members watch every plan).
    pub fn capture(state: &RepoState) -> Self {
        let watched: BTreeSet<PlanKey> = state.plans.keys().cloned().collect();
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
    /// Active PR reviews projected from `.clank/pr-reviews/`
    /// (clank-pr-review-mode). Default empty so non-PR callers and
    /// test mocks need no awareness; the FS adapter overrides it.
    fn pr_reviews(&self) -> Vec<PrReviewInput> {
        Vec::new()
    }
}

/// One active PR review's raw inputs for the gate: its number, the
/// current round, and the verdicts posted FOR that round (already
/// filtered to `reviewed_round == round` and mapped to
/// `ReviewEntry`). `derive_status` runs `compute_gate` over these.
#[derive(Debug, Clone)]
pub struct PrReviewInput {
    pub pr: u32,
    /// `owner/name` slug, for building the PR's GitHub URL.
    pub repo: String,
    pub round: u64,
    pub current_verdicts: Vec<ReviewEntry>,
}

/// Per-PR-review gate + routing state, the PR analogue of
/// `PlanWorkState`.
#[derive(Debug, Clone)]
pub struct PrReviewWorkState {
    pub pr: u32,
    /// `owner/name` slug, for building the PR's GitHub URL.
    pub repo: String,
    pub round: u64,
    pub gate: crate::vocab::CommitGateState,
    /// Reviewers in the currently-active tier who still owe a
    /// current-round verdict (the ones to wake). Empty when it's
    /// master's turn.
    pub missing_reviewers: Vec<AgentLabel>,
}

#[derive(Debug, Clone)]
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
    /// Active GitHub PR reviews (clank-pr-review-mode), projected
    /// from `.clank/pr-reviews/` via `PlanStateLookup::pr_reviews`.
    pub pr_reviews: Vec<PrReviewWorkState>,
    /// `Some` iff HEAD's `[..]` tag violates the commit-tag invariant
    /// (`commit-tag-fixup-is-first-class-state`). This is the SINGLE
    /// derived source of the master's "fix the commit tag"
    /// correction: `work_for` routes master to it (dominating review
    /// motion) and withholds reviewer wakes while it's set. Affected
    /// active plan rows also carry `WaitingOn::MasterToFixCommitTag`
    /// (same computation) so status/TUI render the correction. Empty
    /// when no HEAD facts were supplied or the tag is valid.
    pub head_correction: Option<HeadCorrection>,
}

/// A HEAD commit-tag violation paired with the SHA to amend, ready to
/// route to the master as a `WaitItem::FixCommitTag`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadCorrection {
    pub sha: CommitSha,
    pub violation: HeadTagViolation,
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
/// Gate-tier activation is MILESTONE-conditional
/// (`gate-reviewers-only-plan-change-and-finish`): once the commit
/// tier is unanimous, the gate tier is consulted only when the
/// latest reviewable commit is a milestone — `latest_touched_plan`
/// (plan sign-off) OR commit-tier FINISHED. Routine commits bypass
/// the gate tier → `Approved`.
///
/// Empty-set / devolve rule: with empty `commit_reviewers`, "all
/// commit positive/finished" is vacuously true. The FINISHED
/// milestone is guarded (no commit reviewer to signal it), so a
/// master+gate-only repo wakes gate-reviewers on plan-doc commits
/// only — not every commit.
pub fn compute_gate(
    reviews: &[ReviewEntry],
    commit_reviewers: &[AgentLabel],
    gate_reviewers: &[AgentLabel],
    latest_touched_plan: bool,
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

    // Build label → verdict map for the per-tier coverage checks.
    let by_label: std::collections::HashMap<&AgentLabel, &Verdict> =
        reviews.iter().map(|r| (&r.author, &r.verdict)).collect();

    // Commit tier first. A non-positive verdict (Request-Changes OR
    // Unmarked) wakes the master to revise ONLY once EVERY commit reviewer
    // has verdicted (wait-for-all). While any is still pending, the gate
    // stays in the reviewers-wake / master-sleeps posture (`Unreviewed`)
    // and the non-positive verdict is HELD — acting on the first
    // Request-Changes before the others weigh in is the bug this fixes.
    let commit = tier_coverage(commit_reviewers, &by_label);
    if !commit.all_positive() {
        return if commit.all_submitted() {
            CommitGateState::ChangesRequested
        } else {
            CommitGateState::Unreviewed
        };
    }

    let finished =
        |label: &AgentLabel| -> bool { matches!(by_label.get(label), Some(Verdict::Finished)) };

    // The gate tier is consulted ONLY at a MILESTONE on the latest
    // reviewable commit — never on routine WIP commits. A milestone
    // is either:
    //   1. the commit changed the plan document (`latest_touched_plan`)
    //      and the commit tier approved it — plan sign-off, or
    //   2. the commit tier marked it FINISHED — the final gate.
    // On any other commit (pure code, merely approved) the gate tier
    // is bypassed and the gate passes straight to `Approved` so master
    // keeps moving. This makes "gate reviewer woken on an intermediate
    // code commit" unrepresentable by construction.
    //
    // Devolve guard: with an EMPTY commit tier, "all finished" is
    // vacuously true on the empty set — which would make every commit
    // a FINISHED milestone and reanimate the regression for master+gate
    // setups. With no commit reviewer to actually signal FINISHED, the
    // milestone reduces to `latest_touched_plan` only.
    let commit_finished = !commit_reviewers.is_empty() && commit_reviewers.iter().all(&finished);
    let is_milestone = latest_touched_plan || commit_finished;
    if !is_milestone {
        return CommitGateState::Approved;
    }

    // Milestone: the gate tier now contributes — same wait-for-all rule.
    // A non-positive gate verdict wakes the master only once every gate
    // reviewer has verdicted; while any is pending the master sleeps and
    // the gate reviewers wake (`ApprovedPendingGate`). An empty gate tier
    // is vacuously positive (no gate review configured).
    let gate = tier_coverage(gate_reviewers, &by_label);
    if !gate.all_positive() {
        return if gate.all_submitted() {
            CommitGateState::ChangesRequested
        } else {
            CommitGateState::ApprovedPendingGate
        };
    }

    // Both tiers positive at a milestone. Decide Finished vs Approved
    // by whether every reviewer is at Finished verdict.
    let all_finished =
        commit_reviewers.iter().all(&finished) && gate_reviewers.iter().all(&finished);
    if all_finished {
        CommitGateState::Finished
    } else {
        CommitGateState::Approved
    }
}

/// Review coverage for ONE reviewer tier on a commit — the single, named
/// answer to "have all expected reviewers of this tier verdicted, and are
/// they all positive?". Consumed by [`compute_gate`] and [`missing_for_gate`]
/// so the "all-done" question is computed once, never re-derived per gate
/// branch (the centralization that makes "acted on partial reviews" hard to
/// write).
struct TierCoverage {
    /// Expected reviewers of the tier with NO submitted verdict yet.
    pending: Vec<AgentLabel>,
    /// Submitted reviewers who are not positive — Request-Changes OR
    /// Unmarked (the two verdicts that route to `ChangesRequested`).
    non_positive: Vec<AgentLabel>,
}

impl TierCoverage {
    /// Every expected reviewer of the tier has submitted a verdict.
    fn all_submitted(&self) -> bool {
        self.pending.is_empty()
    }
    /// Every expected reviewer has submitted AND is positive.
    fn all_positive(&self) -> bool {
        self.pending.is_empty() && self.non_positive.is_empty()
    }
}

/// Partition a reviewer tier by verdict coverage. `by_label` maps each
/// SUBMITTED expected reviewer to its verdict; a tier label absent from it
/// is pending. Reviewers not in `tier` are ignored (stale/foreign feedback
/// never gates).
fn tier_coverage(
    tier: &[AgentLabel],
    by_label: &std::collections::HashMap<&AgentLabel, &crate::vocab::Verdict>,
) -> TierCoverage {
    use crate::vocab::Verdict;
    let mut pending = Vec::new();
    let mut non_positive = Vec::new();
    for label in tier {
        match by_label.get(label) {
            None => pending.push(label.clone()),
            Some(Verdict::Approve | Verdict::Finished) => {}
            Some(Verdict::RequestChanges | Verdict::Unmarked) => non_positive.push(label.clone()),
        }
    }
    TierCoverage {
        pending,
        non_positive,
    }
}

impl RepoState {
    /// Derive the role-flavored wait surface. `head` carries the live
    /// HEAD facts the commit-tag invariant needs
    /// (`commit-tag-fixup-is-first-class-state`); pass `None` on a
    /// fresh repo or where HEAD facts aren't available (the
    /// correction simply won't be derived). The violation is computed
    /// ONCE here and is the single source for both `head_correction`
    /// (master routing) and the `MasterToFixCommitTag` marking on
    /// affected plan rows (status/TUI display).
    pub fn derive_status(
        &self,
        reviews: &impl PlanStateLookup,
        policy: &WorkPolicy,
        head: Option<&HeadCommit>,
    ) -> WorkStatus {
        use crate::vocab::{CommitGateState, PlanWorktreeStatus};

        // Commit-tag invariant: compute the violation once. `known`
        // is active ∪ plans HEAD introduced so a `[foo] intro` accepts
        // its own brand-new tag (mirrors `apply_commit`'s known set).
        let head_correction = head.and_then(|h| {
            let mut known: BTreeSet<PlanKey> = self.plans.keys().cloned().collect();
            known.extend(h.touched.iter().cloned());
            head_tag_violation(h, &known).map(|violation| HeadCorrection {
                sha: h.sha.clone(),
                violation,
            })
        });
        // Active plans the violation implicates: those HEAD touched or
        // named with a real plan. Their rows get `MasterToFixCommitTag`
        // (below `Blocked`, above review/master-action states) so the
        // correction renders uniformly.
        let correction_plans: BTreeSet<PlanKey> = match (&head_correction, head) {
            (Some(c), Some(h)) => {
                let mut s: BTreeSet<PlanKey> = h.touched.iter().cloned().collect();
                s.extend(c.violation.untagged_touched.iter().cloned());
                s.extend(c.violation.extra_named.iter().cloned());
                s.retain(|k| self.plans.contains_key(k));
                s
            }
            _ => BTreeSet::new(),
        };

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
                let latest_touched_plan = latest_event.map_or(false, |e| e.touched_plan);

                let entries = reviews.reviews_for(&latest_sha);
                // SINGLE gate-state decision: always `compute_gate`. When
                // plan review is disabled, route through it with EMPTY tiers
                // (→ `Approved`) so the master isn't blocked — rather than a
                // duplicate inline verdict check that would re-implement (and
                // drift from) the gate logic.
                let (commit_tier, gate_tier): (&[AgentLabel], &[AgentLabel]) =
                    if policy.plan_feedback {
                        (&policy.commit_reviewers, &policy.gate_reviewers)
                    } else {
                        (&[], &[])
                    };
                let gate = compute_gate(&entries, commit_tier, gate_tier, latest_touched_plan);

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
                            // All commit-reviewers signed off; some gate-
                            // reviewer hasn't voted yet. Master sleeps; the
                            // PENDING gate-reviewers wake. Single-source via
                            // `missing_for_gate` (→ `tier_coverage`) — a
                            // gate-reviewer who already filed Request-Changes
                            // is HELD, not re-summoned.
                            let missing = crate::repo_state::NonEmptyVec::new(missing_for_gate(
                                gate, &entries, policy,
                            ))
                            .expect(
                                "ApprovedPendingGate state implies a non-empty pending gate-reviewer set",
                            );
                            WaitingOn::GateReviewersMissing { missing }
                        }
                        CommitGateState::Unreviewed => {
                            // Some commit-reviewer hasn't verdicted yet. The
                            // PENDING reviewers wake; master sleeps. A
                            // commit-reviewer who already filed Request-Changes
                            // /Unmarked is HELD (not pending), so it is NOT in
                            // the wake set — its verdict surfaces to the master
                            // once everyone has spoken. Single-source via
                            // `missing_for_gate` (→ `tier_coverage`).
                            let missing = crate::repo_state::NonEmptyVec::new(missing_for_gate(
                                gate, &entries, policy,
                            ))
                            .expect(
                                "Unreviewed gate state implies a non-empty pending commit-reviewer set",
                            );
                            WaitingOn::ReviewerApprovalsMissing { missing }
                        }
                    }
                };

                // Commit-tag correction dominates review/master-action
                // states (but not Blocked — handled by the `continue`
                // above). Marked from the SAME violation that produced
                // `head_correction`, so display and routing can't drift.
                let waiting_on = if correction_plans.contains(key) {
                    WaitingOn::MasterToFixCommitTag
                } else {
                    waiting_on
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
                // An ad-hoc commit has no plan to "change", so it is
                // never a plan-doc milestone (latest_touched_plan =
                // false). The ad-hoc tier inversion is tracked
                // separately (see plan: Out of scope).
                let gate = compute_gate(
                    &entries,
                    &policy.commit_reviewers,
                    &policy.gate_reviewers,
                    false,
                );
                ad_hoc.push(AdHocWorkState {
                    sha: event.sha.clone(),
                    gate,
                });
            }
        }

        // PR reviews (clank-pr-review-mode): same gate engine as
        // plans, but keyed by PR + round instead of a sha. Feed
        // compute_gate the current-round verdicts with
        // latest_touched_plan=false, so the gate tier engages only
        // once the commit tier is FINISHED (the milestone) — the
        // plan's tier semantics, no new gate code.
        let mut pr_reviews = Vec::new();
        for input in reviews.pr_reviews() {
            let gate = compute_gate(
                &input.current_verdicts,
                &policy.commit_reviewers,
                &policy.gate_reviewers,
                false,
            );
            // Round 0 = master still drafting; the review hasn't been
            // opened (no `propose` yet), so no reviewer is summoned
            // whatever the gate says. round is the "opened?" signal —
            // the PR analogue of a plan's commit existing before its
            // reviewers are summoned.
            let missing_reviewers = if input.round == 0 {
                Vec::new()
            } else {
                missing_for_gate(gate, &input.current_verdicts, policy)
            };
            pr_reviews.push(PrReviewWorkState {
                pr: input.pr,
                repo: input.repo,
                round: input.round,
                gate,
                missing_reviewers,
            });
        }

        WorkStatus {
            plans,
            ad_hoc,
            pr_reviews,
            head_correction,
        }
    }
}

/// The active tier's reviewers to WAKE for the current commit, given the
/// gate state: the tier's PENDING reviewers (those who haven't submitted a
/// verdict yet). `Unreviewed` → pending commit reviewers; `ApprovedPendingGate`
/// → pending gate reviewers; any master-turn state → empty.
///
/// Pending — NOT "not-yet-positive" — is the right wake set under
/// wait-for-all: a reviewer who already filed Request-Changes/Unmarked has
/// verdicted (their verdict is HELD until everyone has spoken), so they must
/// not be re-summoned to review the same commit. Same single-source coverage
/// (`tier_coverage`) as `compute_gate`.
fn missing_for_gate(
    gate: crate::vocab::CommitGateState,
    current_verdicts: &[ReviewEntry],
    policy: &WorkPolicy,
) -> Vec<AgentLabel> {
    use crate::vocab::CommitGateState;
    let tier = match gate {
        CommitGateState::Unreviewed => &policy.commit_reviewers,
        CommitGateState::ApprovedPendingGate => &policy.gate_reviewers,
        _ => return Vec::new(),
    };
    let by_label: std::collections::HashMap<&AgentLabel, &crate::vocab::Verdict> = current_verdicts
        .iter()
        .map(|r| (&r.author, &r.verdict))
        .collect();
    tier_coverage(tier, &by_label).pending
}

impl WorkStatus {
    pub fn work_for(&self, author: &AgentLabel, role: Role) -> Vec<WaitItem> {
        // A broken HEAD tag preempts everything
        // (`commit-tag-fixup-is-first-class-state`): master fixes the
        // commit message before any review motion; reviewers are NOT
        // woken until it's fixed. This is the SINGLE source for the
        // master's fixup item (the per-plan `MasterToFixCommitTag`
        // marking below is for display only). Blocked entries are
        // co-surfaced by the caller (`wfw`), as for ordinary items.
        if let Some(correction) = &self.head_correction {
            return match role {
                Role::Master => vec![WaitItem::FixCommitTag {
                    sha: correction.sha.clone(),
                    violation: correction.violation.clone(),
                }],
                Role::Reviewer => Vec::new(),
            };
        }

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
        for pr in &self.pr_reviews {
            use crate::vocab::CommitGateState;
            match role {
                Role::Reviewer if pr.missing_reviewers.iter().any(|l| l == author) => {
                    out.push(WaitItem::PrReviewer {
                        pr: pr.pr,
                        round: pr.round,
                    });
                }
                Role::Master => {
                    let next = if pr.round == 0 {
                        // Round 0: master drafts, then `propose` opens
                        // the review. Always master's turn, regardless
                        // of gate.
                        Some(PrMasterNext::Draft)
                    } else {
                        match pr.gate {
                            CommitGateState::ChangesRequested => Some(PrMasterNext::Integrate),
                            CommitGateState::Finished => Some(PrMasterNext::Submit),
                            CommitGateState::Approved => Some(PrMasterNext::Continue),
                            // Unreviewed / ApprovedPendingGate →
                            // reviewers' turn; Blocked is unreachable
                            // for PR reviews.
                            _ => None,
                        }
                    };
                    if let Some(next) = next {
                        out.push(WaitItem::PrMaster {
                            pr: pr.pr,
                            round: pr.round,
                            next,
                        });
                    }
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

    fn pset(names: &[&str]) -> BTreeSet<PlanKey> {
        names.iter().map(|n| plan(n)).collect()
    }

    /// Build a `HeadCommit` + run `head_tag_violation` against
    /// `known = active ∪ touched` (the set `derive_status` uses).
    fn violate(
        adopted: bool,
        subject: &str,
        active: &[&str],
        touched: &[&str],
    ) -> Option<HeadTagViolation> {
        let head = HeadCommit {
            sha: sha("aaaa"),
            subject: subject.to_string(),
            touched: pset(touched),
            adopted,
        };
        let mut known: BTreeSet<PlanKey> = pset(active);
        known.extend(head.touched.iter().cloned());
        head_tag_violation(&head, &known)
    }

    #[test]
    fn head_tag_unknown_name_flagged_only_when_adopted() {
        // T = ∅ (pure code): an unknown tag name is the original
        // unknown-tag check. Flagged on an adopted repo, tolerated on
        // a guest repo.
        let v = violate(true, "[bar] work", &["foo"], &[]).expect("unknown tag flagged");
        assert_eq!(v.unknown, vec!["bar".to_string()]);
        assert!(v.untagged_touched.is_empty() && v.extra_named.is_empty());
        // `[misc]` is no longer special — just an unknown tag.
        assert!(violate(true, "[misc] one-off", &["foo"], &[]).is_some());
        // Known active plan, no touch → legit implementation tag.
        assert!(violate(true, "[foo] work", &["foo"], &[]).is_none());
        // No tag, no touch → ad-hoc, fine.
        assert!(violate(true, "just code", &["foo"], &[]).is_none());
        // NOT adopted → never police.
        assert!(violate(false, "[bar] work", &["foo"], &[]).is_none());
        // Multi-tag, one unknown (T = ∅) → flag only the unknown name.
        let v = violate(true, "[foo,bar] x", &["foo"], &[]).expect("partial unknown flagged");
        assert_eq!(v.unknown, vec!["bar".to_string()]);
    }

    #[test]
    fn head_tag_touched_but_unnamed_is_flagged() {
        // T ⊄ G: HEAD touched a plan file the tag doesn't name.
        // `[plan]` placeholder touching `real.md` (the motivating bug).
        let v = violate(true, "[plan] fix it", &["real"], &["real"])
            .expect("placeholder over real plan flagged");
        assert_eq!(v.untagged_touched, vec![plan("real")]);
        assert_eq!(v.unknown, vec!["plan".to_string()]);
        // untagged touching real.md → T={real}, G=∅.
        let v = violate(true, "fix it", &["real"], &["real"]).expect("untagged touch flagged");
        assert_eq!(v.untagged_touched, vec![plan("real")]);
        // `[a]` touching a.md + b.md → b unnamed.
        let v = violate(true, "[a] cross", &["a", "b"], &["a", "b"])
            .expect("second touched plan must be named");
        assert_eq!(v.untagged_touched, vec![plan("b")]);
        assert!(v.extra_named.is_empty());
    }

    #[test]
    fn head_tag_extra_named_on_plan_touch_is_flagged() {
        // G ⊋ T on a plan-touching commit: `[a,b]` but only a.md
        // touched. `b` rides along unverifiably — strict G == T bans it.
        let v = violate(true, "[a,b] x", &["a", "b"], &["a"]).expect("extra named flagged");
        assert_eq!(v.extra_named, vec![plan("b")]);
        assert!(v.untagged_touched.is_empty() && v.unknown.is_empty());
    }

    #[test]
    fn head_tag_strict_equality_when_touch_is_satisfied() {
        // T = G exactly → valid.
        assert!(violate(true, "[a,b] both", &["a", "b"], &["a", "b"]).is_none());
        // codex 1ea61ae: `[foo] finish`/`delete` IS a touch of foo, so
        // T={foo}=G even though finalize removes foo from active. The
        // carve-out falls out for free.
        assert!(violate(true, "[foo] finish", &[], &["foo"]).is_none());
        assert!(violate(true, "[foo] delete", &[], &["foo"]).is_none());
        // intro of a brand-new plan: its own tag is accepted via
        // known = active ∪ touched.
        assert!(violate(true, "[new] intro", &[], &["new"]).is_none());
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
        assert_eq!(
            compute_gate(&[], &[], &[], false),
            CommitGateState::Approved
        );
    }

    #[test]
    fn compute_gate_zero_reviewers_ignores_stale_request_changes() {
        // Removed reviewer's stale RC must not gate.
        let reviews = [entry(crate::vocab::Verdict::RequestChanges, "alice")];
        assert_eq!(
            compute_gate(&reviews, &[], &[], false),
            CommitGateState::Approved
        );
    }

    #[test]
    fn compute_gate_single_expected_approve_is_approved() {
        let reviews = [entry(crate::vocab::Verdict::Approve, "codex")];
        assert_eq!(
            compute_gate(&reviews, &[label("codex")], &[], false),
            CommitGateState::Approved
        );
    }

    #[test]
    fn compute_gate_single_expected_request_changes() {
        let reviews = [entry(crate::vocab::Verdict::RequestChanges, "codex")];
        assert_eq!(
            compute_gate(&reviews, &[label("codex")], &[], false),
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
            compute_gate(&reviews, &[label("codex"), label("ruthless")], &[], false),
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
            compute_gate(&reviews, &[label("codex"), label("ruthless")], &[], false),
            // Both reviewers have submitted (codex approve, ruthless RC) →
            // master woken with the full set.
            CommitGateState::ChangesRequested
        );
    }

    #[test]
    fn compute_gate_request_changes_held_while_another_pending_is_unreviewed() {
        // THE FIX: codex requests changes, ruthless hasn't verdicted →
        // master must NOT be woken on partial feedback. Gate stays Unreviewed
        // (reviewers wake, master sleeps); the RC is HELD until ruthless
        // verdicts.
        let reviews = [entry(crate::vocab::Verdict::RequestChanges, "codex")];
        assert_eq!(
            compute_gate(&reviews, &[label("codex"), label("ruthless")], &[], false),
            CommitGateState::Unreviewed
        );
    }

    #[test]
    fn compute_gate_two_request_changes_all_submitted_is_changes_requested() {
        let reviews = [
            entry(crate::vocab::Verdict::RequestChanges, "codex"),
            entry(crate::vocab::Verdict::RequestChanges, "ruthless"),
        ];
        assert_eq!(
            compute_gate(&reviews, &[label("codex"), label("ruthless")], &[], false),
            CommitGateState::ChangesRequested
        );
    }

    #[test]
    fn compute_gate_unmarked_held_while_another_pending_is_unreviewed() {
        // Unmarked parity: an unparseable verdict is non-positive, held like
        // RequestChanges while another reviewer is still pending.
        let reviews = [entry(crate::vocab::Verdict::Unmarked, "codex")];
        assert_eq!(
            compute_gate(&reviews, &[label("codex"), label("ruthless")], &[], false),
            CommitGateState::Unreviewed
        );
    }

    #[test]
    fn compute_gate_unmarked_with_all_submitted_is_changes_requested() {
        // Unmarked does NOT slip through to a positive state once all in.
        let reviews = [
            entry(crate::vocab::Verdict::Unmarked, "codex"),
            entry(crate::vocab::Verdict::Approve, "ruthless"),
        ];
        assert_eq!(
            compute_gate(&reviews, &[label("codex"), label("ruthless")], &[], false),
            CommitGateState::ChangesRequested
        );
    }

    #[test]
    fn compute_gate_gate_tier_request_changes_held_while_pending_is_pending_gate() {
        // At a milestone (commit tier approved): one gate reviewer requests
        // changes, another gate reviewer pending → master sleeps, gate
        // reviewers wake (ApprovedPendingGate), the gate RC held until all
        // gate reviewers verdict.
        let reviews = [
            entry(crate::vocab::Verdict::Approve, "codex"),
            entry(crate::vocab::Verdict::RequestChanges, "ruthless"),
        ];
        assert_eq!(
            compute_gate(
                &reviews,
                &[label("codex")],
                &[label("ruthless"), label("glm")],
                true,
            ),
            CommitGateState::ApprovedPendingGate
        );
    }

    #[test]
    fn compute_gate_gate_tier_request_changes_all_submitted_is_changes_requested() {
        let reviews = [
            entry(crate::vocab::Verdict::Approve, "codex"),
            entry(crate::vocab::Verdict::RequestChanges, "ruthless"),
            entry(crate::vocab::Verdict::Approve, "glm"),
        ];
        assert_eq!(
            compute_gate(
                &reviews,
                &[label("codex")],
                &[label("ruthless"), label("glm")],
                true,
            ),
            CommitGateState::ChangesRequested
        );
    }

    #[test]
    fn compute_gate_two_expected_one_missing_is_unreviewed() {
        let reviews = [entry(crate::vocab::Verdict::Approve, "codex")];
        assert_eq!(
            compute_gate(&reviews, &[label("codex"), label("ruthless")], &[], false),
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
            compute_gate(&reviews, &[label("codex"), label("ruthless")], &[], false),
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
            compute_gate(&reviews, &[label("codex"), label("ruthless")], &[], false),
            CommitGateState::Approved
        );
    }

    #[test]
    fn compute_gate_unmarked_treated_as_changes() {
        let reviews = [entry(crate::vocab::Verdict::Unmarked, "codex")];
        assert_eq!(
            compute_gate(&reviews, &[label("codex")], &[], false),
            CommitGateState::ChangesRequested
        );
    }

    // ── teams-based-agent-registration: two-tier compute_gate
    //    edge cases (ruthless pin 1 — defends compute_gate's
    //    PRODUCTION of the new states; the work_for tests above
    //    test RECEPTION but bypass compute_gate by constructing
    //    WorkStatus directly).

    #[test]
    fn compute_gate_empty_commit_gate_only_wakes_on_plan_doc_not_every_commit() {
        // Devolve case under milestone gating
        // (`gate-reviewers-only-plan-change-and-finish`, ruthless
        // pin): empty commit_reviewers + gate reviewer. With no
        // commit reviewer to signal FINISHED, the milestone reduces
        // to `touched_plan` only — so a ROUTINE (pure-code) commit
        // does NOT wake the gate reviewer (the regression this
        // guards against)...
        assert_eq!(
            compute_gate(&[], &[], &[label("ruthless")], false),
            CommitGateState::Approved,
            "empty-commit + gate: pure-code commit must NOT wake the gate reviewer"
        );
        // ...but a PLAN-DOC commit is a milestone and does.
        assert_eq!(
            compute_gate(&[], &[], &[label("ruthless")], true),
            CommitGateState::ApprovedPendingGate,
            "empty-commit + gate: plan-doc commit IS a milestone → gate wakes"
        );
    }

    #[test]
    fn compute_gate_empty_both_returns_approved_not_finished() {
        // Master-only repo: empty commit + empty gate → Approved
        // (NOT Finished). Preserves pre-plan behavior; lloyd
        // 2026-06-08 directive (codex 8cb01b6 catch on the
        // earlier plan-body drift).
        assert_eq!(
            compute_gate(&[], &[], &[], false),
            CommitGateState::Approved
        );
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
            compute_gate(&reviews, &[label("codex")], &[label("ruthless")], false),
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
            compute_gate(&reviews, &[label("codex")], &[label("ruthless")], false),
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
            compute_gate(&reviews, &[label("codex")], &[label("ruthless")], false),
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
            compute_gate(&reviews, &[label("codex")], &[label("ruthless")], false),
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
            compute_gate(&reviews, &[label("codex"), label("ruthless")], &[], false),
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
            compute_gate(&reviews, &[label("codex"), label("ruthless")], &[], false),
            CommitGateState::Unreviewed
        );
    }

    // ── gate-reviewers-only-plan-change-and-finish: milestone
    //    gating. The gate tier is consulted ONLY at a plan-doc
    //    change or a finish — never on routine WIP commits.

    #[test]
    fn compute_gate_routine_code_commit_approved_does_not_wake_gate() {
        // THE CORE REGRESSION FIX: commit-reviewer APPROVES a
        // pure-code (non-plan, non-finish) commit. The gate
        // reviewer must NOT be woken — gate passes to Approved so
        // master keeps moving.
        let reviews = [entry(crate::vocab::Verdict::Approve, "codex")];
        assert_eq!(
            compute_gate(&reviews, &[label("codex")], &[label("ruthless")], false),
            CommitGateState::Approved,
            "routine code commit (approved, not a milestone) must bypass the gate tier"
        );
    }

    #[test]
    fn compute_gate_plan_doc_commit_approved_wakes_gate() {
        // Milestone (1) — plan-doc change approved by the commit
        // tier → gate reviewer signs off on the plan.
        let reviews = [entry(crate::vocab::Verdict::Approve, "codex")];
        assert_eq!(
            compute_gate(&reviews, &[label("codex")], &[label("ruthless")], true),
            CommitGateState::ApprovedPendingGate,
            "plan-doc commit (touched_plan) IS a milestone → gate wakes"
        );
    }

    #[test]
    fn compute_gate_finished_commit_wakes_gate_even_without_plan_change() {
        // Milestone (2) — commit tier FINISHED on a pure-code
        // commit (touched_plan=false). Still a milestone (the final
        // gate), so the gate reviewer wakes.
        let reviews = [entry(crate::vocab::Verdict::Finished, "codex")];
        assert_eq!(
            compute_gate(&reviews, &[label("codex")], &[label("ruthless")], false),
            CommitGateState::ApprovedPendingGate,
            "FINISHED commit is a milestone → gate wakes even with no plan change"
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
            pr_reviews: Vec::new(),
            head_correction: None,
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
    fn snapshot_capture_watches_all_active_plans() {
        let mut state = RepoState::default();
        state.plans.insert(plan("a"), PlanState::default());
        state.plans.insert(plan("b"), PlanState::default());
        let snap = StartupSnapshot::capture(&state);
        assert_eq!(snap.watched.len(), 2);
        assert!(snap.watched.contains(&plan("a")));
        assert!(snap.watched.contains(&plan("b")));
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
    fn missing_for_gate_wakes_only_pending_not_held_request_changes() {
        // The wake-set fix: under wait-for-all, a commit reviewer who filed
        // RequestChanges has verdicted (HELD) — the wake set is the PENDING
        // reviewer only, NEVER the RC reviewer (who would otherwise be
        // re-summoned to re-review a commit they already verdicted on).
        // plan_policy()'s commit tier is [codex, ruthless].
        let verdicts = [entry(crate::vocab::Verdict::RequestChanges, "codex")];
        let missing = missing_for_gate(
            crate::vocab::CommitGateState::Unreviewed,
            &verdicts,
            &plan_policy(),
        );
        assert_eq!(missing, vec![label("ruthless")]);
    }

    // ============ PR review wait surface ============

    fn two_tier_policy() -> WorkPolicy {
        WorkPolicy {
            plan_feedback: true,
            adhoc_feedback: false,
            commit_reviewers: vec![label("codex")],
            gate_reviewers: vec![label("ruthless")],
        }
    }

    struct MockPrReviews(Vec<PrReviewInput>);
    impl PlanStateLookup for MockPrReviews {
        fn reviews_for(&self, _sha: &CommitSha) -> Vec<ReviewEntry> {
            Vec::new()
        }
        fn worktree_status(&self, _plan: &PlanKey) -> PlanWorktreeStatus {
            PlanWorktreeStatus::Clean
        }
        fn pr_reviews(&self) -> Vec<PrReviewInput> {
            self.0.clone()
        }
    }

    fn pr_input(round: u64, verdicts: &[(&str, crate::vocab::Verdict)]) -> PrReviewInput {
        PrReviewInput {
            pr: 123,
            repo: "o/r".into(),
            round,
            current_verdicts: verdicts
                .iter()
                .map(|(l, v)| ReviewEntry {
                    author: label(l),
                    verdict: *v,
                })
                .collect(),
        }
    }

    fn pr_state(input: PrReviewInput) -> PrReviewWorkState {
        let state = RepoState::default();
        state
            .derive_status(&MockPrReviews(vec![input]), &two_tier_policy(), None)
            .pr_reviews
            .into_iter()
            .next()
            .unwrap()
    }

    #[test]
    fn pr_gate_tier_progression() {
        use crate::vocab::{CommitGateState, Verdict};
        // Round >= 1 (review opened by `propose`). No current
        // verdicts → commit tier owes review.
        let s = pr_state(pr_input(1, &[]));
        assert_eq!(s.gate, CommitGateState::Unreviewed);
        assert_eq!(s.missing_reviewers, vec![label("codex")]);

        // Commit tier FINISHED is the milestone → gate tier wakes.
        let s = pr_state(pr_input(1, &[("codex", Verdict::Finished)]));
        assert_eq!(s.gate, CommitGateState::ApprovedPendingGate);
        assert_eq!(s.missing_reviewers, vec![label("ruthless")]);

        // Both tiers FINISHED → master submits, nobody owes review.
        let s = pr_state(pr_input(
            1,
            &[
                ("codex", Verdict::Finished),
                ("ruthless", Verdict::Finished),
            ],
        ));
        assert_eq!(s.gate, CommitGateState::Finished);
        assert!(s.missing_reviewers.is_empty());

        // A request-changes anywhere → master integrates.
        let s = pr_state(pr_input(1, &[("codex", Verdict::RequestChanges)]));
        assert_eq!(s.gate, CommitGateState::ChangesRequested);
        assert!(s.missing_reviewers.is_empty());
    }

    #[test]
    fn pr_round_zero_is_master_draft_turn() {
        // The reported bug: a freshly-started review (round 0, no
        // verdicts, nothing drafted) must NOT summon a reviewer — it's
        // master's turn to draft, then `propose`. compute_gate still
        // reads Unreviewed, but round, not gate, gates the summon.
        let s = pr_state(pr_input(0, &[]));
        assert_eq!(s.gate, crate::vocab::CommitGateState::Unreviewed);
        assert!(
            s.missing_reviewers.is_empty(),
            "no reviewer summoned before the review is opened"
        );
        let ws = WorkStatus {
            plans: Vec::new(),
            ad_hoc: Vec::new(),
            pr_reviews: vec![s],
            head_correction: None,
        };
        assert_eq!(
            ws.work_for(&label("claude"), Role::Master),
            vec![WaitItem::PrMaster {
                pr: 123,
                round: 0,
                next: PrMasterNext::Draft
            }]
        );
        assert!(ws.work_for(&label("codex"), Role::Reviewer).is_empty());
    }

    #[test]
    fn pr_stale_round_verdicts_are_dropped_by_the_projection() {
        // The projection only passes CURRENT-round verdicts; a verdict
        // for an older round must not satisfy the gate. (Here we
        // simulate the projection's filter by passing no current
        // verdicts — the stale ones never reach derive_status.)
        let s = pr_state(pr_input(3, &[]));
        assert_eq!(s.gate, crate::vocab::CommitGateState::Unreviewed);
        assert_eq!(s.round, 3);
    }

    fn ws_pr(gate: crate::vocab::CommitGateState, missing: &[&str]) -> WorkStatus {
        WorkStatus {
            plans: Vec::new(),
            ad_hoc: Vec::new(),
            pr_reviews: vec![PrReviewWorkState {
                pr: 123,
                repo: "o/r".into(),
                round: 2,
                gate,
                missing_reviewers: missing.iter().map(|l| label(l)).collect(),
            }],
            head_correction: None,
        }
    }

    #[test]
    fn work_for_routes_pr_reviewer_only_to_the_missing() {
        let ws = ws_pr(crate::vocab::CommitGateState::Unreviewed, &["codex"]);
        assert_eq!(
            ws.work_for(&label("codex"), Role::Reviewer),
            vec![WaitItem::PrReviewer { pr: 123, round: 2 }]
        );
        // ruthless isn't in the missing set → no item.
        assert!(ws.work_for(&label("ruthless"), Role::Reviewer).is_empty());
        // master gets nothing while reviewers owe review.
        assert!(ws.work_for(&label("claude"), Role::Master).is_empty());
    }

    #[test]
    fn work_for_routes_pr_master_by_gate() {
        use crate::vocab::CommitGateState;
        let cases = [
            (CommitGateState::ChangesRequested, PrMasterNext::Integrate),
            (CommitGateState::Finished, PrMasterNext::Submit),
            (CommitGateState::Approved, PrMasterNext::Continue),
        ];
        for (gate, next) in cases {
            let ws = ws_pr(gate, &[]);
            assert_eq!(
                ws.work_for(&label("claude"), Role::Master),
                vec![WaitItem::PrMaster {
                    pr: 123,
                    round: 2,
                    next
                }],
                "gate {gate:?}"
            );
            // reviewers get nothing on a master-turn gate.
            assert!(ws.work_for(&label("codex"), Role::Reviewer).is_empty());
        }
    }

    #[test]
    fn work_for_pr_master_silent_while_reviewers_owe() {
        // ApprovedPendingGate / Unreviewed are reviewers' turns —
        // master must not get a PrMaster item.
        for gate in [
            crate::vocab::CommitGateState::Unreviewed,
            crate::vocab::CommitGateState::ApprovedPendingGate,
        ] {
            let ws = ws_pr(gate, &["ruthless"]);
            assert!(
                ws.work_for(&label("claude"), Role::Master).is_empty(),
                "gate {gate:?}"
            );
        }
    }

    #[test]
    fn compute_gate_unaffected_by_blocks() {
        // compute_gate is per-commit pure; blocks live a layer up
        // in derive_status. compute_gate's behavior is unchanged.
        assert_eq!(
            compute_gate(&[], &[], &[], false),
            CommitGateState::Approved
        );
        let reviews = [entry(crate::vocab::Verdict::Approve, "codex")];
        assert_eq!(
            compute_gate(&reviews, &[label("codex")], &[], false),
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

    fn make_plan_with_plan_doc_commit(commit_sha: CommitSha) -> PlanState {
        use crate::repo_state::PlanTimelineEvent;
        let mut ps = PlanState::default();
        ps.commits.push(PlanTimelineEvent {
            sha: commit_sha.clone(),
            ts: 1,
            touched_plan: true,
            touched_code: false,
        });
        ps
    }

    fn master_gate_policy() -> WorkPolicy {
        WorkPolicy {
            plan_feedback: true,
            adhoc_feedback: false,
            commit_reviewers: vec![label("codex")],
            gate_reviewers: vec![label("ruthless")],
        }
    }

    // ── commit-tag-fixup-is-first-class-state ──────────────────
    //
    // HEAD's tag must match the plan files it touched. A violation is
    // a dominating derived state: master gets a FixCommitTag item,
    // reviewers get nothing, and the implicated plan row renders the
    // correction.

    fn head(subject: &str, touched: &[&str]) -> HeadCommit {
        HeadCommit {
            sha: sha("aaaa"),
            subject: subject.to_string(),
            touched: touched.iter().map(|t| plan(t)).collect(),
            adopted: true,
        }
    }

    /// A plan with one reviewable commit at `aaaa`, two registered
    /// commit reviewers (so absent the correction it would be
    /// Unreviewed → reviewers woken).
    fn state_one_plan(stem: &str) -> RepoState {
        let mut state = RepoState::default();
        state
            .plans
            .insert(plan(stem), make_plan_with_one_reviewable(sha("aaaa")));
        state
    }

    #[test]
    fn derive_status_placeholder_tag_over_real_plan_is_correction() {
        // `[plan]` placeholder touching real.md → T={real}, G={plan}.
        // Reproduce-first: the OLD one-directional check (`G ⊆
        // active∪touched`) never fired here because `plan` parses; only
        // the new `T ⊆ G` direction catches it.
        let state = state_one_plan("real");
        let status = state.derive_status(
            &MockReviews(vec![]),
            &plan_policy(),
            Some(&head("[plan] fix", &["real"])),
        );
        assert!(
            status.head_correction.is_some(),
            "violation must be derived"
        );
        assert_eq!(status.plans[0].waiting_on, WaitingOn::MasterToFixCommitTag);
        // Master routed to fix; reviewers NOT woken.
        let master = status.work_for(&label("lloyd"), Role::Master);
        assert_eq!(master.len(), 1);
        assert!(matches!(master[0], WaitItem::FixCommitTag { .. }));
        assert!(status.work_for(&label("codex"), Role::Reviewer).is_empty());
        assert!(
            status
                .work_for(&label("ruthless"), Role::Reviewer)
                .is_empty()
        );
    }

    #[test]
    fn derive_status_untagged_touch_is_correction() {
        // untagged commit touching real.md → T={real}, G=∅.
        let state = state_one_plan("real");
        let status = state.derive_status(
            &MockReviews(vec![]),
            &plan_policy(),
            Some(&head("fix it", &["real"])),
        );
        assert!(status.head_correction.is_some());
        assert_eq!(status.plans[0].waiting_on, WaitingOn::MasterToFixCommitTag);
        assert!(status.work_for(&label("codex"), Role::Reviewer).is_empty());
    }

    #[test]
    fn derive_status_tag_missing_second_touched_plan_is_correction() {
        // `[a]` touching a.md + b.md → b unnamed. Both plans active.
        let mut state = RepoState::default();
        state
            .plans
            .insert(plan("a"), make_plan_with_one_reviewable(sha("aaaa")));
        state
            .plans
            .insert(plan("b"), make_plan_with_one_reviewable(sha("bbbb")));
        let status = state.derive_status(
            &MockReviews(vec![]),
            &plan_policy(),
            Some(&head("[a] cross", &["a", "b"])),
        );
        assert!(status.head_correction.is_some());
        // Both implicated plans carry the correction.
        for ps in &status.plans {
            assert_eq!(
                ps.waiting_on,
                WaitingOn::MasterToFixCommitTag,
                "plan {} must carry the correction",
                ps.plan.as_str()
            );
        }
        let master = status.work_for(&label("lloyd"), Role::Master);
        assert_eq!(master.len(), 1, "exactly one fixup, not one per plan");
    }

    #[test]
    fn derive_status_valid_multi_tag_stays_valid() {
        // `[a,b]` touching a.md + b.md → T == G → no correction; normal
        // review resumes (reviewers woken on the Unreviewed gate).
        let mut state = RepoState::default();
        state
            .plans
            .insert(plan("a"), make_plan_with_one_reviewable(sha("aaaa")));
        state
            .plans
            .insert(plan("b"), make_plan_with_one_reviewable(sha("bbbb")));
        let status = state.derive_status(
            &MockReviews(vec![]),
            &plan_policy(),
            Some(&head("[a,b] both", &["a", "b"])),
        );
        assert!(status.head_correction.is_none(), "T == G is valid");
        for ps in &status.plans {
            assert!(matches!(
                ps.waiting_on,
                WaitingOn::ReviewerApprovalsMissing { .. }
            ));
        }
        assert_eq!(status.work_for(&label("codex"), Role::Reviewer).len(), 2);
    }

    #[test]
    fn derive_status_finish_delete_carveout_stays_valid() {
        // `[foo] finish` removes foo from active but IS a touch of foo,
        // so T={foo}=G → valid (no correction). (foo already gone from
        // `plans` post-finalize; the violation check is fold-independent
        // and reads only HEAD facts.)
        let state = RepoState::default();
        let status = state.derive_status(
            &MockReviews(vec![]),
            &plan_policy(),
            Some(&head("[foo] finish", &["foo"])),
        );
        assert!(status.head_correction.is_none());
    }

    #[test]
    fn derive_status_adhoc_code_and_impl_tag_stay_valid() {
        // T = ∅: untagged ad-hoc is fine, and a `[foo]` implementation
        // commit (active, no plan touch) is the common case — must stay
        // valid (G == T does NOT apply when T = ∅).
        let state = state_one_plan("foo");
        // untagged code commit, no plan touch.
        let s1 = state.derive_status(
            &MockReviews(vec![]),
            &plan_policy(),
            Some(&head("just code", &[])),
        );
        assert!(s1.head_correction.is_none(), "ad-hoc untagged is valid");
        // `[foo]` implementation commit, no plan touch.
        let s2 = state.derive_status(
            &MockReviews(vec![]),
            &plan_policy(),
            Some(&head("[foo] implement", &[])),
        );
        assert!(s2.head_correction.is_none(), "implementation tag is valid");
        assert_eq!(s2.work_for(&label("codex"), Role::Reviewer).len(), 1);
    }

    #[test]
    fn derive_status_correction_yields_to_block() {
        // Precedence: a plan-scoped block dominates even the tag
        // correction (a human-blocking issue still wins). The blocked
        // plan stays Blocked; the violation still routes master.
        let state = state_one_plan("real");
        let blocks = std::collections::BTreeMap::from([(
            plan("real"),
            vec![mkblock("claude", "halt", "hold")],
        )]);
        let status = state.derive_status(
            &MockBlocks(blocks),
            &plan_policy(),
            Some(&head("[plan] fix", &["real"])),
        );
        assert!(matches!(
            status.plans[0].waiting_on,
            WaitingOn::Blocked { .. }
        ));
    }

    #[test]
    fn derive_status_no_head_no_correction() {
        // No HEAD facts (fresh repo / caller didn't supply) → never a
        // correction; normal review path.
        let state = state_one_plan("real");
        let status = state.derive_status(&MockReviews(vec![]), &plan_policy(), None);
        assert!(status.head_correction.is_none());
    }

    #[test]
    fn derive_status_routine_code_commit_does_not_wake_gate_reviewer() {
        // THE REGRESSION GUARANTEE, end-to-end (compute_gate →
        // derive_status → work_for), per the plan's edge-list and
        // ruthless's mechanical-check 67: a routine code commit the
        // commit tier APPROVED must NOT produce a work item for the
        // gate reviewer. (compute_gate proving Approved isn't enough
        // — work_for is a separate match-arm surface.)
        let mut state = RepoState::default();
        state
            .plans
            .insert(plan("foo"), make_plan_with_one_reviewable(sha("aaaa")));
        let reviews = MockReviews(vec![(
            sha("aaaa"),
            vec![entry(crate::vocab::Verdict::Approve, "codex")],
        )]);
        let status = state.derive_status(&reviews, &master_gate_policy(), None);

        assert_eq!(
            status.plans[0].gate,
            CommitGateState::Approved,
            "routine code commit must bypass the gate tier"
        );
        assert!(
            status
                .work_for(&label("ruthless"), Role::Reviewer)
                .is_empty(),
            "gate reviewer must NOT be woken on a routine code commit"
        );
    }

    #[test]
    fn derive_status_plan_doc_commit_wakes_gate_reviewer() {
        // Contrast: a plan-doc commit the commit tier APPROVED IS a
        // milestone → the gate reviewer DOES get a work item.
        let mut state = RepoState::default();
        state
            .plans
            .insert(plan("foo"), make_plan_with_plan_doc_commit(sha("bbbb")));
        let reviews = MockReviews(vec![(
            sha("bbbb"),
            vec![entry(crate::vocab::Verdict::Approve, "codex")],
        )]);
        let status = state.derive_status(&reviews, &master_gate_policy(), None);

        assert_eq!(status.plans[0].gate, CommitGateState::ApprovedPendingGate);
        assert_eq!(
            status.work_for(&label("ruthless"), Role::Reviewer).len(),
            1,
            "gate reviewer must be woken on an approved plan-doc commit"
        );
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
        let status = state.derive_status(&MockBlocks(blocks), &plan_policy(), None);
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
        let status = state.derive_status(&MockBlocks(blocks), &plan_policy(), None);
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
        let status = state.derive_status(&MockBlocks(Default::default()), &plan_policy(), None);
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
        let status = state.derive_status(&MockBlocks(blocks), &plan_policy(), None);
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
        let status = state.derive_status(&MockBlocks(blocks), &plan_policy(), None);
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
        let status = state.derive_status(&reviews, &adhoc_policy(), None);
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
        let status = state.derive_status(&reviews, &adhoc_policy(), None);
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
        let status = state.derive_status(&MockDirty::new(), &plan_policy(), None);
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
        let status = state.derive_status(&mock, &plan_policy(), None);
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
        let status = state.derive_status(&mock, &plan_policy(), None);
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
        let status = state.derive_status(&mock, &plan_policy(), None);
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
        let status = state.derive_status(&MockDirty::new(), &plan_policy(), None);
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
