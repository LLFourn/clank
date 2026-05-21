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

use crate::attribution::{CommitChanges, FinalizeChangeKind, classify};
use crate::disk_format::FeedbackTarget;
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
/// 2. Write each feedback entry into the matching commit's gate
///    on `RepoState.commits[target_sha].gate.feedback`. The
///    ownership invariant (`CommitAttribution::Plan(plan)` must
///    match the feedback path's plan_key) is enforced before any
///    mutation; mismatches drop silently.
/// 3. Call [`rebuild_plan_gates`] to recompute every reviewable
///    gate's participants / approvers / requesters / missing /
///    state chronologically over the plan's timeline SHAs.
///    **Cumulative participants propagate forward** — a feedback
///    file on commit A makes the reviewer a participant for every
///    later reviewable gate in that plan.
/// 4. Bump `plan.last_activity_ts` to include feedback mtimes.
///
/// Finished plans are sealed: live feedback targeting their
/// commits is ignored at this stage.
pub fn attach_live_feedback(
    base: BaseRepoState,
    feedback_files: Vec<FeedbackBlob>,
) -> LiveRepoState {
    attach_live_feedback_with_config(base, feedback_files, &crate::cli::config::Config::default())
}

/// Phase 4 of `commit-first-review-model`: the live-feedback
/// overlay is config-aware so ad hoc commit reviewability can be
/// turned off (`force_review_on_misc_commits = false`) and the
/// reviewer set can be pinned via `ad_hoc_reviewers`. The
/// config-less wrapper above uses defaults; the daemon and CLI
/// call this variant directly with a loaded `Config`.
pub fn attach_live_feedback_with_config(
    base: BaseRepoState,
    feedback_files: Vec<FeedbackBlob>,
    config: &crate::cli::config::Config,
) -> LiveRepoState {
    let mut state = base.into_inner();

    // Repo-wide feedback-author set (Q1 of the plan): every label
    // that has authored any feedback on the current branch's
    // history becomes a potential ad hoc reviewer. Built before we
    // partition by target so plan AND ad-hoc feedback both
    // contribute. Excludes ad-hoc commit authors at gate-build
    // time, not here.
    let branch_authors: BTreeSet<AgentLabel> = feedback_files
        .iter()
        .map(|fb| fb.parsed.author.clone())
        .collect();

    // Partition feedback into per-plan groups + a separate ad-hoc
    // bucket. Phase 4: ad hoc paths use `_` as the reserved
    // segment, parsed into `FeedbackTarget::AdHoc`.
    let mut by_plan: BTreeMap<PlanKey, Vec<FeedbackBlob>> = BTreeMap::new();
    let mut ad_hoc: Vec<FeedbackBlob> = Vec::new();
    for fb in feedback_files {
        match &fb.parsed.target {
            FeedbackTarget::Plan(key) => {
                by_plan.entry(key.clone()).or_default().push(fb);
            }
            FeedbackTarget::AdHoc => ad_hoc.push(fb),
        }
    }

    // Build ad hoc commit gates BEFORE writing per-plan feedback
    // so the by-SHA gate exists when an ad hoc feedback file lands
    // on the same SHA later in the walk. Gate construction
    // respects `force_review_on_misc_commits` and the optional
    // `ad_hoc_reviewers` override.
    if config.review.force_review_on_misc_commits {
        let participants = ad_hoc_participants(config, &branch_authors);
        if !participants.is_empty() {
            build_ad_hoc_gates(&mut state, &participants);
        }
    }

    // Write each ad hoc feedback blob into the matching commit's
    // gate. Ownership + reviewability checks happen inline.
    for fb in ad_hoc {
        let Some(node) = state.commits.get_mut(&fb.parsed.target_sha) else {
            continue;
        };
        if !matches!(node.attribution, CommitAttribution::AdHoc) {
            continue;
        }
        let Some(gate) = node.gate.as_mut() else {
            continue;
        };
        let verdict = parse_verdict(&fb.body);
        let feedback = Feedback {
            author: fb.parsed.author.clone(),
            verdict,
            body: fb.body,
            path: fb.abs_path.to_string_lossy().into_owned(),
            created_at: fb.created_at,
        };
        gate.feedback.insert(feedback.author.clone(), feedback);
    }
    // Recompose every ad hoc gate's approvers/requesters/missing/
    // state from its accumulated feedback. Cumulative participants
    // don't apply per-commit for ad hoc — each ad hoc commit is its
    // own review unit — but the role/verdict aggregation still
    // needs to run.
    rebuild_ad_hoc_gates(&mut state);

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
            // The commit's attribution must match the feedback path.
            // Ad hoc feedback was already split off above; this loop
            // only sees `FeedbackTarget::Plan` entries.
            let matches_attribution = matches!(
                (&fb.parsed.target, &node.attribution),
                (
                    FeedbackTarget::Plan(p),
                    CommitAttribution::Plan { plan },
                ) if p == plan
            );
            if !matches_attribution {
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

/// Compute the participant set for ad hoc commit gates. Config
/// override (`ad_hoc_reviewers`) wins; otherwise derive from the
/// repo's branch feedback authors.
fn ad_hoc_participants(
    config: &crate::cli::config::Config,
    branch_authors: &BTreeSet<AgentLabel>,
) -> Vec<AgentLabel> {
    if let Some(explicit) = &config.review.ad_hoc_reviewers {
        return explicit.clone();
    }
    branch_authors.iter().cloned().collect()
}

/// For each `CommitAttribution::AdHoc` commit, install an empty
/// `Unreviewed` gate with the supplied participant set. Existing
/// gates (rare; shouldn't happen for AdHoc post-fold) are left
/// alone so re-runs are idempotent.
fn build_ad_hoc_gates(state: &mut RepoState, participants: &[AgentLabel]) {
    for node in state.commits.values_mut() {
        if !matches!(node.attribution, CommitAttribution::AdHoc) {
            continue;
        }
        if node.gate.is_some() {
            continue;
        }
        node.gate = Some(CommitGate {
            state: CommitGateState::Unreviewed,
            participants: participants.to_vec(),
            approvers: Vec::new(),
            requesters: Vec::new(),
            ambiguous: Vec::new(),
            missing: participants.to_vec(),
            feedback: BTreeMap::new(),
        });
    }
}

/// Recompute one ad hoc commit's gate from its accumulated feedback
/// + the gate's fixed participant set. Unlike per-plan gates, ad
/// hoc commits do NOT carry cumulative participants — each commit
/// is its own review unit.
///
/// **The single source of truth for ad hoc gate recomposition.**
/// Called from both [`attach_live_feedback_with_config`] (bulk
/// rebuild from disk) and the runtime's `upsert_feedback` /
/// `remove_feedback` watcher path (incremental in-memory update).
/// Duplicating the recompose logic between bulk and live paths is
/// exactly how the cache and live daemon would drift — keep one
/// impl, just like `rebuild_plan_gates` for plan commits.
pub fn rebuild_ad_hoc_gate(node: &mut CommitNode) {
    if !matches!(node.attribution, CommitAttribution::AdHoc) {
        return;
    }
    let Some(gate) = node.gate.as_mut() else {
        return;
    };
    let participants = gate.participants.clone();
    let feedback = gate.feedback.clone();
    let mut participants_carry = participants.clone();
    let recomposed = compose_gate(&node.sha, feedback, &mut participants_carry);
    *gate = CommitGate {
        state: recomposed.state,
        participants,
        approvers: recomposed.approvers,
        requesters: recomposed.requesters,
        ambiguous: recomposed.ambiguous,
        missing: recomposed.missing,
        feedback: recomposed.feedback,
    };
}

/// After ad hoc feedback has been written into per-commit gates,
/// recompute every ad hoc commit's gate. Used by the bulk attach
/// path; the live watcher path calls [`rebuild_ad_hoc_gate`] on the
/// single mutated node.
fn rebuild_ad_hoc_gates(state: &mut RepoState) {
    for node in state.commits.values_mut() {
        rebuild_ad_hoc_gate(node);
    }
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
    // current_effective is updated AFTER prefix-aware classification
    // (later in step 5) so explicit `[misc]` / unknown-prefix → AdHoc
    // doesn't seed an incidental file-touched plan as the active
    // chain. Without this, today's raw effective_session would flip
    // current_effective to a plan that the commit explicitly opted
    // out of via prefix — codex on ad9c147.

    // Phase 5 of `commit-first-review-model`: parse the commit-
    // title prefix BEFORE per-plan timeline construction so the
    // prefix drives classification consistently. File-touch /
    // walk-back inference becomes the fallback when no prefix is
    // present.
    let title_prefix = parse_title_prefix(&event.subject);

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

    // 5. Per-plan classification + timeline append. Phase 5 of
    //    `commit-first-review-model` makes the title prefix the
    //    primary signal: prefix-named plans become the attributed
    //    set, file-touch/walk-back is the fallback. The same
    //    classification drives both the per-plan timeline appends
    //    AND CommitNode — no late override.
    let known_plans: BTreeSet<PlanKey> = state.plans.keys().cloned().collect();
    let classification = classify_effective(
        title_prefix.as_ref(),
        &active_changes,
        &attr,
        &plans_finalized_here,
        &known_plans,
    );

    // Drive `current_effective` from the effective classification.
    // Phase 5 of `commit-first-review-model`: the prefix-aware
    // result owns the walk-back chain, not the raw file-touch
    // rule. This is what makes `[misc]` touching plan-bar NOT seed
    // bar as the active plan for unprefixed descendants — codex on
    // ad9c147.
    //
    // - `Plan(plan)` → seed plan as the active chain.
    // - `MultiPlan(_)` → transparent; preserve parent's chain
    //   (matches today's effective_session behavior for multi-touch).
    // - `Finalize(_)` → transparent; finalize doesn't claim future
    //   commits' attribution.
    // - `AdHoc` → preserve parent's chain. An explicit `[misc]` (or
    //   unknown prefix degrading to ad hoc) must not switch the
    //   active plan via incidental file touches.
    carry.current_effective = match &classification.attribution {
        CommitAttribution::Plan { plan } => Some(plan.clone()),
        CommitAttribution::MultiPlan { .. } | CommitAttribution::Finalize { .. } => {
            carry.current_effective.clone()
        }
        CommitAttribution::AdHoc => {
            // When the AdHoc came from an EXPLICIT prefix
            // (`[misc]` or unknown), preserve the parent's chain
            // verbatim — the operator opted out of plan
            // attribution for this one commit and unprefixed
            // descendants should inherit what was active before.
            //
            // When the AdHoc came from genuine no-context
            // inference (no prefix, no touched plans, no parent
            // effective), the walk-back chain stays empty —
            // effective_session would return None too. Either
            // way, preserve the parent's value.
            carry.current_effective.clone()
        }
    };

    let mut plans_with_event: BTreeSet<PlanKey> = plans_finalized_here.clone();
    let mut single_plan_kind: Option<(PlanKey, CommitKind)> = None;
    let mut single_plan_gate: Option<CommitGate> = None;

    for plan_key in &classification.plans_in_scope {
        if is_frozen(state, plan_key) {
            continue;
        }
        if !state.plans.contains_key(plan_key) {
            // Prefix named a known plan whose entry was deleted in
            // this commit (rare). Skip silently — the timeline
            // anchor is gone.
            continue;
        }
        let our_touch = active_changes
            .plan_touches
            .iter()
            .find(|t| &t.session == plan_key);
        let kind = per_plan_kind_for(
            &classification.attribution,
            plan_key,
            our_touch,
            active_changes.has_non_plan_code_changes,
        );
        if matches!(kind, CommitKind::Unattributed) {
            continue;
        }
        let gate = if kind.is_reviewable() {
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
        let sha = commit_sha.clone();
        let author_ts = event.author_ts;
        let subject = event.subject.clone();
        if let Some(g) = gate {
            if single_plan_gate.is_some() {
                // Defensive: with a single-plan attribution, only one
                // gate should be produced. If we hit two (shouldn't
                // happen post-Phase 5 because plans_in_scope is
                // {plan_x} for Plan(plan_x)), drop both rather than
                // claim a canonical gate.
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
                unreachable!("Unattributed is filtered out by per_plan_kind_for")
            }
        };
        plan.timeline.push(timeline_event);
        plans_with_event.insert(plan_key.clone());
        if single_plan_kind.is_none() {
            single_plan_kind = Some((plan_key.clone(), kind));
        } else {
            single_plan_kind = Some((plan_key.clone(), CommitKind::MultiPlan));
        }
    }

    // 6. Build the repo-wide CommitNode from the same effective
    //    classification used for per-plan timeline appends. No late
    //    override.
    let kind_for_node = match &classification.attribution {
        CommitAttribution::Plan { .. } => single_plan_kind
            .as_ref()
            .map(|(_, k)| *k)
            .unwrap_or(CommitKind::Unattributed),
        CommitAttribution::MultiPlan { .. } => CommitKind::MultiPlan,
        CommitAttribution::Finalize { .. } => CommitKind::Finalize,
        CommitAttribution::AdHoc => CommitKind::Unattributed,
    };
    let gate_for_node = match &classification.attribution {
        CommitAttribution::Plan { .. } => single_plan_gate,
        _ => None,
    };
    let mut commit_plans: BTreeSet<PlanKey> = BTreeSet::new();
    commit_plans.extend(plans_with_event.iter().cloned());
    commit_plans.extend(plans_finalized_here.iter().cloned());
    // Plan-touched files always appear in CommitNode.plans even
    // when the prefix excludes them from attribution (e.g.
    // `[misc]` touching plan-foo.md still means foo's body
    // tracked the new content in step 2 — record the touch).
    for touch in &active_changes.plan_touches {
        commit_plans.insert(touch.session.clone());
    }
    let commit_node = CommitNode {
        sha: commit_sha.clone(),
        author_ts: event.author_ts,
        subject: event.subject.clone(),
        kind: kind_for_node,
        attribution: classification.attribution.clone(),
        plans: commit_plans,
        gate: gate_for_node,
        attribution_warning: classification.warning,
    };
    state.commits.insert(commit_sha.clone(), commit_node);
    state.commit_order.push(commit_sha.clone());

    // ============================================================
    // New sans-io fold (parallel to the legacy fold above). Per
    // core-state-rewrite.md Phase 1 — populate
    // `state.fold: trinity_core::repo_state::RepoState` from the
    // same CommitEvent. Subsequent commits migrate consumers off
    // the legacy fields and onto `state.fold`, after which the
    // legacy fold above gets deleted.
    apply_commit_new_fold(state, event, &plans_finalized_here);

    // 7. Carry forward.
    carry.previous_commit = Some(commit_sha);
}

/// New sans-io fold step. Builds a `trinity_core::repo_state::CommitEvent`
/// from the daemon's `CommitEvent` + the freeze decision already
/// computed by the legacy fold, then calls
/// `state.fold.apply_commit`. The freeze decision rides through
/// `plans_finalized_here` — that set was computed against the legacy
/// fold's tree state, but mathematically equals the parent-vs-commit
/// tree predicate the plan requires for `newly_finished`.
fn apply_commit_new_fold(
    state: &mut RepoState,
    event: &CommitEvent,
    plans_finalized_here: &BTreeSet<PlanKey>,
) {
    use trinity_core::repo_state as fold;

    let plan_touches: Vec<fold::PlanTouchInput> = event
        .changes
        .plan_touches
        .iter()
        .map(|t| fold::PlanTouchInput {
            plan: t.session.clone(),
            kind: match t.kind {
                PlanTouchKind::Intro => fold::TouchKind::Intro,
                PlanTouchKind::Revision => {
                    if t.new_path.is_none() {
                        fold::TouchKind::Delete
                    } else {
                        fold::TouchKind::Revise
                    }
                }
            },
        })
        .collect();

    let new_event = fold::CommitEvent {
        sha: event.commit.clone(),
        author_ts: event.author_ts,
        subject: event.subject.clone(),
        plan_touches,
        newly_finished: plans_finalized_here.clone(),
        has_code_changes: event.changes.has_non_plan_code_changes,
    };

    state.fold.apply_commit(&new_event);
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
/// Effective classification for one commit, produced once at the
/// top of `apply_commit` and consumed by both per-plan timeline
/// construction and `CommitNode`. Phase 5 architectural fix
/// (codex review on 94f1e5a): the prefix drives this; file-touch
/// and walk-back inference is the fallback. Nothing downstream
/// overrides the result.
struct EffectiveClassification {
    /// The CommitAttribution that goes on CommitNode.
    attribution: CommitAttribution,
    /// Plans whose timeline gets an event for this commit. Empty
    /// for AdHoc; one entry for Plan(_); the full set for
    /// MultiPlan/Finalize.
    plans_in_scope: BTreeSet<PlanKey>,
    /// Optional non-blocking warning attached to
    /// `CommitNode.attribution_warning`. Set when the prefix
    /// named at least one unknown plan and we degraded to AdHoc.
    warning: Option<String>,
}

fn classify_effective(
    prefix: Option<&TitlePrefix>,
    active_changes: &CommitChanges,
    attr: &AttributionResult,
    plans_finalized_here: &BTreeSet<PlanKey>,
    known_plans: &BTreeSet<PlanKey>,
) -> EffectiveClassification {
    // Explicit prefix wins. Falls through to file-touch / walk-back
    // when no prefix is present.
    if let Some(prefix) = prefix {
        match prefix {
            TitlePrefix::Misc => {
                return EffectiveClassification {
                    attribution: CommitAttribution::AdHoc,
                    plans_in_scope: BTreeSet::new(),
                    warning: None,
                };
            }
            TitlePrefix::Plans(names) => {
                let parsed: Vec<Result<PlanKey, String>> = names
                    .iter()
                    .map(|n| PlanKey::parse(n).map_err(|_| n.clone()))
                    .collect();
                let unknown: Vec<String> = parsed
                    .iter()
                    .filter_map(|r| match r {
                        Ok(k) if known_plans.contains(k) => None,
                        Ok(k) => Some(k.as_str().to_string()),
                        Err(s) => Some(s.clone()),
                    })
                    .collect();
                if !unknown.is_empty() {
                    let warning = format!(
                        "commit-title prefix names unknown plan(s): {}; treated as ad hoc — amend to `[misc]` or a known plan name to silence",
                        unknown.join(", ")
                    );
                    return EffectiveClassification {
                        attribution: CommitAttribution::AdHoc,
                        plans_in_scope: BTreeSet::new(),
                        warning: Some(warning),
                    };
                }
                let valid: BTreeSet<PlanKey> = parsed.into_iter().flatten().collect();
                if valid.len() == 1 {
                    let plan = valid.iter().next().cloned().unwrap();
                    return EffectiveClassification {
                        attribution: CommitAttribution::Plan { plan: plan.clone() },
                        plans_in_scope: [plan].into_iter().collect(),
                        warning: None,
                    };
                }
                return EffectiveClassification {
                    attribution: CommitAttribution::MultiPlan {
                        plans: valid.clone(),
                    },
                    plans_in_scope: valid,
                    warning: None,
                };
            }
        }
    }

    // No prefix → fall through to today's file-touch /
    // walk-back inference. Phase 5 of `commit-first-review-model`:
    // commits that infer a plan attribution without an explicit
    // prefix get a missing-prefix warning so non-strict mode can
    // surface "touches plan-a but no `[plan-a]` prefix; consider
    // amending" to the master.
    let touched_plans: BTreeSet<PlanKey> = active_changes
        .plan_touches
        .iter()
        .map(|t| t.session.clone())
        .collect();
    let missing_prefix_warning = |suggested: &str| {
        format!(
            "commit subject missing convention prefix; inferred attribution suggests `{suggested}` — amend the title to silence"
        )
    };
    if touched_plans.len() >= 2 {
        let names: Vec<&str> = touched_plans.iter().map(|p| p.as_str()).collect();
        let suggested = format!("[{}]", names.join(","));
        return EffectiveClassification {
            attribution: CommitAttribution::MultiPlan {
                plans: touched_plans.clone(),
            },
            plans_in_scope: touched_plans,
            warning: Some(missing_prefix_warning(&suggested)),
        };
    }
    // Single-plan freeze (and only freeze) → Finalize.
    if plans_finalized_here.len() == 1 && touched_plans.is_empty() {
        let plan = plans_finalized_here.iter().next().cloned().unwrap();
        return EffectiveClassification {
            attribution: CommitAttribution::Finalize { plan: plan.clone() },
            plans_in_scope: [plan].into_iter().collect(),
            warning: None,
        };
    }
    if let AttributionResult::Attributed { session, .. } = attr {
        let plan = session.clone();
        let suggested = format!("[{}]", plan.as_str());
        return EffectiveClassification {
            attribution: CommitAttribution::Plan { plan: plan.clone() },
            plans_in_scope: [plan].into_iter().collect(),
            warning: Some(missing_prefix_warning(&suggested)),
        };
    }
    EffectiveClassification {
        attribution: CommitAttribution::AdHoc,
        plans_in_scope: BTreeSet::new(),
        warning: None,
    }
}

/// Per-plan kind for a single plan given the effective
/// attribution. The plan is known to be in
/// `classification.plans_in_scope` so we always emit a non-
/// Unattributed kind unless attribution is AdHoc.
fn per_plan_kind_for(
    attribution: &CommitAttribution,
    plan_key: &PlanKey,
    our_touch: Option<&crate::attribution::PlanTouch>,
    has_non_plan_code: bool,
) -> CommitKind {
    match attribution {
        CommitAttribution::MultiPlan { .. } => CommitKind::MultiPlan,
        CommitAttribution::Finalize { plan } if plan == plan_key => CommitKind::Finalize,
        CommitAttribution::Finalize { .. } => CommitKind::Unattributed,
        CommitAttribution::Plan { plan } if plan == plan_key => {
            match (our_touch, has_non_plan_code) {
                (Some(_), true) => CommitKind::Mixed,
                (Some(_), false) => CommitKind::PlanOnly,
                (None, true) => CommitKind::CodeOnly,
                (None, false) => CommitKind::Unattributed,
            }
        }
        CommitAttribution::Plan { .. } => CommitKind::Unattributed,
        CommitAttribution::AdHoc => CommitKind::Unattributed,
    }
}

/// Parsed commit-title prefix per Phase 5 of
/// `commit-first-review-model`. A subject like `[plan-a] doing X`
/// returns `Some(Plans(["plan-a"]))`; `[misc]` returns `Some(Misc)`;
/// anything else returns `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TitlePrefix {
    /// Explicit out-of-plan marker. Overrides file inference.
    Misc,
    /// One or more plan names listed in the prefix. May contain
    /// invalid names; the classifier degrades to AdHoc + warning
    /// when any name doesn't match a known plan.
    Plans(Vec<String>),
}

/// Parse a `[xxx]` prefix off the start of `subject`. Returns
/// `None` if the subject doesn't start with `[`. The prefix's
/// content is comma-split; `[misc]` (case-insensitive) is special.
pub fn parse_title_prefix(subject: &str) -> Option<TitlePrefix> {
    let trimmed = subject.trim_start();
    let rest = trimmed.strip_prefix('[')?;
    let close = rest.find(']')?;
    let inner = &rest[..close];
    if inner.is_empty() {
        return None;
    }
    let names: Vec<String> = inner
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if names.is_empty() {
        return None;
    }
    if names.len() == 1 && names[0].eq_ignore_ascii_case("misc") {
        return Some(TitlePrefix::Misc);
    }
    Some(TitlePrefix::Plans(names))
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
                target: FeedbackTarget::Plan(sess(session)),
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
            snap(vec![event(
                "c1c1",
                vec![intro_with_body("foo", "# foo\n")],
                false,
            )]),
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
        let state = derive_state(PathBuf::from("/r"), snap(vec![event("c1c1", vec![], true)]));
        let node = state.commits.get(&sha("c1c1")).expect("c1c1 node");
        assert!(
            matches!(node.attribution, CommitAttribution::AdHoc),
            "no plan context → AdHoc; got {:?}",
            node.attribution
        );
        assert!(node.plans.is_empty());
        assert!(
            node.gate.is_none(),
            "AdHoc commits are non-reviewable until Phase 4"
        );
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
