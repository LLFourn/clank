//! The sans-IO boundary for repo-state derivation.
//!
//! `DiskSnapshot` is a list of per-commit events along HEAD's
//! first-parent chain plus the working-tree feedback files (which
//! aren't in git). Each `CommitEvent` carries everything the fold
//! needs from that commit's diff: plan touches with bodies, finalize
//! changes with first lines, code-change flag, author timestamp,
//! subject.
//!
//! `derive_state` is a pure function: start with empty `RepoState`,
//! apply each `CommitEvent` in order, then apply feedback by target
//! SHA. State at the end represents the repo: which plans exist
//! (currently in tree or ever-frozen), what they say, who's waiting
//! on what. No HEAD-tree side data, no `--all` queries, no
//! topological `parent_of` calls — the chronological fold IS the
//! lifecycle.
//!
//! The IO layer (`git_io::snapshot`) builds the `DiskSnapshot` by
//! running one `git log --first-parent` and per-commit `git diff-tree`
//! + `git show` reads. `rebuild::rebuild_repo` glues the two.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use crate::attribution::{CommitChanges, FinalizeChangeKind, classify, effective_session};
use crate::disk_format::{FeedbackPath, finalize_first_line_starts_with_approve, parse_verdict};
use crate::lifecycle::{AgentLabel, CommitSha, PlanKey, content_hash};
use crate::repo_state::{
    AttributionResult, CommitKind, Feedback, Plan, PlanTimelineEvent, PlanTouchKind, RepoState,
    Verdict,
};
use crate::review_state::{CommitGate, CommitGateState};

/// Everything Trinity needs to derive a repo's state, materialized
/// from a single sequential walk of HEAD's first-parent chain.
/// `git_io::snapshot` builds it; `derive_state` consumes it as a fold.
///
/// Strictly one shape of input: a list of commit events plus the
/// on-disk working-tree feedback. No HEAD-tree side data, no
/// pre-built plan-files snapshot — `derive_state` builds
/// `state.plans` by applying each commit's event in order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DiskSnapshot {
    pub head: Option<CommitSha>,
    /// First-parent commit chain from the root to HEAD, oldest-first.
    /// Each event carries the per-commit data the fold needs:
    /// structured diff against parent + author timestamp + subject.
    pub history: Vec<CommitEvent>,
    /// Working-tree feedback files (NOT in git). Each is keyed by its
    /// target commit SHA; the fold applies it when it processes that
    /// commit.
    pub feedback_files: Vec<FeedbackBlob>,
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

/// Pure derivation of `RepoState` from a snapshot. No IO.
///
/// Single chronological commit fold: for each commit in
/// `snapshot.history` (oldest-first), update attribution and
/// plan_touches, replay finalize-snapshot changes into a running
/// per-plan tree state, check the finalize rule (and set
/// `frozen_at` monotonically), then build per-plan gate entries
/// applying the working-tree feedback for that commit. Frozen plans
/// are skipped end-to-end for subsequent commits: no new attribution,
/// plan_touches, gates, or lifecycle change.
pub fn derive_state(repo_root: PathBuf, snapshot: DiskSnapshot) -> RepoState {
    let mut state = RepoState::empty(repo_root);
    state.head = snapshot.head;
    let mut carry = FoldCarry::new(snapshot.feedback_files);
    for event in &snapshot.history {
        apply_commit(&mut state, &mut carry, event);
    }
    state
}

/// Transient fold state carried between `apply_commit` calls. Holds
/// just enough to apply the next commit without rebuilding from the
/// past — the running tree state (which plan files exist + their
/// bodies), the running finalize-tree (which APPROVE files each plan
/// has), the walk-back attribution carry, the cumulative gate
/// participants, the pre-indexed working-tree feedback, and the
/// previous commit's SHA (for `plan_intro_parent` of plans
/// introduced at the next commit).
///
/// Constructed by `FoldCarry::new(feedback_files)` (which indexes the
/// on-disk feedback once) and then threaded through every
/// `apply_commit` call. None of these fields belong on `RepoState` —
/// they're scratch state for the fold, not facts about the repo.
pub struct FoldCarry {
    current_effective: Option<PlanKey>,
    plan_in_tree: BTreeSet<PlanKey>,
    plan_bodies: BTreeMap<PlanKey, String>,
    finalize_tree: BTreeMap<PlanKey, BTreeMap<String, String>>,
    gate_participants: BTreeMap<PlanKey, Vec<AgentLabel>>,
    previous_commit: Option<CommitSha>,
    feedback_by_plan: BTreeMap<PlanKey, BTreeMap<(CommitSha, AgentLabel), Feedback>>,
}

impl FoldCarry {
    pub fn new(feedback_files: Vec<FeedbackBlob>) -> Self {
        let mut feedback_by_plan: BTreeMap<PlanKey, BTreeMap<(CommitSha, AgentLabel), Feedback>> =
            BTreeMap::new();
        for fb in feedback_files {
            let verdict = parse_verdict(&fb.body);
            let feedback = Feedback {
                author: fb.parsed.author.clone(),
                verdict,
                body: fb.body,
                path: fb.abs_path.to_string_lossy().into_owned(),
                created_at: fb.created_at,
            };
            feedback_by_plan
                .entry(fb.parsed.plan_key.clone())
                .or_default()
                .insert((fb.parsed.target_sha, feedback.author.clone()), feedback);
        }
        Self {
            current_effective: None,
            plan_in_tree: BTreeSet::new(),
            plan_bodies: BTreeMap::new(),
            finalize_tree: BTreeMap::new(),
            gate_participants: BTreeMap::new(),
            previous_commit: None,
            feedback_by_plan,
        }
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
                carry.gate_participants.remove(&touch.session);
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
                carry
                    .gate_participants
                    .entry(touch.session.clone())
                    .or_default();
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
            let participants = carry.gate_participants.entry(plan_key.clone()).or_default();
            let fb_map = carry.feedback_by_plan.get(plan_key);
            Some(build_gate_step(&commit_sha, fb_map, participants))
        } else {
            None
        };
        let plan = state.plans.get_mut(plan_key).expect("just verified");
        if event.author_ts > plan.last_activity_ts {
            plan.last_activity_ts = event.author_ts;
        }
        if let Some(g) = &gate {
            let feedback_ts = g.feedback.values().map(|f| f.created_at).max().unwrap_or(0);
            if feedback_ts > plan.last_activity_ts {
                plan.last_activity_ts = feedback_ts;
            }
        }
        let sha = commit_sha.clone();
        let author_ts = event.author_ts;
        let subject = event.subject.clone();
        let timeline_event = match kind {
            CommitKind::PlanOnly => PlanTimelineEvent::PlanOnly {
                sha,
                author_ts,
                subject,
                gate: gate.expect("PlanOnly is reviewable and always has a gate"),
            },
            CommitKind::CodeOnly => PlanTimelineEvent::CodeOnly {
                sha,
                author_ts,
                subject,
                gate: gate.expect("CodeOnly is reviewable and always has a gate"),
            },
            CommitKind::Mixed => PlanTimelineEvent::Mixed {
                sha,
                author_ts,
                subject,
                gate: gate.expect("Mixed is reviewable and always has a gate"),
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
    }

    // 6. Carry forward.
    carry.previous_commit = Some(commit_sha);
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

/// Build a single commit's `CommitGate`, extending `participants` with
/// any new authors. Pulls feedback for `commit_sha` from `fb_map`.
fn build_gate_step(
    commit_sha: &CommitSha,
    fb_map: Option<&BTreeMap<(CommitSha, AgentLabel), Feedback>>,
    participants: &mut Vec<AgentLabel>,
) -> CommitGate {
    let mut approvers: Vec<AgentLabel> = Vec::new();
    let mut requesters: Vec<AgentLabel> = Vec::new();
    let mut ambiguous: Vec<AgentLabel> = Vec::new();
    let mut commit_feedback: BTreeMap<AgentLabel, Feedback> = BTreeMap::new();
    if let Some(map) = fb_map {
        for ((target, _key_author), fb) in map {
            if target == commit_sha {
                commit_feedback.insert(fb.author.clone(), fb.clone());
            }
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

    fn snap(history: Vec<CommitEvent>) -> DiskSnapshot {
        DiskSnapshot {
            head: history.last().map(|e| e.commit.clone()),
            history,
            feedback_files: Vec::new(),
        }
    }

    fn snap_with_feedback(history: Vec<CommitEvent>, fb: Vec<FeedbackBlob>) -> DiskSnapshot {
        DiskSnapshot {
            head: history.last().map(|e| e.commit.clone()),
            history,
            feedback_files: fb,
        }
    }

    // ============================================================
    // Sequential fold behavior.
    // ============================================================

    #[test]
    fn empty_snapshot_yields_empty_state() {
        let state = derive_state(PathBuf::from("/r"), DiskSnapshot::default());
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
        assert!(a_c2.gate().is_none());
        assert!(b_c2.gate().is_none());
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
        let state = derive_state(
            PathBuf::from("/r"),
            snap_with_feedback(
                vec![event(
                    "c1c1",
                    vec![intro_with_body("foo", "# foo\n")],
                    false,
                )],
                vec![feedback("foo", "c1c1", "alice", "APPROVE\n")],
            ),
        );
        let plan = &state.plans[&sess("foo")];
        let event = plan.event_for(&sha("c1c1")).expect("event for c1");
        let gate = event.gate().expect("gate for c1");
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
        let finalize_event = plan.event_for(&sha("c2c2")).expect("c2 on timeline");
        assert!(
            finalize_event.gate().is_none(),
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
            c2.gate().is_none(),
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
        assert!(c3.gate().is_some(), "reviewable PlanOnly must have a gate");
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
        let state = derive_state(
            PathBuf::from("/r"),
            snap_with_feedback(
                vec![event(
                    "c1c1",
                    vec![intro_with_body("foo", "# foo\n")],
                    false,
                )],
                vec![
                    feedback("foo", "c1c1", "alice", "APPROVE\n\nlgtm\n"),
                    feedback("foo", "c1c1", "bob", "REQUEST_CHANGES\n\nbug\n"),
                ],
            ),
        );
        let plan = &state.plans[&sess("foo")];
        let gate = plan
            .timeline
            .iter()
            .find_map(|e| e.gate())
            .expect("foo's intro commit must carry a gate after fold");
        assert!(!gate.feedback.is_empty(), "gate should have feedback");
        for (key, value) in &gate.feedback {
            assert_eq!(
                key, &value.author,
                "CommitGate.feedback invariant: map key must equal value.author"
            );
        }
    }
}
