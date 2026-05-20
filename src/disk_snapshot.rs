//! The sans-IO boundary for repo-state derivation.
//!
//! `CommitSnapshot` is a list of per-commit events along HEAD's
//! first-parent chain. Each `CommitEvent` carries everything the
//! fold needs from that commit's diff: plan touches with bodies,
//! finalize changes with first lines, code-change flag, author
//! timestamp, subject. Working-tree feedback is **not** in
//! `CommitSnapshot` — it's collected separately by
//! `git_io::collect_feedback_files` and applied in the live overlay.
//!
//! `derive_base_state` is a pure function: start with empty
//! `RepoState`, apply each `CommitEvent` in order. The result is a
//! `BaseRepoState` whose gates have empty feedback and `Unreviewed`
//! state — those are filled in by `attach_live_feedback`.
//!
//! The split is the architectural contract from
//! `.trinity/plans/cache-core-fold-and-live-feedback.md`: only the
//! commit-derived `BaseRepoState` is cacheable, because only it is a
//! pure function of HEAD's commit DAG.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use crate::attribution::{CommitChanges, FinalizeChangeKind, classify, effective_session};
use crate::disk_format::{FeedbackPath, finalize_first_line_starts_with_approve, parse_verdict};
use crate::lifecycle::{AgentLabel, CommitSha, PlanKey, content_hash};
use crate::repo_state::{
    AttributionResult, BaseRepoState, CommitAttribution, CommitKind, CommitNode, Feedback,
    LiveRepoState, Plan, PlanTimelineEvent, PlanTouchKind, RepoState, Verdict,
};
use crate::review_state::{CommitGate, CommitGateState};

/// Commit-derived input to the fold. Strictly a function of HEAD's
/// first-parent commit DAG — no working-tree state. `git_io::snapshot`
/// produces it; `derive_base_state` folds it into a `BaseRepoState`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CommitSnapshot {
    pub head: Option<CommitSha>,
    /// First-parent commit chain from the root to HEAD, oldest-first.
    pub history: Vec<CommitEvent>,
}

/// One first-parent commit's contribution to repo state: the diff
/// against its parent (plan touches with bodies, finalize-file
/// changes, code-change flag) plus author timestamp and subject. The
/// fold applies these in chronological order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitEvent {
    pub commit: CommitSha,
    pub author_ts: i64,
    pub subject: String,
    pub changes: CommitChanges,
}

/// One feedback file in the working tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedbackBlob {
    pub abs_path: PathBuf,
    pub parsed: FeedbackPath,
    pub body: String,
    /// Unix-seconds mtime from `fs::metadata`. Threaded into `Feedback` /
    /// `HeldFeedback` so UI surfaces can sort feedback chronologically.
    pub created_at: i64,
}

/// Pure derivation of `BaseRepoState` from a commit snapshot. No
/// IO and no working-tree access — feedback-blind. The resulting
/// reviewable gates carry empty `feedback` maps and `Unreviewed`
/// state; [`attach_live_feedback`] fills those in.
///
/// Single chronological commit fold: for each commit in
/// `snapshot.history` (oldest-first), update attribution and
/// plan_touches, replay finalize-snapshot changes into a running
/// per-plan tree state, check the finalize rule (and set
/// `frozen_at` monotonically), then append per-plan timeline
/// events. Frozen plans are skipped end-to-end for subsequent
/// commits.
pub fn derive_base_state(repo_root: PathBuf, snapshot: CommitSnapshot) -> BaseRepoState {
    let mut state = RepoState::empty(repo_root);
    state.head = snapshot.head;
    let mut carry = FoldCarry::new();
    for event in &snapshot.history {
        apply_commit(&mut state, &mut carry, event);
    }
    BaseRepoState::new(state)
}

/// Apply working-tree feedback files on top of a `BaseRepoState`.
/// This is the only stage that reads mutable `.trinity/feedback/`
/// data — the cache layer never sees it.
///
/// Algorithm (per active plan, oldest-to-newest):
///
/// 1. Build a per-plan feedback index keyed by `(plan, target_sha,
///    author)`.
/// 2. Write each feedback entry into the matching reviewable
///    timeline event's `gate.feedback` map.
/// 3. Call [`rebuild_plan_gates`] to recompute every reviewable
///    gate's participants / approvers / requesters / missing /
///    state chronologically. **Cumulative participants propagate
///    forward** — a feedback file on commit A makes the reviewer a
///    participant for every later reviewable gate in that plan.
/// 4. Bump `plan.last_activity_ts` to include feedback mtimes.
///
/// Finished plans are sealed: live feedback targeting their
/// commits is ignored at this stage.
pub fn attach_live_feedback(
    base: BaseRepoState,
    feedback_files: Vec<FeedbackBlob>,
) -> LiveRepoState {
    let mut state = base.into_inner();

    // Index feedback by plan. Per-plan we'll walk the timeline once.
    let mut by_plan: BTreeMap<PlanKey, Vec<FeedbackBlob>> = BTreeMap::new();
    for fb in feedback_files {
        by_plan
            .entry(fb.parsed.plan_key.clone())
            .or_default()
            .push(fb);
    }

    for (plan_key, blobs) in by_plan {
        let Some(plan) = state.plans.get(&plan_key) else {
            continue;
        };
        if plan.is_frozen() {
            // Sealed plans don't accept live feedback — finished
            // semantics are commit-derived only.
            continue;
        }

        // Write each blob into the matching commit's gate.feedback.
        // Phase 2 of commit-first-review-model: the gate lives on
        // RepoState.commits[sha], indexed by SHA. Drop silently if
        // the target SHA isn't a reviewable commit attributed to
        // this plan.
        //
        // `max_feedback_ts` is only bumped after a successful
        // insert. A stale feedback file with a target SHA that no
        // longer exists must NOT make the plan look recently active.
        let mut max_feedback_ts: i64 = 0;
        for fb in blobs {
            let target = fb.parsed.target_sha.clone();
            let Some(node) = state.commits.get_mut(&target) else {
                continue;
            };
            // The commit's attribution must include this plan for the
            // feedback to land — otherwise the path is malformed
            // (e.g. typo, ad hoc commit's feedback being filed under
            // a real plan key). Phase 4 routes ad hoc feedback via
            // the `_` reserved key, not a plan key, so this gate is
            // strict by design.
            let belongs_to_plan = match &node.attribution {
                CommitAttribution::Plan { plan } => plan == &plan_key,
                _ => false,
            };
            if !belongs_to_plan {
                continue;
            }
            let Some(gate) = node.gate.as_mut() else {
                continue;
            };
            let verdict = parse_verdict(&fb.body);
            let created_at = fb.created_at;
            let feedback = Feedback {
                author: fb.parsed.author.clone(),
                verdict,
                body: fb.body,
                path: fb.abs_path.to_string_lossy().into_owned(),
                created_at,
            };
            gate.feedback.insert(feedback.author.clone(), feedback);
            if created_at > max_feedback_ts {
                max_feedback_ts = created_at;
            }
        }

        rebuild_plan_gates(&mut state, &plan_key);
        if let Some(plan) = state.plans.get_mut(&plan_key)
            && max_feedback_ts > plan.last_activity_ts
        {
            plan.last_activity_ts = max_feedback_ts;
        }
    }

    LiveRepoState::new(state)
}

/// Rebuild every reviewable gate that belongs to `plan_key`, walking
/// chronologically with cumulative participants.
///
/// Phase 2 of `commit-first-review-model`: gates live on
/// `RepoState.commits`, so this helper takes `&mut RepoState` and a
/// plan key. The walk order is the plan's `timeline` (chronological
/// SHA list); for each reviewable SHA, look up `state.commits[sha]`
/// and recompose its gate from its accumulated feedback +
/// cumulative-participant carry.
///
/// **The single source of truth for live-feedback gate composition.**
/// Called from both [`attach_live_feedback`] (bulk rebuild from disk)
/// and the runtime's `upsert_feedback` / `remove_feedback` watcher
/// path (incremental in-memory update). Duplicating the chronological-
/// walk algorithm is exactly how the cache path and live daemon path
/// would drift; keep one impl.
///
/// Frozen plans are no-ops.
pub fn rebuild_plan_gates(state: &mut RepoState, plan_key: &PlanKey) {
    let Some(plan) = state.plans.get(plan_key) else {
        return;
    };
    if plan.is_frozen() {
        return;
    }
    let sha_seq: Vec<CommitSha> = plan
        .timeline
        .iter()
        .filter(|e| e.is_reviewable())
        .map(|e| e.sha().clone())
        .collect();
    let mut participants: Vec<AgentLabel> = Vec::new();
    for sha in &sha_seq {
        let Some(node) = state.commits.get_mut(sha) else {
            continue;
        };
        let Some(gate) = node.gate.as_mut() else {
            continue;
        };
        let commit_feedback = gate.feedback.clone();
        *gate = compose_gate(sha, commit_feedback, &mut participants);
    }
}

/// Transient fold state carried between `apply_commit` calls. Holds
/// just enough to apply the next commit without rebuilding from the
/// past — the running tree state (which plan files exist + their
/// bodies), the running finalize-tree (which APPROVE files each
/// plan has), the walk-back attribution carry, and the previous
/// commit's SHA (for `plan_intro_parent` of plans introduced at
/// the next commit).
///
/// Constructed by `FoldCarry::new()` and threaded through every
/// `apply_commit` call. None of these fields belong on `RepoState`
/// — they're scratch state for the fold, not facts about the repo.
/// Feedback is **not** carried here any more; gates are filled
/// with empty feedback by the base fold and rebuilt by
/// [`attach_live_feedback`].
pub struct FoldCarry {
    current_effective: Option<PlanKey>,
    plan_in_tree: BTreeSet<PlanKey>,
    plan_bodies: BTreeMap<PlanKey, String>,
    finalize_tree: BTreeMap<PlanKey, BTreeMap<String, String>>,
    previous_commit: Option<CommitSha>,
}

impl FoldCarry {
    pub fn new() -> Self {
        Self {
            current_effective: None,
            plan_in_tree: BTreeSet::new(),
            plan_bodies: BTreeMap::new(),
            finalize_tree: BTreeMap::new(),
            previous_commit: None,
        }
    }
}

impl Default for FoldCarry {
    fn default() -> Self {
        Self::new()
    }
}

/// Apply one `CommitEvent` to the accumulator. Every state mutation
/// for the commit happens here in one place — `derive_state` is just
/// a loop. After `apply_commit` returns, `state` correctly reflects
/// everything up to and including this commit; no post-pass needed.
pub fn apply_commit(state: &mut RepoState, carry: &mut FoldCarry, event: &CommitEvent) {
    let commit_sha = event.commit.clone();

    // 1. Walk-back attribution. Filter frozen-plan touches BEFORE
    //    classify / effective_session — sealing applies to inheritance.
    if let Some(k) = &carry.current_effective
        && is_frozen(state, k)
    {
        carry.current_effective = None;
    }
    let active_changes = if event
        .changes
        .plan_touches
        .iter()
        .any(|t| is_frozen(state, &t.session))
    {
        CommitChanges {
            plan_touches: event
                .changes
                .plan_touches
                .iter()
                .filter(|t| !is_frozen(state, &t.session))
                .cloned()
                .collect(),
            has_non_plan_code_changes: event.changes.has_non_plan_code_changes,
            finalize_changes: event.changes.finalize_changes.clone(),
            trinity_paths: event.changes.trinity_paths.clone(),
            touched_trinity: event.changes.touched_trinity,
            trinity_paths_touched: event.changes.trinity_paths_touched.clone(),
        }
    } else {
        event.changes.clone()
    };
    let attr = classify(&active_changes, carry.current_effective.as_ref());
    carry.current_effective = effective_session(&active_changes, carry.current_effective.as_ref());

    // 2. Apply plan_touches: tree state + Plan create/delete.
    for touch in &event.changes.plan_touches {
        let is_delete = matches!(touch.kind, PlanTouchKind::Revision) && touch.new_path.is_none();
        if is_delete {
            carry.plan_in_tree.remove(&touch.session);
            carry.plan_bodies.remove(&touch.session);
            // Monotone-finished: frozen plans stay in `state.plans`
            // even after their plan file is deleted (body captured
            // at freeze). Unfrozen plans have no anchor → remove.
            if !is_frozen(state, &touch.session) {
                state.plans.remove(&touch.session);
            }
        } else {
            carry.plan_in_tree.insert(touch.session.clone());
            if let Some(body) = &touch.new_body {
                carry
                    .plan_bodies
                    .insert(touch.session.clone(), body.clone());
            }
            // Intro of a stem not in `state.plans` creates the
            // entry. `plan_intro` is this commit; `plan_intro_parent`
            // is the previous first-parent commit (or `None` at the
            // root).
            if matches!(touch.kind, PlanTouchKind::Intro)
                && touch.new_path.is_some()
                && !state.plans.contains_key(&touch.session)
            {
                let plan_path = touch
                    .new_path
                    .clone()
                    .expect("new_path is Some per outer guard")
                    .to_string_lossy()
                    .into_owned();
                let body = carry
                    .plan_bodies
                    .get(&touch.session)
                    .cloned()
                    .unwrap_or_default();
                let body_hash = content_hash(&body);
                state.plans.insert(
                    touch.session.clone(),
                    Plan {
                        id: touch.session.clone(),
                        plan_path,
                        body,
                        body_hash,
                        plan_intro: commit_sha.clone(),
                        plan_intro_parent: carry.previous_commit.clone(),
                        last_activity_ts: 0,
                        timeline: Vec::new(),
                        archived_cycles: Vec::new(),
                    },
                );
            }
            // Revision of a non-frozen plan tracks HEAD body. Frozen
            // plans are sealed — body stays at freeze-time.
            if matches!(touch.kind, PlanTouchKind::Revision)
                && touch.new_path.is_some()
                && !is_frozen(state, &touch.session)
                && let (Some(plan), Some(body)) =
                    (state.plans.get_mut(&touch.session), &touch.new_body)
            {
                plan.body = body.clone();
                plan.body_hash = content_hash(body);
            }
        }
    }

    // 3. Apply finalize_changes to the running finalize tree, and
    //    collect plans potentially affected by this commit (touched
    //    plus had a finalize change). The freeze check only runs
    //    against those.
    let mut maybe_affected: BTreeSet<PlanKey> = BTreeSet::new();
    for fc in &event.changes.finalize_changes {
        let files = carry.finalize_tree.entry(fc.plan_key.clone()).or_default();
        match &fc.kind {
            FinalizeChangeKind::Upsert { first_line } => {
                files.insert(fc.file_name.clone(), first_line.clone());
            }
            FinalizeChangeKind::Remove => {
                files.remove(&fc.file_name);
            }
        }
        maybe_affected.insert(fc.plan_key.clone());
    }
    for touch in &event.changes.plan_touches {
        maybe_affected.insert(touch.session.clone());
    }

    // 4. Freeze rule. When fired, set `frozen_at` AND append a
    //    `Finalize` timeline event so the lifecycle boundary is
    //    visible/clickable in the timeline. Finalize events carry
    //    `gate: None` — they are never review targets and don't feed
    //    `waiting_on`. The approving files live in
    //    `.trinity/finished/<stem>/` at this commit (a snapshot, not
    //    live feedback).
    let mut plans_finalized_here: BTreeSet<PlanKey> = BTreeSet::new();
    for plan_key in &maybe_affected {
        if !state.plans.contains_key(plan_key) {
            continue;
        }
        if is_frozen(state, plan_key) {
            continue;
        }
        if !carry.plan_in_tree.contains(plan_key) {
            continue;
        }
        let files = carry.finalize_tree.get(plan_key);
        if finalize_rule_satisfied(files) {
            let approver_count = files.map(|m| m.len()).unwrap_or(0) as u32;
            let captured_body = carry.plan_bodies.get(plan_key).cloned();
            if let Some(plan) = state.plans.get_mut(plan_key) {
                if let Some(body) = captured_body {
                    plan.body_hash = content_hash(&body);
                    plan.body = body;
                }
                plan.archived_cycles.push(crate::repo_state::ArchivedCycle {
                    closer: commit_sha.clone(),
                    approver_count,
                });
                // Append the Finalize event last — this is what makes
                // `plan.frozen_at()` return Some(commit_sha). The
                // monotone rule guarantees no further events on this
                // plan's timeline (step 5 short-circuits on frozen
                // plans).
                plan.timeline.push(PlanTimelineEvent::Finalize {
                    sha: commit_sha.clone(),
                    author_ts: event.author_ts,
                    subject: event.subject.clone(),
                });
                if event.author_ts > plan.last_activity_ts {
                    plan.last_activity_ts = event.author_ts;
                }
                plans_finalized_here.insert(plan_key.clone());
            }
        }
    }

    // 5. Per-plan classification + timeline append. For each non-frozen
    //    plan, decide this commit's kind FOR THAT PLAN (PlanOnly /
    //    CodeOnly / Mixed / MultiPlan) and append a single
    //    `PlanTimelineEvent` carrying the kind, author_ts, subject,
    //    and (for reviewable kinds) the computed gate. `Unattributed`
    //    is not part of any plan's timeline and is skipped.
    //
    //    `active_changes` (not raw `event.changes`) drives this so a
    //    commit touching both a frozen and an active plan does not
    //    flip the active plan's classification to MultiPlan — sealing
    //    must hide the frozen plan from per-plan classification too.
    let distinct_plans_touched = active_changes
        .plan_touches
        .iter()
        .map(|t| &t.session)
        .collect::<BTreeSet<_>>()
        .len();
    let attr_session: Option<&PlanKey> = match &attr {
        AttributionResult::Attributed {
            session,
            has_code_changes: true,
            ..
        } => Some(session),
        _ => None,
    };

    // Track which plans this commit ends up attributed-to (per-plan timeline
    // event of any kind). Combined with `plans_finalized_here` below to build
    // the repo-wide `CommitNode.plans` set.
    let mut plans_with_event: BTreeSet<PlanKey> = plans_finalized_here.clone();
    // The per-plan kind for whichever plan owns this commit's reviewable
    // event — used as the repo-wide `CommitNode.kind` when attribution is
    // single-plan. `None` when the commit produced no reviewable per-plan
    // event.
    let mut single_plan_kind: Option<(PlanKey, CommitKind)> = None;
    // The single-plan gate (Phase 2 of commit-first-review-model). When
    // exactly one plan claims this commit as a reviewable event, the
    // gate lands on `CommitNode.gate` indexed by SHA. MultiPlan / multi-
    // touch commits leave this `None`.
    let mut single_plan_gate: Option<CommitGate> = None;

    let plan_keys: Vec<PlanKey> = state.plans.keys().cloned().collect();
    for plan_key in &plan_keys {
        if is_frozen(state, plan_key) {
            continue;
        }
        let our_touch = active_changes
            .plan_touches
            .iter()
            .find(|t| &t.session == plan_key);
        let has_code_for_plan = attr_session == Some(plan_key);
        let kind = if our_touch.is_some() {
            if distinct_plans_touched >= 2 {
                CommitKind::MultiPlan
            } else if has_code_for_plan {
                CommitKind::Mixed
            } else {
                CommitKind::PlanOnly
            }
        } else if has_code_for_plan {
            CommitKind::CodeOnly
        } else {
            CommitKind::Unattributed
        };
        if matches!(kind, CommitKind::Unattributed) {
            continue;
        }
        let gate = if kind.is_reviewable() {
            // Base fold: gates have no feedback yet. `attach_live_feedback`
            // fills them in via `rebuild_plan_gates`. The state is
            // Unreviewed with empty participant/approver/missing sets.
            Some(CommitGate {
                state: CommitGateState::Unreviewed,
                participants: Vec::new(),
                approvers: Vec::new(),
                requesters: Vec::new(),
                ambiguous: Vec::new(),
                missing: Vec::new(),
                feedback: BTreeMap::new(),
            })
        } else {
            None
        };
        let plan = state.plans.get_mut(plan_key).expect("just verified");
        if event.author_ts > plan.last_activity_ts {
            plan.last_activity_ts = event.author_ts;
        }
        // Feedback timestamps no longer enter here; `attach_live_feedback`
        // bumps `last_activity_ts` from feedback mtimes after the base
        // fold completes.
        let sha = commit_sha.clone();
        let author_ts = event.author_ts;
        let subject = event.subject.clone();
        // Phase 2 of commit-first-review-model: the gate lives on the
        // RepoState.commits map, not on the timeline event. The per-
        // plan timeline retains its chronological role as a SHA list
        // filtered by attribution; gate lookups thread through
        // `state.gate_for(sha)`. Carry the gate forward to step 6.
        if let Some(g) = gate {
            if single_plan_gate.is_some() {
                // Belt-and-braces: a commit reaching this branch for >1
                // plan would be the MultiPlan case from step 5, whose
                // kind isn't reviewable — so `gate` should be None here.
                // If we somehow have two gates, drop them rather than
                // pretend the commit has one canonical gate.
                single_plan_gate = None;
            } else {
                single_plan_gate = Some(g);
            }
        }
        let timeline_event = match kind {
            CommitKind::PlanOnly => PlanTimelineEvent::PlanOnly {
                sha,
                author_ts,
                subject,
            },
            CommitKind::CodeOnly => PlanTimelineEvent::CodeOnly {
                sha,
                author_ts,
                subject,
            },
            CommitKind::Mixed => PlanTimelineEvent::Mixed {
                sha,
                author_ts,
                subject,
            },
            CommitKind::MultiPlan => PlanTimelineEvent::MultiPlan {
                sha,
                author_ts,
                subject,
            },
            CommitKind::Finalize => {
                unreachable!("Finalize is handled in step 4; should not reach the per-plan append")
            }
            CommitKind::Unattributed => {
                unreachable!("Unattributed is filtered out of the per-plan classifier")
            }
        };
        plan.timeline.push(timeline_event);
        plans_with_event.insert(plan_key.clone());
        // Record the per-plan kind so step 6 can pick it up for the
        // repo-wide CommitNode. A commit attributed to exactly one
        // active plan via step 5 has a unique kind; for MultiPlan or
        // Finalize-only commits this stays None and the step-6
        // classifier picks the correct repo-wide kind on its own.
        if single_plan_kind.is_none() {
            single_plan_kind = Some((plan_key.clone(), kind));
        } else {
            single_plan_kind = Some((plan_key.clone(), CommitKind::MultiPlan));
        }
    }

    // 6. Build the repo-wide CommitNode. Phase 1 of
    //    `commit-first-review-model`: shadow the per-plan timeline with
    //    one authoritative-per-commit attribution. The existing matcher
    //    still reads `Plan.timeline`; Phase 2 makes this map
    //    authoritative.
    let commit_node = build_commit_node(
        &commit_sha,
        event,
        &active_changes,
        &attr,
        &plans_with_event,
        &plans_finalized_here,
        single_plan_kind.as_ref(),
        single_plan_gate,
    );
    state.commits.insert(commit_sha.clone(), commit_node);

    // 7. Carry forward.
    carry.previous_commit = Some(commit_sha);
}

/// Build a `CommitNode` for this commit. Attribution priority:
///
/// 1. Multi-plan touch (>= 2 distinct plan files) → `MultiPlan`.
/// 2. Single freeze fire (commit fires the finalize rule for exactly
///    one plan and produces no other per-plan reviewable event) →
///    `Finalize(plan)`.
/// 3. `AttributionResult::Attributed { session, .. }` → `Plan(session)`.
/// 4. Otherwise → `AdHoc` (legacy `Unattributed`).
///
/// The `plans` set is the union of every plan touched, every plan that
/// got a reviewable event, and every plan that froze on this commit.
/// `gate` mirrors the single-plan timeline event when attribution is
/// `Plan(_)`; otherwise `None`.
fn build_commit_node(
    commit_sha: &CommitSha,
    event: &CommitEvent,
    active_changes: &CommitChanges,
    attr: &AttributionResult,
    plans_with_event: &BTreeSet<PlanKey>,
    plans_finalized_here: &BTreeSet<PlanKey>,
    single_plan_kind: Option<&(PlanKey, CommitKind)>,
    single_plan_gate: Option<CommitGate>,
) -> CommitNode {
    let touched_plans: BTreeSet<PlanKey> = active_changes
        .plan_touches
        .iter()
        .map(|t| t.session.clone())
        .collect();
    let mut plans: BTreeSet<PlanKey> = BTreeSet::new();
    plans.extend(touched_plans.iter().cloned());
    plans.extend(plans_with_event.iter().cloned());
    plans.extend(plans_finalized_here.iter().cloned());

    let attribution = if touched_plans.len() >= 2 {
        CommitAttribution::MultiPlan {
            plans: touched_plans.clone(),
        }
    } else if plans_finalized_here.len() == 1 && single_plan_kind.is_none() {
        let plan = plans_finalized_here.iter().next().cloned().unwrap();
        CommitAttribution::Finalize { plan }
    } else if let AttributionResult::Attributed { session, .. } = attr {
        CommitAttribution::Plan {
            plan: session.clone(),
        }
    } else {
        CommitAttribution::AdHoc
    };

    let (kind, gate) = match &attribution {
        CommitAttribution::Plan { .. } => {
            let kind = single_plan_kind
                .as_ref()
                .map(|(_, k)| *k)
                .unwrap_or(CommitKind::Unattributed);
            (kind, single_plan_gate)
        }
        CommitAttribution::MultiPlan { .. } => (CommitKind::MultiPlan, None),
        CommitAttribution::Finalize { .. } => (CommitKind::Finalize, None),
        CommitAttribution::AdHoc => (CommitKind::Unattributed, None),
    };

    CommitNode {
        sha: commit_sha.clone(),
        author_ts: event.author_ts,
        subject: event.subject.clone(),
        kind,
        attribution,
        plans,
        gate,
    }
}

fn is_frozen(state: &RepoState, plan_key: &PlanKey) -> bool {
    state
        .plans
        .get(plan_key)
        .map(|p| p.is_frozen())
        .unwrap_or(false)
}

/// True iff the plan's finalize tree contains ≥1 file and every file's
/// first line starts with `APPROVE`. None / empty → false.
fn finalize_rule_satisfied(files: Option<&BTreeMap<String, String>>) -> bool {
    let Some(files) = files else {
        return false;
    };
    !files.is_empty()
        && files
            .values()
            .all(|first_line| finalize_first_line_starts_with_approve(first_line))
}

/// Compose a single commit's `CommitGate` from its already-known
/// `commit_feedback` map plus the cumulative `participants` carry.
/// Extends `participants` with any author not already in it, then
/// derives approvers / requesters / ambiguous / missing / state.
///
/// Pure function — no IO, no globals. Used by [`rebuild_plan_gates`]
/// when walking a plan's timeline oldest-to-newest.
fn compose_gate(
    _commit_sha: &CommitSha,
    commit_feedback: BTreeMap<AgentLabel, Feedback>,
    participants: &mut Vec<AgentLabel>,
) -> CommitGate {
    let mut approvers: Vec<AgentLabel> = Vec::new();
    let mut requesters: Vec<AgentLabel> = Vec::new();
    let mut ambiguous: Vec<AgentLabel> = Vec::new();
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
    CommitGate {
        state,
        participants: participants.clone(),
        approvers,
        requesters,
        ambiguous,
        missing,
        feedback: commit_feedback,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attribution::PlanTouch;
    use crate::lifecycle::AgentLabel;
    use crate::repo_state::PlanTouchKind;

    fn sha(s: &str) -> CommitSha {
        CommitSha::parse(s).unwrap_or_else(|e| panic!("invalid test SHA {s:?}: {e}"))
    }

    fn sess(s: &str) -> PlanKey {
        PlanKey::parse(s).unwrap()
    }

    fn intro_with_body(stem: &str, body: &str) -> PlanTouch {
        PlanTouch {
            session: sess(stem),
            kind: PlanTouchKind::Intro,
            new_path: Some(PathBuf::from(format!(".trinity/plans/{stem}.md"))),
            new_body: Some(body.to_string()),
        }
    }

    fn revise_with_body(stem: &str, body: &str) -> PlanTouch {
        PlanTouch {
            session: sess(stem),
            kind: PlanTouchKind::Revision,
            new_path: Some(PathBuf::from(format!(".trinity/plans/{stem}.md"))),
            new_body: Some(body.to_string()),
        }
    }

    fn delete_touch(stem: &str) -> PlanTouch {
        PlanTouch {
            session: sess(stem),
            kind: PlanTouchKind::Revision,
            new_path: None,
            new_body: None,
        }
    }

    fn event(commit: &str, touches: Vec<PlanTouch>, has_code: bool) -> CommitEvent {
        CommitEvent {
            commit: sha(commit),
            author_ts: 0,
            subject: String::new(),
            changes: CommitChanges {
                plan_touches: touches,
                has_non_plan_code_changes: has_code,
                finalize_changes: Vec::new(),
                trinity_paths: Vec::new(),
                touched_trinity: false,
                trinity_paths_touched: Vec::new(),
            },
        }
    }

    fn event_with_finalize(
        commit: &str,
        touches: Vec<PlanTouch>,
        finalize: Vec<crate::attribution::FinalizeChange>,
    ) -> CommitEvent {
        CommitEvent {
            commit: sha(commit),
            author_ts: 0,
            subject: String::new(),
            changes: CommitChanges {
                plan_touches: touches,
                has_non_plan_code_changes: false,
                finalize_changes: finalize,
                trinity_paths: Vec::new(),
                touched_trinity: false,
                trinity_paths_touched: Vec::new(),
            },
        }
    }

    fn upsert(stem: &str, file: &str, first_line: &str) -> crate::attribution::FinalizeChange {
        crate::attribution::FinalizeChange {
            plan_key: sess(stem),
            file_name: file.to_string(),
            kind: FinalizeChangeKind::Upsert {
                first_line: first_line.to_string(),
            },
        }
    }

    fn feedback(session: &str, target: &str, author: &str, body: &str) -> FeedbackBlob {
        FeedbackBlob {
            abs_path: PathBuf::from(format!(
                "/r/.trinity/feedback/{session}/{target}/{author}.md"
            )),
            parsed: FeedbackPath {
                plan_key: sess(session),
                target_sha: sha(target),
                author: AgentLabel::parse(author).unwrap(),
                raw: PathBuf::from(format!("{session}/{target}/{author}.md")),
            },
            body: body.to_string(),
            created_at: 0,
        }
    }

    fn snap(history: Vec<CommitEvent>) -> CommitSnapshot {
        CommitSnapshot {
            head: history.last().map(|e| e.commit.clone()),
            history,
        }
    }

    /// Test helper: replay the full pipeline (base fold + live
    /// feedback attach) and return the underlying `RepoState`.
    fn derive_state(repo: PathBuf, snap: CommitSnapshot) -> RepoState {
        attach_live_feedback(derive_base_state(repo, snap), Vec::new()).into_inner()
    }

    /// Test helper with feedback overlay.
    fn derive_state_with_feedback(
        repo: PathBuf,
        snap: CommitSnapshot,
        fb: Vec<FeedbackBlob>,
    ) -> RepoState {
        attach_live_feedback(derive_base_state(repo, snap), fb).into_inner()
    }

    // ============================================================
    // Sequential fold behavior.
    // ============================================================

    #[test]
    fn empty_snapshot_yields_empty_state() {
        let state = derive_state(PathBuf::from("/r"), CommitSnapshot::default());
        assert!(state.plans.is_empty());
        assert!(state.head.is_none());
    }

    #[test]
    fn intro_creates_plan_with_intro_and_parent() {
        let state = derive_state(
            PathBuf::from("/r"),
            snap(vec![
                event("aaaa", vec![], true),
                event("bbbb", vec![intro_with_body("foo", "# foo\n")], false),
            ]),
        );
        let plan = &state.plans[&sess("foo")];
        assert_eq!(plan.body, "# foo\n");
        assert_eq!(plan.plan_intro, sha("bbbb"));
        assert_eq!(plan.plan_intro_parent, Some(sha("aaaa")));
        let shas: Vec<_> = plan.timeline.iter().map(|e| e.sha().clone()).collect();
        assert_eq!(shas, vec![sha("bbbb")]);
        assert!(
            plan.timeline
                .iter()
                .all(|e| matches!(e.kind(), CommitKind::PlanOnly))
        );
    }

    #[test]
    fn revision_updates_body_for_unfrozen_plan() {
        let state = derive_state(
            PathBuf::from("/r"),
            snap(vec![
                event("c1c1", vec![intro_with_body("foo", "# v1\n")], false),
                event("c2c2", vec![revise_with_body("foo", "# v2\n")], false),
            ]),
        );
        let plan = &state.plans[&sess("foo")];
        assert_eq!(plan.body, "# v2\n");
        let shas: Vec<_> = plan.timeline.iter().map(|e| e.sha().clone()).collect();
        assert_eq!(shas, vec![sha("c1c1"), sha("c2c2")]);
    }

    #[test]
    fn impl_commit_attributes_via_walkback() {
        let state = derive_state(
            PathBuf::from("/r"),
            snap(vec![
                event("c1c1", vec![intro_with_body("foo", "# foo\n")], false),
                event("c2c2", vec![], true),
            ]),
        );
        let plan = &state.plans[&sess("foo")];
        let impls: Vec<_> = plan
            .timeline
            .iter()
            .filter(|e| matches!(e.kind(), CommitKind::CodeOnly | CommitKind::Mixed))
            .map(|e| e.sha().clone())
            .collect();
        assert_eq!(impls, vec![sha("c2c2")]);
    }

    #[test]
    fn multi_plan_commit_is_unattributed_for_walkback() {
        // c2 touches both a and b → MultiPlan for each; c3 walks back
        // through c2 transparently and attributes to a (the earliest
        // single-plan ancestor's session via current_effective).
        let state = derive_state(
            PathBuf::from("/r"),
            snap(vec![
                event("c1c1", vec![intro_with_body("a", "# a\n")], false),
                event(
                    "c2c2",
                    vec![
                        revise_with_body("a", "# a v2\n"),
                        intro_with_body("b", "# b\n"),
                    ],
                    false,
                ),
                event("c3c3", vec![], true),
            ]),
        );
        let a = &state.plans[&sess("a")];
        let b = &state.plans[&sess("b")];
        // c2 appears in both plans' timelines, as MultiPlan (no gate).
        let c2 = sha("c2c2");
        let a_c2 = a.event_for(&c2).expect("a has c2");
        let b_c2 = b.event_for(&c2).expect("b has c2");
        assert!(matches!(a_c2.kind(), CommitKind::MultiPlan));
        assert!(matches!(b_c2.kind(), CommitKind::MultiPlan));
        assert!(state.gate_for(&c2).is_none());
        // c3 walks back to a (oldest single-plan-touch ancestor).
        let a_c3 = a.event_for(&sha("c3c3")).expect("a has c3");
        assert!(matches!(a_c3.kind(), CommitKind::CodeOnly));
        assert!(b.event_for(&sha("c3c3")).is_none());
    }

    #[test]
    fn delete_unfrozen_plan_removes_it() {
        let state = derive_state(
            PathBuf::from("/r"),
            snap(vec![
                event("c1c1", vec![intro_with_body("foo", "# foo\n")], false),
                event("c2c2", vec![delete_touch("foo")], false),
            ]),
        );
        assert!(!state.plans.contains_key(&sess("foo")));
    }

    #[test]
    fn delete_then_readd_creates_fresh_plan_no_history_leak() {
        let state = derive_state(
            PathBuf::from("/r"),
            snap(vec![
                event("c1c1", vec![intro_with_body("foo", "# v1\n")], false),
                event("c2c2", vec![], true), // impl on the old foo
                event("c3c3", vec![delete_touch("foo")], false),
                event("c4c4", vec![intro_with_body("foo", "# v2 fresh\n")], false),
            ]),
        );
        let plan = &state.plans[&sess("foo")];
        assert_eq!(plan.body, "# v2 fresh\n");
        assert_eq!(plan.plan_intro, sha("c4c4"));
        // The fresh foo's timeline only contains c4 — c1 belongs
        // to the deleted-and-gone old foo.
        let shas: Vec<_> = plan.timeline.iter().map(|e| e.sha().clone()).collect();
        assert_eq!(shas, vec![sha("c4c4")]);
        assert!(
            plan.timeline
                .iter()
                .all(|e| !matches!(e.kind(), CommitKind::CodeOnly | CommitKind::Mixed))
        );
    }

    // ============================================================
    // Freeze rule.
    // ============================================================

    #[test]
    fn freeze_with_one_approve() {
        let state = derive_state(
            PathBuf::from("/r"),
            snap(vec![
                event("c1c1", vec![intro_with_body("foo", "# foo\n")], false),
                event_with_finalize("c2c2", vec![], vec![upsert("foo", "alice.md", "APPROVE")]),
            ]),
        );
        let plan = &state.plans[&sess("foo")];
        assert_eq!(plan.frozen_at(), Some(&sha("c2c2")));
        assert_eq!(plan.body, "# foo\n");
    }

    #[test]
    fn mixed_verdict_does_not_freeze() {
        let state = derive_state(
            PathBuf::from("/r"),
            snap(vec![
                event("c1c1", vec![intro_with_body("foo", "# foo\n")], false),
                event_with_finalize(
                    "c2c2",
                    vec![],
                    vec![
                        upsert("foo", "alice.md", "APPROVE"),
                        upsert("foo", "bob.md", "REQUEST_CHANGES"),
                    ],
                ),
            ]),
        );
        assert!(state.plans[&sess("foo")].frozen_at().is_none());
    }

    #[test]
    fn frozen_plan_body_captured_at_freeze_commit() {
        // c1: body "v1". c2: revise to "v2". c3: finalize (freezes at v2). c4: revise (frozen, no effect on body).
        let state = derive_state(
            PathBuf::from("/r"),
            snap(vec![
                event("c1c1", vec![intro_with_body("foo", "# v1\n")], false),
                event("c2c2", vec![revise_with_body("foo", "# v2\n")], false),
                event_with_finalize("c3c3", vec![], vec![upsert("foo", "alice.md", "APPROVE")]),
                event(
                    "c4c4",
                    vec![revise_with_body("foo", "# v3 ignored\n")],
                    false,
                ),
            ]),
        );
        let plan = &state.plans[&sess("foo")];
        assert_eq!(plan.frozen_at(), Some(&sha("c3c3")));
        assert_eq!(plan.body, "# v2\n");
    }

    #[test]
    fn frozen_plan_survives_plan_file_deletion() {
        // Monotone-finished: delete after freeze → plan stays.
        let state = derive_state(
            PathBuf::from("/r"),
            snap(vec![
                event("c1c1", vec![intro_with_body("foo", "# foo\n")], false),
                event_with_finalize("c2c2", vec![], vec![upsert("foo", "alice.md", "APPROVE")]),
                event("c3c3", vec![delete_touch("foo")], false),
            ]),
        );
        let plan = &state.plans[&sess("foo")];
        assert_eq!(plan.frozen_at(), Some(&sha("c2c2")));
        assert_eq!(plan.body, "# foo\n");
    }

    #[test]
    fn request_changes_in_finished_then_delete_no_ghost() {
        // REQUEST_CHANGES never freezes; delete then has no anchor → plan absent.
        let state = derive_state(
            PathBuf::from("/r"),
            snap(vec![
                event("c1c1", vec![intro_with_body("foo", "# foo\n")], false),
                event_with_finalize(
                    "c2c2",
                    vec![],
                    vec![upsert("foo", "alice.md", "REQUEST_CHANGES")],
                ),
                event("c3c3", vec![delete_touch("foo")], false),
            ]),
        );
        assert!(!state.plans.contains_key(&sess("foo")));
    }

    // ============================================================
    // Feedback / gates.
    // ============================================================

    #[test]
    fn feedback_lands_in_gate_for_reviewable_commit() {
        let state = derive_state_with_feedback(
            PathBuf::from("/r"),
            snap(vec![event(
                "c1c1",
                vec![intro_with_body("foo", "# foo\n")],
                false,
            )]),
            vec![feedback("foo", "c1c1", "alice", "APPROVE\n")],
        );
        let plan = &state.plans[&sess("foo")];
        let _event = plan.event_for(&sha("c1c1")).expect("event for c1");
        let gate = state.gate_for(&sha("c1c1")).expect("gate for c1");
        let entry = gate
            .feedback
            .get(&AgentLabel::parse("alice").unwrap())
            .unwrap();
        assert_eq!(entry.verdict, crate::repo_state::Verdict::Approve);
        assert_eq!(
            plan.latest_reviewable_event().map(|e| e.sha().clone()),
            Some(sha("c1c1"))
        );
    }

    #[test]
    fn finalize_commit_appears_as_visible_non_reviewable_timeline_event() {
        // Codex regression: finalize is a lifecycle marker, not a
        // review target. After a finalize-only commit, the plan must
        // (a) have frozen_at set, (b) carry a Finalize timeline
        // event for the freeze commit, (c) leave that event's gate
        // as None, and (d) keep `latest_reviewable_event` pointing
        // at the prior reviewable commit (not the freeze).
        let state = derive_state(
            PathBuf::from("/r"),
            snap(vec![
                event("c1c1", vec![intro_with_body("foo", "# foo\n")], false),
                event_with_finalize("c2c2", vec![], vec![upsert("foo", "alice.md", "APPROVE")]),
            ]),
        );
        let plan = &state.plans[&sess("foo")];
        assert_eq!(plan.frozen_at(), Some(&sha("c2c2")));
        let kinds: Vec<_> = plan
            .timeline
            .iter()
            .map(|e| (e.sha().clone(), e.kind()))
            .collect();
        assert_eq!(
            kinds,
            vec![
                (sha("c1c1"), CommitKind::PlanOnly),
                (sha("c2c2"), CommitKind::Finalize),
            ]
        );
        let _finalize_event = plan.event_for(&sha("c2c2")).expect("c2 on timeline");
        assert!(
            state.gate_for(&sha("c2c2")).is_none(),
            "Finalize must not carry a gate"
        );
        assert_eq!(
            plan.latest_reviewable_event().map(|e| e.sha().clone()),
            Some(sha("c1c1")),
            "latest reviewable skips Finalize"
        );
    }

    #[test]
    fn finalize_commit_with_bundled_code_change_does_not_emit_codeonly_event() {
        // Pin-down for ruthless review §1: a single commit that
        // BOTH lands `.trinity/finished/foo/...` AND modifies code
        // is fully absorbed into the Finalize event. The bundled
        // code changes do not surface as a `CodeOnly` event on
        // foo's timeline. This is deliberate: post-freeze the plan
        // is sealed end-to-end and cannot accumulate further
        // reviewable work, even on the freeze commit itself.
        //
        // Trinity does not enforce that finalize commits touch only
        // `.trinity/finished/`; producing clean finalize commits is
        // the responsibility of whatever tool writes the finalize
        // (see `trinity-cli` stub in the plan). If this test starts
        // failing, the architectural decision has changed — update
        // the plan §"Finalize commit" before "fixing" the test.
        let state = derive_state(
            PathBuf::from("/r"),
            snap(vec![
                event("c1c1", vec![intro_with_body("foo", "# foo\n")], false),
                CommitEvent {
                    commit: sha("c2c2"),
                    author_ts: 0,
                    subject: String::new(),
                    changes: CommitChanges {
                        plan_touches: vec![],
                        has_non_plan_code_changes: true,
                        finalize_changes: vec![upsert("foo", "alice.md", "APPROVE")],
                        trinity_paths: Vec::new(),
                        touched_trinity: false,
                        trinity_paths_touched: Vec::new(),
                    },
                },
            ]),
        );
        let plan = &state.plans[&sess("foo")];
        let c2 = plan.event_for(&sha("c2c2")).expect("c2 on timeline");
        assert!(
            matches!(c2.kind(), CommitKind::Finalize),
            "freeze commit must be Finalize, not CodeOnly: {:?}",
            c2.kind()
        );
        assert!(
            state.gate_for(&sha("c2c2")).is_none(),
            "Finalize must not carry a gate even when bundled with code changes"
        );
        assert!(
            !plan
                .timeline
                .iter()
                .any(|e| matches!(e.kind(), CommitKind::CodeOnly)),
            "bundled code changes must not produce a CodeOnly event on a frozen plan"
        );
    }

    #[test]
    fn finalize_commit_bundled_with_plan_body_revision_emits_only_finalize() {
        // Pin-down for ruthless review §2: a single commit that
        // revises plan body AND freezes produces ONE Finalize
        // timeline event for that SHA — not two events (PlanOnly +
        // Finalize). The body change is captured at the freeze
        // moment (per the freeze rule: body = carry.plan_bodies at
        // freeze time), so the semantic content is preserved in
        // `plan.body`. The timeline shape is intentional —
        // `event_for(sha)` returns a single event per SHA, so the
        // API contract stays simple.
        let state = derive_state(
            PathBuf::from("/r"),
            snap(vec![
                event("c1c1", vec![intro_with_body("foo", "# v1\n")], false),
                CommitEvent {
                    commit: sha("c2c2"),
                    author_ts: 0,
                    subject: String::new(),
                    changes: CommitChanges {
                        plan_touches: vec![revise_with_body("foo", "# v2 final\n")],
                        has_non_plan_code_changes: false,
                        finalize_changes: vec![upsert("foo", "alice.md", "APPROVE")],
                        trinity_paths: Vec::new(),
                        touched_trinity: false,
                        trinity_paths_touched: Vec::new(),
                    },
                },
            ]),
        );
        let plan = &state.plans[&sess("foo")];
        assert_eq!(plan.body, "# v2 final\n");
        let kinds: Vec<_> = plan
            .timeline
            .iter()
            .map(|e| (e.sha().clone(), e.kind()))
            .collect();
        assert_eq!(
            kinds,
            vec![
                (sha("c1c1"), CommitKind::PlanOnly),
                (sha("c2c2"), CommitKind::Finalize),
            ]
        );
    }

    #[test]
    fn timeline_order_preserves_intro_impl_revision_sequence() {
        // Codex regression: intro (c1) → impl (c2) → revision (c3)
        // must surface in that exact order. The prior split-bucket
        // model with the broken two-pointer merge produced
        // [c1, c3, c2]. The first-class timeline preserves fold
        // order by construction.
        let state = derive_state(
            PathBuf::from("/r"),
            snap(vec![
                event("c1c1", vec![intro_with_body("foo", "# v1\n")], false),
                event("c2c2", vec![], true),
                event("c3c3", vec![revise_with_body("foo", "# v2\n")], false),
            ]),
        );
        let plan = &state.plans[&sess("foo")];
        let shas: Vec<_> = plan.timeline.iter().map(|e| e.sha().clone()).collect();
        assert_eq!(shas, vec![sha("c1c1"), sha("c2c2"), sha("c3c3")]);
    }

    #[test]
    fn frozen_plan_touch_does_not_flip_active_plan_to_multiplan() {
        // Codex regression: a single commit touching both a frozen
        // plan and an active plan should classify the active plan
        // as PlanOnly (sealing hides the frozen plan from per-plan
        // classification). The prior code read raw event.changes,
        // so the active plan got marked MultiPlan.
        //
        // Setup:
        //   c1: intro `frozen` → freeze it at c2.
        //   c3: touches both `frozen` (no-op, sealed) and `active` (intro).
        //   For `active`, kind must be PlanOnly (not MultiPlan).
        let state = derive_state(
            PathBuf::from("/r"),
            snap(vec![
                event("c1c1", vec![intro_with_body("frozen", "# frozen\n")], false),
                event_with_finalize(
                    "c2c2",
                    vec![],
                    vec![upsert("frozen", "alice.md", "APPROVE")],
                ),
                event(
                    "c3c3",
                    vec![
                        revise_with_body("frozen", "# ignored\n"),
                        intro_with_body("active", "# active\n"),
                    ],
                    false,
                ),
            ]),
        );
        let active = &state.plans[&sess("active")];
        let c3 = active.event_for(&sha("c3c3")).expect("active has c3");
        assert!(
            matches!(c3.kind(), CommitKind::PlanOnly),
            "expected PlanOnly, got {:?}",
            c3.kind()
        );
        assert!(
            state.gate_for(&sha("c3c3")).is_some(),
            "reviewable PlanOnly must have a gate"
        );
    }

    #[test]
    fn last_activity_ts_tracks_max_author_ts() {
        let event_at = |sha_str: &str, ts: i64| CommitEvent {
            commit: sha(sha_str),
            author_ts: ts,
            subject: String::new(),
            changes: CommitChanges {
                plan_touches: vec![intro_with_body("foo", "# foo\n")],
                has_non_plan_code_changes: false,
                finalize_changes: Vec::new(),
                trinity_paths: Vec::new(),
                touched_trinity: false,
                trinity_paths_touched: Vec::new(),
            },
        };
        let state = derive_state(PathBuf::from("/r"), snap(vec![event_at("c1c1", 100)]));
        let plan = &state.plans[&sess("foo")];
        assert_eq!(plan.last_activity_ts, 100);
    }

    /// `wasm-markdown-rendering.md` Phase 2 made `Feedback` carry
    /// an `author` field redundant with the `CommitGate.feedback`
    /// map key. Pin the invariant at the fold/ingest boundary:
    /// every key equals its value's `author`. If a future writer
    /// drifts the redundancy, this regression catches it before
    /// projection / wire code starts trusting the wrong half.
    #[test]
    fn commit_gate_feedback_key_matches_value_author() {
        let state = derive_state_with_feedback(
            PathBuf::from("/r"),
            snap(vec![event(
                "c1c1",
                vec![intro_with_body("foo", "# foo\n")],
                false,
            )]),
            vec![
                feedback("foo", "c1c1", "alice", "APPROVE\n\nlgtm\n"),
                feedback("foo", "c1c1", "bob", "REQUEST_CHANGES\n\nbug\n"),
            ],
        );
        let plan = &state.plans[&sess("foo")];
        let gate = plan
            .timeline
            .iter()
            .find_map(|e| state.gate_for(e.sha()))
            .expect("foo's intro commit must carry a gate after fold");
        assert!(!gate.feedback.is_empty(), "gate should have feedback");
        for (key, value) in &gate.feedback {
            assert_eq!(
                key, &value.author,
                "CommitGate.feedback invariant: map key must equal value.author"
            );
        }
    }

    // ============================================================
    // Phase 1 invariants for `.trinity/plans/cache-core-fold-and-
    // live-feedback.md`. Pin the boundary the cache will rely on.
    // ============================================================

    /// Test 1: `derive_base_state` is feedback-blind. The same
    /// `CommitSnapshot` produces a `BaseRepoState` with empty,
    /// `Unreviewed` gates regardless of what feedback exists on
    /// disk — because it never reads disk.
    #[test]
    fn derive_base_state_is_feedback_blind() {
        let snap = snap(vec![event(
            "c1c1",
            vec![intro_with_body("foo", "# foo\n")],
            false,
        )]);
        let base = derive_base_state(PathBuf::from("/r"), snap.clone());
        let plan = &base.plans[&sess("foo")];
        let _intro = plan.event_for(&sha("c1c1")).expect("intro event");
        let gate = base.gate_for(&sha("c1c1")).expect("intro is reviewable");
        assert_eq!(gate.state, CommitGateState::Unreviewed);
        assert!(gate.feedback.is_empty());
        assert!(gate.participants.is_empty());
        assert!(gate.missing.is_empty());
    }

    /// Test 3: cumulative participants survive the split. Feedback
    /// on commit A from `alice` must mark her as missing on a later
    /// reviewable commit B in the same plan. This is the
    /// architectural invariant `rebuild_plan_gates` exists to
    /// preserve; patching only the target gate would lose it.
    #[test]
    fn cumulative_participants_propagate_across_reviewable_events() {
        // Two reviewable commits on plan `foo`: c1 (intro, PlanOnly)
        // and c2 (Mixed: revision + code).
        let history = vec![
            event("c1c1", vec![intro_with_body("foo", "# v1\n")], false),
            event("c2c2", vec![revise_with_body("foo", "# v2\n")], true),
        ];
        // Feedback only on c1.
        let fb = vec![feedback("foo", "c1c1", "alice", "APPROVE\n")];
        let state = derive_state_with_feedback(PathBuf::from("/r"), snap(history), fb);
        let plan = &state.plans[&sess("foo")];

        let _c2 = plan.event_for(&sha("c2c2")).expect("c2 event");
        let gate_c2 = state.gate_for(&sha("c2c2")).expect("c2 is reviewable");
        let alice = AgentLabel::parse("alice").unwrap();
        assert!(
            gate_c2.participants.contains(&alice),
            "alice should be a cumulative participant at c2; got {:?}",
            gate_c2.participants,
        );
        assert!(
            gate_c2.missing.contains(&alice),
            "alice voted on c1 but not c2 → must be `missing` at c2; got {:?}",
            gate_c2.missing,
        );
        assert_eq!(gate_c2.state, CommitGateState::Unreviewed);
    }

    /// Regression for codex's Phase 1 review on 44e5bd0:
    /// `last_activity_ts` must NOT be bumped by a feedback file
    /// whose target SHA is missing from the plan's current timeline
    /// (deleted-then-readded stem, off-chain commit, etc.). Such
    /// feedback is dropped silently — and silently means without
    /// activity-timestamp side effects.
    #[test]
    fn dropped_feedback_does_not_bump_last_activity_ts() {
        // Plan exists with one reviewable intro at c1. Feedback
        // targets c2 (not on the timeline).
        let history = vec![event(
            "c1c1",
            vec![intro_with_body("foo", "# foo\n")],
            false,
        )];
        let mut blob = feedback("foo", "c2c2", "alice", "APPROVE\n");
        blob.created_at = 9_999_999_999; // far future
        let state = derive_state_with_feedback(PathBuf::from("/r"), snap(history), vec![blob]);
        let plan = &state.plans[&sess("foo")];

        // Feedback must not appear in any gate.
        let intro_gate = state.gate_for(&sha("c1c1")).expect("intro gate");
        assert!(
            intro_gate.feedback.is_empty(),
            "feedback for unknown target SHA must not attach",
        );

        // last_activity_ts must be commit-derived (0 here — the
        // fixture used author_ts: 0), NOT the future feedback mtime.
        assert_eq!(
            plan.last_activity_ts, 0,
            "stale feedback must not bump last_activity_ts; got {}",
            plan.last_activity_ts,
        );
    }

    /// Test 5: live feedback targeting a finished plan does not
    /// re-open the lifecycle. Once frozen, the plan is sealed and
    /// `attach_live_feedback` skips it.
    #[test]
    fn finished_plan_ignores_live_feedback() {
        let history = vec![
            event("c1c1", vec![intro_with_body("foo", "# foo\n")], false),
            event_with_finalize(
                "c2c2",
                vec![],
                vec![crate::attribution::FinalizeChange {
                    plan_key: sess("foo"),
                    file_name: "alice.md".into(),
                    kind: crate::attribution::FinalizeChangeKind::Upsert {
                        first_line: "APPROVE".into(),
                    },
                }],
            ),
        ];
        // Late feedback "after" finalize, targeting the intro.
        let fb = vec![feedback("foo", "c1c1", "bob", "REQUEST_CHANGES\n\nlate\n")];
        let state = derive_state_with_feedback(PathBuf::from("/r"), snap(history), fb);
        let plan = &state.plans[&sess("foo")];
        assert!(plan.is_frozen(), "plan should still be frozen");
        let _intro = plan.event_for(&sha("c1c1")).expect("intro event");
        let gate = state.gate_for(&sha("c1c1")).expect("intro is reviewable");
        assert!(
            gate.feedback.is_empty(),
            "frozen plan should not absorb live feedback; got {:?}",
            gate.feedback.keys().collect::<Vec<_>>(),
        );
    }

    // ============================================================
    // Phase 1 (commit-first-review-model): repo-wide CommitNode map.
    // ============================================================

    #[test]
    fn commits_map_records_plan_intro_node() {
        let state = derive_state(
            PathBuf::from("/r"),
            snap(vec![event("c1c1", vec![intro_with_body("foo", "# foo\n")], false)]),
        );
        let node = state.commits.get(&sha("c1c1")).expect("commit node");
        assert!(
            matches!(&node.attribution, CommitAttribution::Plan { plan } if plan == &sess("foo")),
            "intro should be Plan(foo), got {:?}",
            node.attribution
        );
        assert_eq!(node.kind, CommitKind::PlanOnly);
        assert!(node.plans.contains(&sess("foo")));
        assert!(node.gate.is_some(), "PlanOnly is reviewable; gate expected");
    }

    #[test]
    fn commits_map_records_codeonly_as_plan_via_walkback() {
        let state = derive_state(
            PathBuf::from("/r"),
            snap(vec![
                event("c1c1", vec![intro_with_body("foo", "# foo\n")], false),
                event("c2c2", vec![], true),
            ]),
        );
        let node = state.commits.get(&sha("c2c2")).expect("c2c2 node");
        assert!(
            matches!(&node.attribution, CommitAttribution::Plan { plan } if plan == &sess("foo")),
            "code-only walkback should attribute to active plan; got {:?}",
            node.attribution
        );
        assert_eq!(node.kind, CommitKind::CodeOnly);
        assert!(node.gate.is_some());
    }

    #[test]
    fn commits_map_records_ad_hoc_when_no_plan_context() {
        let state = derive_state(
            PathBuf::from("/r"),
            snap(vec![event("c1c1", vec![], true)]),
        );
        let node = state.commits.get(&sha("c1c1")).expect("c1c1 node");
        assert!(
            matches!(node.attribution, CommitAttribution::AdHoc),
            "no plan context → AdHoc; got {:?}",
            node.attribution
        );
        assert!(node.plans.is_empty());
        assert!(node.gate.is_none(), "AdHoc commits are non-reviewable until Phase 4");
    }

    #[test]
    fn commits_map_records_multi_plan_touch() {
        let state = derive_state(
            PathBuf::from("/r"),
            snap(vec![
                event("c1c1", vec![intro_with_body("foo", "# foo\n")], false),
                event("c2c2", vec![intro_with_body("bar", "# bar\n")], false),
                event(
                    "c3c3",
                    vec![
                        revise_with_body("foo", "# foo v2\n"),
                        revise_with_body("bar", "# bar v2\n"),
                    ],
                    false,
                ),
            ]),
        );
        let node = state.commits.get(&sha("c3c3")).expect("c3c3 node");
        match &node.attribution {
            CommitAttribution::MultiPlan { plans } => {
                assert!(plans.contains(&sess("foo")));
                assert!(plans.contains(&sess("bar")));
                assert_eq!(plans.len(), 2);
            }
            other => panic!("expected MultiPlan, got {other:?}"),
        }
        assert_eq!(node.kind, CommitKind::MultiPlan);
        assert!(node.gate.is_none());
    }

    #[test]
    fn commits_map_records_finalize_attribution() {
        // intro foo, then finalize commit lands an APPROVE file under
        // .trinity/finished/foo/. The finalize commit has no plan_touch
        // and no code change, so attribution-classify returns
        // Unattributed via walk-back stop — but plans_finalized_here
        // catches it and Finalize wins.
        let history = vec![
            event("c1c1", vec![intro_with_body("foo", "# foo\n")], false),
            event_with_finalize("c2c2", vec![], vec![upsert("foo", "alice.md", "APPROVE")]),
        ];
        let state = derive_state(PathBuf::from("/r"), snap(history));
        let node = state.commits.get(&sha("c2c2")).expect("c2c2 node");
        match &node.attribution {
            CommitAttribution::Finalize { plan } => assert_eq!(plan, &sess("foo")),
            other => panic!("expected Finalize(foo), got {other:?}"),
        }
        assert_eq!(node.kind, CommitKind::Finalize);
        assert!(node.gate.is_none());
        assert!(node.plans.contains(&sess("foo")));
    }
}
