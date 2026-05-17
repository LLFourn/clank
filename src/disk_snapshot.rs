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
use crate::disk_format::{
    FeedbackPath, finalize_first_line_starts_with_approve, parse_verdict,
};
use crate::lifecycle::{AgentLabel, CommitSha, PlanKey, content_hash};
use crate::repo_state::{
    AttributionResult, CommitKind, Feedback, Plan, PlanTouchKind, RepoState, Verdict,
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

/// Author timestamp + subject for one commit. Stored on `RepoState`
/// (keyed by SHA) so the projection layer can render timelines and
/// compute `last_activity_ts` without reaching into git. Derived
/// per-commit from `CommitEvent` during the fold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitMetaEntry {
    pub author_ts: i64,
    pub subject: String,
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

    // Pre-index feedback by plan_key. Filtering against state.plans
    // (which doesn't exist yet) is deferred to per-commit application,
    // so we keep ALL parseable feedback here regardless of whether the
    // plan currently exists.
    let mut feedback_by_plan: BTreeMap<PlanKey, BTreeMap<(CommitSha, AgentLabel), Feedback>> =
        BTreeMap::new();
    for fb in snapshot.feedback_files {
        let verdict = parse_verdict(&fb.body);
        feedback_by_plan
            .entry(fb.parsed.plan_key.clone())
            .or_default()
            .insert(
                (fb.parsed.target_sha, fb.parsed.author),
                Feedback {
                    path: fb.abs_path,
                    body: fb.body,
                    verdict,
                    created_at: fb.created_at,
                },
            );
    }

    // Per-plan carry-along state for the fold.
    let mut current_effective: Option<PlanKey> = None;
    // `current_plan_in_tree` mirrors which `.trinity/plans/<stem>.md`
    // blobs exist in the running tree state. Populated from
    // `PlanTouch.new_path`. At fold end it should match HEAD's tree.
    let mut current_plan_in_tree: BTreeSet<PlanKey> = BTreeSet::new();
    // `current_plan_bodies` mirrors the body of each plan file at the
    // running tree state. Populated from `PlanTouch.new_body`. Used at
    // freeze time to capture the plan body from the freeze commit
    // rather than HEAD — monotone-finished semantics.
    let mut current_plan_bodies: BTreeMap<PlanKey, String> = BTreeMap::new();
    let mut finalize_tree: BTreeMap<PlanKey, BTreeMap<String, String>> = BTreeMap::new();
    let mut gate_participants: BTreeMap<PlanKey, Vec<AgentLabel>> = BTreeMap::new();

    for entry in &snapshot.history {
        // Drop any current_effective that points at a now-frozen plan.
        if let Some(k) = &current_effective {
            if is_frozen(&state, k) {
                current_effective = None;
            }
        }

        // Apply commit: attribution + plan_touches (skipping frozen-plan
        // touches under their key). commit_order grows unconditionally.
        let commit_sha = entry.commit.clone();
        state.commit_order.push(commit_sha.clone());
        state.commit_meta.insert(
            commit_sha.clone(),
            CommitMetaEntry {
                author_ts: entry.author_ts,
                subject: entry.subject.clone(),
            },
        );

        let raw_attr = classify(&entry.changes, current_effective.as_ref());
        current_effective = effective_session(&entry.changes, current_effective.as_ref());
        // If raw_attr points at a frozen plan, downgrade to Unattributed —
        // the plan is sealed; post-freeze code commits don't attribute to it.
        let attr = match &raw_attr {
            AttributionResult::Attributed { session, .. } if is_frozen(&state, session) => {
                AttributionResult::Unattributed
            }
            _ => raw_attr,
        };
        // Same filter on current_effective for future inheritance.
        if let Some(k) = &current_effective {
            if is_frozen(&state, k) {
                current_effective = None;
            }
        }
        state.attribution.insert(commit_sha.clone(), attr);

        if !entry.changes.plan_touches.is_empty() {
            let mut filtered: Vec<(PlanKey, PlanTouchKind)> = Vec::new();
            for touch in &entry.changes.plan_touches {
                if !is_frozen(&state, &touch.session) {
                    filtered.push((touch.session.clone(), touch.kind));
                }
                // Mirror tree state from kind / new_path / new_body.
                //
                // Producer rules (`git_io::parse_diff_tree`):
                //   - Real `D` (delete) → kind=Revision, new_path=None.
                //   - Real `A` (add) → kind=Intro, new_path=Some(_).
                //   - Real `M`/`R` (modify/rename) → kind=Revision,
                //     new_path=Some(_).
                //   - `new_body` populated for production paths (real
                //     git fetches it via `show_blob`); synthetic tests
                //     may leave it `None`.
                //
                // A synthetic test fixture passing kind=Intro with
                // new_path=None means "this commit introduces the
                // plan, body/path elided" — treat as a tree insert.
                // Only the Revision + None pair (the real delete
                // signal) removes from the tree.
                let is_delete = matches!(touch.kind, PlanTouchKind::Revision)
                    && touch.new_path.is_none();
                if is_delete {
                    current_plan_in_tree.remove(&touch.session);
                    current_plan_bodies.remove(&touch.session);
                } else {
                    current_plan_in_tree.insert(touch.session.clone());
                    if let Some(body) = &touch.new_body {
                        current_plan_bodies.insert(touch.session.clone(), body.clone());
                    }
                }
                // On a fresh Intro of a stem not yet in state.plans
                // (history-rooted: HEAD's `snapshot.plan_files` didn't
                // include it because the plan was deleted from HEAD by
                // a later commit), create the entry now. `or_insert`
                // is a no-op for HEAD-rooted plans already seeded from
                // `snapshot.plan_files`.
                if matches!(touch.kind, PlanTouchKind::Intro)
                    && touch.new_path.is_some()
                    && !state.plans.contains_key(&touch.session)
                {
                    let plan_path = touch
                        .new_path
                        .clone()
                        .expect("new_path is Some per the match above");
                    let body = current_plan_bodies
                        .get(&touch.session)
                        .cloned()
                        .unwrap_or_default();
                    let body_hash = content_hash(&body);
                    // First-parent parent = the commit just before the
                    // current one in chronological order, which has
                    // already been pushed onto commit_order before this
                    // block runs. `None` for the root commit.
                    let plan_intro_parent =
                        if state.commit_order.len() >= 2 {
                            state
                                .commit_order
                                .get(state.commit_order.len() - 2)
                                .cloned()
                        } else {
                            None
                        };
                    state.plans.insert(
                        touch.session.clone(),
                        Plan {
                            id: touch.session.clone(),
                            plan_path,
                            body,
                            body_hash,
                            plan_intro: commit_sha.clone(),
                            plan_intro_parent,
                            commits: BTreeMap::new(),
                            frozen_at: None,
                            freeze_events: Vec::new(),
                            archived_cycles: Vec::new(),
                        },
                    );
                    gate_participants
                        .entry(touch.session.clone())
                        .or_default();
                }
                // On Revision of a not-yet-frozen plan, keep its body
                // tracking what's in HEAD. Frozen plans are sealed —
                // their body was captured at freeze time.
                if matches!(touch.kind, PlanTouchKind::Revision)
                    && touch.new_path.is_some()
                    && !is_frozen(&state, &touch.session)
                {
                    if let (Some(plan), Some(body)) =
                        (state.plans.get_mut(&touch.session), &touch.new_body)
                    {
                        plan.body = body.clone();
                        plan.body_hash = content_hash(body);
                    }
                }
            }
            if !filtered.is_empty() {
                state.plan_touches.insert(commit_sha.clone(), filtered);
            }
        }

        // Apply finalize_changes to the running per-plan finalize tree.
        // Collect the set of plans potentially affected (so we only check
        // the rule when a touched commit could plausibly have changed it).
        let mut maybe_affected: BTreeSet<PlanKey> = BTreeSet::new();
        for fc in &entry.changes.finalize_changes {
            let files = finalize_tree.entry(fc.plan_key.clone()).or_default();
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
        for touch in &entry.changes.plan_touches {
            maybe_affected.insert(touch.session.clone());
        }

        // Finalize-rule check: for each maybe-affected plan that isn't
        // frozen and whose plan file is currently in the tree, fire if
        // the finalize tree contains ≥1 file and all first lines start
        // with APPROVE. On freeze, capture body from
        // `current_plan_bodies` (the body at this commit's tree).
        for plan_key in &maybe_affected {
            if !state.plans.contains_key(plan_key) {
                continue;
            }
            if is_frozen(&state, plan_key) {
                continue;
            }
            if !current_plan_in_tree.contains(plan_key) {
                continue;
            }
            let files = finalize_tree.get(plan_key);
            if finalize_rule_satisfied(files) {
                let approver_count = files.map(|m| m.len()).unwrap_or(0) as u32;
                let captured_body = current_plan_bodies.get(plan_key).cloned();
                if let Some(plan) = state.plans.get_mut(plan_key) {
                    if let Some(body) = captured_body {
                        plan.body_hash = content_hash(&body);
                        plan.body = body;
                    }
                    plan.frozen_at = Some(commit_sha.clone());
                    plan.freeze_events.push(commit_sha.clone());
                    plan.archived_cycles
                        .push(crate::repo_state::ArchivedCycleSummary {
                            closer: commit_sha.clone(),
                            approver_count,
                        });
                }
            }
        }

        // Per-plan gate carry: extend participants + build gate entry
        // for this commit (if reviewable). Skip frozen plans entirely.
        // Iterates the *current* state.plans keys so plans added
        // mid-fold (history-rooted) participate too.
        let current_plan_keys: Vec<PlanKey> = state.plans.keys().cloned().collect();
        for plan_key in &current_plan_keys {
            if is_frozen(&state, plan_key) {
                continue;
            }
            let kind = crate::projection::commit_kind_for(
                plan_key,
                &commit_sha,
                &state.plan_touches,
                &state.attribution,
            );
            if !matches!(
                kind,
                CommitKind::PlanOnly | CommitKind::CodeOnly | CommitKind::Mixed
            ) {
                continue;
            }
            let participants = gate_participants.entry(plan_key.clone()).or_default();
            let fb_map = feedback_by_plan.get(plan_key);
            let gate = build_gate_step(&commit_sha, fb_map, participants);
            if let Some(plan) = state.plans.get_mut(plan_key) {
                plan.commits.insert(commit_sha.clone(), gate);
            }
        }
    }

    // Post-fold prune: a plan exists iff (a) its plan file is in
    // HEAD's tree OR (b) it was ever frozen. Plans that fail both
    // (Intro'd then deleted without freezing) have no historical
    // anchor and must not surface as ghost active plans.
    let stale_keys: Vec<PlanKey> = state
        .plans
        .iter()
        .filter(|(k, plan)| plan.frozen_at.is_none() && !current_plan_in_tree.contains(*k))
        .map(|(k, _)| k.clone())
        .collect();
    for k in stale_keys {
        state.plans.remove(&k);
    }

    state
}

fn is_frozen(state: &RepoState, plan_key: &PlanKey) -> bool {
    state
        .plans
        .get(plan_key)
        .map(|p| p.frozen_at.is_some())
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
        for ((target, author), fb) in map {
            if target == commit_sha {
                commit_feedback.insert(author.clone(), fb.clone());
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
    use crate::repo_state::{AttributionResult, PlanTouchKind};

    fn sha(s: &str) -> CommitSha {
        CommitSha::parse(s).unwrap_or_else(|e| panic!("invalid test SHA {s:?}: {e}"))
    }

    fn sess(s: &str) -> PlanKey {
        PlanKey::parse(s).unwrap()
    }

    /// Synthetic `PlanTouch` at the active path with no body. Tests
    /// use this when they don't care about the plan body (e.g. they
    /// only assert on attribution / plan_touches). Always at active
    /// path so the fold's prune step doesn't read it as a deletion.
    fn touch(stem: &str, kind: PlanTouchKind) -> PlanTouch {
        PlanTouch {
            session: sess(stem),
            kind,
            new_path: Some(PathBuf::from(format!(".trinity/plans/{stem}.md"))),
            new_body: None,
        }
    }

    /// Synthetic `PlanTouch` at the active path with no body.
    fn touch_at_active(stem: &str, kind: PlanTouchKind) -> PlanTouch {
        PlanTouch {
            session: sess(stem),
            kind,
            new_path: Some(PathBuf::from(format!(".trinity/plans/{stem}.md"))),
            new_body: None,
        }
    }

    /// Synthetic `PlanTouch` that mirrors a real `A` (add) diff: at
    /// the active path with a body. Use at the Intro commit; the fold
    /// inserts the plan into `state.plans` on this touch.
    fn intro_with_body(stem: &str, body: &str) -> PlanTouch {
        PlanTouch {
            session: sess(stem),
            kind: PlanTouchKind::Intro,
            new_path: Some(PathBuf::from(format!(".trinity/plans/{stem}.md"))),
            new_body: Some(body.to_string()),
        }
    }

    fn event_at(
        commit: &str,
        touches: Vec<PlanTouch>,
        code: bool,
        finalize_changes: Vec<crate::attribution::FinalizeChange>,
    ) -> CommitEvent {
        CommitEvent {
            commit: sha(commit),
            author_ts: 0,
            subject: String::new(),
            changes: CommitChanges {
                plan_touches: touches,
                has_non_plan_code_changes: code,
                finalize_changes,
            },
        }
    }

    fn entry(commit: &str, touches: Vec<PlanTouch>, code: bool) -> CommitEvent {
        event_at(commit, touches, code, Vec::new())
    }

    fn finalize_entry(
        commit: &str,
        touches: Vec<PlanTouch>,
        code: bool,
        finalize_changes: Vec<crate::attribution::FinalizeChange>,
    ) -> CommitEvent {
        event_at(commit, touches, code, finalize_changes)
    }

    fn upsert_finalize(
        stem: &str,
        file: &str,
        first_line: &str,
    ) -> crate::attribution::FinalizeChange {
        crate::attribution::FinalizeChange {
            plan_key: sess(stem),
            file_name: file.to_string(),
            kind: FinalizeChangeKind::Upsert {
                first_line: first_line.to_string(),
            },
        }
    }

    fn remove_finalize(stem: &str, file: &str) -> crate::attribution::FinalizeChange {
        crate::attribution::FinalizeChange {
            plan_key: sess(stem),
            file_name: file.to_string(),
            kind: FinalizeChangeKind::Remove,
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

    #[test]
    fn empty_snapshot_yields_empty_state() {
        let state = derive_state(PathBuf::from("/r"), DiskSnapshot::default());
        assert!(state.plans.is_empty());
        assert!(state.attribution.is_empty());
        assert!(state.head.is_none());
    }

    #[test]
    fn single_plan_creates_session_with_hash_and_intro() {
        // Intro at the second commit; plan_intro = 1231,
        // plan_intro_parent = 9991 (the previous entry in
        // first-parent order).
        let snap = DiskSnapshot {
            head: Some(sha("1231")),
            history: vec![
                entry("9991", vec![], true),
                entry("1231", vec![intro_with_body("foo", "# foo\n")], false),
            ],
            feedback_files: vec![],
        };
        let state = derive_state(PathBuf::from("/r"), snap);
        let s = &state.plans[&sess("foo")];
        assert_eq!(s.body, "# foo\n");
        assert_eq!(s.body_hash, content_hash("# foo\n"));
        assert_eq!(s.plan_intro, sha("1231"));
        assert_eq!(s.plan_intro_parent, Some(sha("9991")));
        assert_eq!(s.plan_path, PathBuf::from(".trinity/plans/foo.md"));
    }

    #[test]
    fn linear_history_attributes_per_walk_back_rules() {
        let snap = DiskSnapshot {
            head: Some(sha("c3c3")),
            history: vec![
                entry("c1c1", vec![intro_with_body("foo", "# foo\n")], false),
                entry("c2c2", vec![], true),
                entry("c3c3", vec![], true),
            ],
            feedback_files: vec![],
        };
        let state = derive_state(PathBuf::from("/r"), snap);
        // c1: plan intro for foo
        assert!(matches!(
            state.attribution[&sha("c1c1")],
            AttributionResult::Attributed {
                plan_touch: Some(PlanTouchKind::Intro),
                ..
            }
        ));
        // c2 + c3: walk-back inherit foo
        for c in ["c2c2", "c3c3"] {
            match &state.attribution[&sha(c)] {
                AttributionResult::Attributed {
                    session,
                    plan_touch: None,
                    has_code_changes: true,
                } => assert_eq!(session, &sess("foo")),
                other => panic!("commit {c}: unexpected attribution {other:?}"),
            }
        }
    }

    #[test]
    fn multi_plan_commit_is_unattributed_and_descendants_walk_through() {
        let snap = DiskSnapshot {
            head: Some(sha("c3c3")),
            history: vec![
                entry("c1c1", vec![intro_with_body("a", "# a\n")], false),
                entry(
                    "c2c2",
                    vec![
                        touch("a", PlanTouchKind::Revision),
                        intro_with_body("b", "# b\n"),
                    ],
                    false,
                ),
                entry("c3c3", vec![], true),
            ],
            feedback_files: vec![],
        };
        let state = derive_state(PathBuf::from("/r"), snap);
        // c2 is multi-plan → unattributed.
        assert_eq!(
            state.attribution[&sha("c2c2")],
            AttributionResult::Unattributed
        );
        // c3 walks back through c2 transparently → attributes to a.
        match &state.attribution[&sha("c3c3")] {
            AttributionResult::Attributed {
                session,
                plan_touch: None,
                has_code_changes: true,
            } => assert_eq!(session, &sess("a")),
            other => panic!("c3: unexpected {other:?}"),
        }
    }

    #[test]
    fn multi_plan_touches_still_count_as_each_sessions_plan_revision() {
        let snap = DiskSnapshot {
            head: Some(sha("c3c3")),
            history: vec![
                entry("c1c1", vec![intro_with_body("first-one", "# one\n")], false),
                entry(
                    "c2c2",
                    vec![intro_with_body("active-one", "# active v2\n")],
                    false,
                ),
                entry(
                    "c3c3",
                    vec![
                        touch("first-one", PlanTouchKind::Revision),
                        touch("active-one", PlanTouchKind::Revision),
                    ],
                    false,
                ),
            ],
            feedback_files: vec![],
        };
        let state = derive_state(PathBuf::from("/r"), snap);

        // Implementation attribution is still deliberately absent for
        // multi-plan commits, but each touched plan sees its own lifecycle
        // event for review targets and timeline rendering.
        assert_eq!(
            state.attribution[&sha("c3c3")],
            AttributionResult::Unattributed
        );
        assert_eq!(
            crate::projection::all_plan_revisions(&state.plans[&sess("first-one")], &state),
            vec![sha("c1c1"), sha("c3c3")]
        );
        assert_eq!(
            crate::projection::all_plan_revisions(&state.plans[&sess("active-one")], &state),
            vec![sha("c2c2"), sha("c3c3")]
        );
    }

    #[test]
    fn mixed_commit_carries_both_plan_touch_and_code() {
        let snap = DiskSnapshot {
            head: Some(sha("c1c1")),
            history: vec![entry(
                "c1c1",
                vec![intro_with_body("foo", "# foo\n")],
                true,
            )],
            feedback_files: vec![],
        };
        let state = derive_state(PathBuf::from("/r"), snap);
        match &state.attribution[&sha("c1c1")] {
            AttributionResult::Attributed {
                plan_touch: Some(PlanTouchKind::Intro),
                has_code_changes: true,
                ..
            } => {}
            other => panic!("expected mixed Attributed; got {other:?}"),
        }
    }

    #[test]
    fn feedback_with_target_sha_lands_in_phase_map() {
        let snap = DiskSnapshot {
            head: Some(sha("c1c1")),
            history: vec![entry(
                "c1c1",
                vec![intro_with_body("foo", "# foo\n")],
                false,
            )],
            feedback_files: vec![feedback("foo", "c1c1", "alice", "APPROVE\n")],
        };
        let state = derive_state(PathBuf::from("/r"), snap);
        let gate = state.plans[&sess("foo")]
            .commits
            .get(&sha("c1c1"))
            .expect("gate for c1");
        let entry = gate
            .feedback
            .get(&AgentLabel::parse("alice").unwrap())
            .expect("alice's feedback on c1");
        assert_eq!(entry.verdict, crate::repo_state::Verdict::Approve);
    }

    // (Flat-drop / held-feedback tests removed in phase 2.4 — the
    // parser no longer accepts paths without a target SHA, so the
    // held-feedback queue went with it.)

    #[test]
    fn feedback_for_unknown_session_is_dropped() {
        let snap = DiskSnapshot {
            head: Some(sha("c1c1")),
            history: vec![],
            feedback_files: vec![feedback("ghost", "c1c1", "alice", "APPROVE\n")],
        };
        let state = derive_state(PathBuf::from("/r"), snap);
        assert!(state.plans.is_empty());
    }

    #[test]
    fn root_with_no_plan_touch_is_unattributed() {
        let snap = DiskSnapshot {
            head: Some(sha("c1c1")),
            history: vec![entry("c1c1", vec![], true)],
            feedback_files: vec![],
        };
        let state = derive_state(PathBuf::from("/r"), snap);
        assert_eq!(
            state.attribution[&sha("c1c1")],
            AttributionResult::Unattributed
        );
    }

    // ===== Determinism / digest =====

    #[test]
    fn derive_state_is_deterministic() {
        let snap = full_workflow_snapshot();
        let a = derive_state(PathBuf::from("/r"), snap.clone());
        let b = derive_state(PathBuf::from("/r"), snap);
        assert_eq!(a.digest(), b.digest());
    }

    #[test]
    fn digest_changes_when_head_changes() {
        let base = full_workflow_snapshot();
        let mut alt = base.clone();
        alt.head = Some(sha("dffd"));
        assert_ne!(
            derive_state(PathBuf::from("/r"), base).digest(),
            derive_state(PathBuf::from("/r"), alt).digest()
        );
    }

    #[test]
    fn digest_changes_when_feedback_verdict_changes() {
        let base = full_workflow_snapshot();
        let mut alt = base.clone();
        // Flip alice's APPROVE to REQUEST_CHANGES.
        for fb in alt.feedback_files.iter_mut() {
            if fb.parsed.author.as_str() == "alice" {
                fb.body = "REQUEST_CHANGES\n".to_string();
            }
        }
        assert_ne!(
            derive_state(PathBuf::from("/r"), base).digest(),
            derive_state(PathBuf::from("/r"), alt).digest()
        );
    }

    #[test]
    fn digest_changes_when_attribution_grows() {
        let base = full_workflow_snapshot();
        let mut alt = base.clone();
        alt.history.push(entry("0099", vec![], true));
        assert_ne!(
            derive_state(PathBuf::from("/r"), base).digest(),
            derive_state(PathBuf::from("/r"), alt).digest()
        );
    }

    #[test]
    fn digest_changes_when_a_new_session_lands() {
        let base = full_workflow_snapshot();
        let mut alt = base.clone();
        // Add a new plan via history (its own Intro commit). Pick a
        // SHA that's not already in base's history.
        alt.history.push(entry(
            "b1b1",
            vec![intro_with_body("bar", "# bar\n")],
            false,
        ));
        assert_ne!(
            derive_state(PathBuf::from("/r"), base).digest(),
            derive_state(PathBuf::from("/r"), alt).digest()
        );
    }

    #[test]
    fn digest_stable_across_feedback_insertion_order() {
        // Even if feedback files come in a different order in the snapshot
        // (e.g. directory walk reorders), the digest stays the same because
        // BTreeMap iterates by key.
        let mut a = full_workflow_snapshot();
        let mut b = a.clone();
        b.feedback_files.reverse();
        assert_eq!(
            derive_state(PathBuf::from("/r"), a.clone()).digest(),
            derive_state(PathBuf::from("/r"), b.clone()).digest()
        );
        // Sanity: the two snapshots ARE different inputs (different order).
        a.feedback_files.sort_by(|x, y| x.body.cmp(&y.body));
        b.feedback_files.sort_by(|x, y| x.body.cmp(&y.body));
        assert_eq!(a, b);
    }

    // ===== Cross-cutting workflow scenarios =====

    #[test]
    fn full_workflow_state_shape() {
        let state = derive_state(PathBuf::from("/r"), full_workflow_snapshot());
        // Two sessions: foo (planning with one impl commit on top) and bar.
        // Wait — full_workflow has only foo. Adjust expectations:
        assert!(state.plans.contains_key(&sess("foo")));
        let foo = &state.plans[&sess("foo")];
        // Plan APPROVE from alice on c1, impl REQUEST_CHANGES from bob on c2.
        // Both now stored under Plan.commits keyed by their SHA.
        assert_eq!(foo.commits[&sha("c1c1")].feedback.len(), 1);
        assert_eq!(foo.commits[&sha("c2c2")].feedback.len(), 1);
        // Attribution: c1 = plan_intro for foo, c2 = impl commit for foo.
        assert!(matches!(
            state.attribution[&sha("c1c1")],
            AttributionResult::Attributed {
                plan_touch: Some(PlanTouchKind::Intro),
                ..
            }
        ));
        assert!(matches!(
            state.attribution[&sha("c2c2")],
            AttributionResult::Attributed {
                plan_touch: None,
                has_code_changes: true,
                ..
            }
        ));
    }

    #[test]
    fn stale_feedback_loads_under_old_sha() {
        // Feedback targeting an old plan revision sha is still recorded
        // (it stays attached to its target SHA); the gate logic at the
        // mcp_response layer is what filters it out for the current gate.
        let snap = DiskSnapshot {
            head: Some(sha("c3c3")),
            history: vec![
                entry("c1c1", vec![intro_with_body("foo", "# foo\n")], false),
                entry("c2c2", vec![touch("foo", PlanTouchKind::Revision)], false),
                entry("c3c3", vec![], true),
            ],
            // stale: c2 is the latest plan rev now
            feedback_files: vec![feedback("foo", "c1c1", "alice", "APPROVE\n")],
        };
        let state = derive_state(PathBuf::from("/r"), snap);
        let foo = &state.plans[&sess("foo")];
        // Stale feedback keeps its target SHA — now stored on the
        // gate for c1, not on a hypothetical "latest" gate.
        let stale_gate = foo.commits.get(&sha("c1c1")).expect("gate for c1");
        assert_eq!(stale_gate.feedback.len(), 1);
    }

    #[test]
    fn many_plans_one_repo() {
        let snap = DiskSnapshot {
            head: Some(sha("c3c3")),
            history: vec![
                entry("c1c1", vec![intro_with_body("alpha", "# alpha\n")], false),
                entry("c2c2", vec![intro_with_body("beta", "# beta\n")], false),
                entry("c3c3", vec![], true),
            ],
            feedback_files: vec![],
        };
        let state = derive_state(PathBuf::from("/r"), snap);
        assert_eq!(state.plans.len(), 2);
        // c3 walks back through c2 (beta's intro), so attributes to beta.
        match &state.attribution[&sha("c3c3")] {
            AttributionResult::Attributed { session, .. } => assert_eq!(session, &sess("beta")),
            other => panic!("c3 unexpected: {other:?}"),
        }
    }

    #[test]
    fn finished_plan_keeps_plan_revisions_and_impl_commits() {
        // Historical commits attributed before a finalize commit stay
        // attributed — the freeze seals subsequent commits, not earlier ones.
        let snap = DiskSnapshot {
            head: Some(sha("c4c4")),
            history: vec![
                entry("c1c1", vec![touch_at_active("foo", PlanTouchKind::Intro)], false),
                entry("c2c2", vec![], true),
                entry("c3c3", vec![], true),
                finalize_entry(
                    "c4c4",
                    vec![],
                    false,
                    vec![upsert_finalize("foo", "alice.md", "APPROVE")],
                ),
            ],
            feedback_files: vec![],
        };
        let state = derive_state(PathBuf::from("/r"), snap);
        assert_eq!(state.plans[&sess("foo")].frozen_at, Some(sha("c4c4")));
        let plan_touches: Vec<_> = state
            .attribution
            .values()
            .filter(|a| {
                matches!(
                    a,
                    AttributionResult::Attributed {
                        plan_touch: Some(_),
                        ..
                    }
                )
            })
            .collect();
        assert_eq!(plan_touches.len(), 1, "only the intro");
        let impl_commits: Vec<_> = state
            .attribution
            .values()
            .filter(|a| {
                matches!(
                    a,
                    AttributionResult::Attributed {
                        plan_touch: None,
                        has_code_changes: true,
                        ..
                    }
                )
            })
            .collect();
        assert_eq!(impl_commits.len(), 2);
    }

    #[test]
    fn empty_body_plan_file_still_creates_session() {
        let snap = DiskSnapshot {
            head: Some(sha("c1c1")),
            history: vec![entry("c1c1", vec![intro_with_body("foo", "")], false)],
            feedback_files: vec![],
        };
        let state = derive_state(PathBuf::from("/r"), snap);
        assert!(state.plans.contains_key(&sess("foo")));
        assert_eq!(state.plans[&sess("foo")].body, "");
    }

    #[test]
    fn unattributed_chain_at_head_does_not_break_sessions() {
        // The first few commits aren't attributable (pre-Trinity). When
        // the plan-intro commit lands later, the session still gets
        // attribution for that and subsequent walks.
        let snap = DiskSnapshot {
            head: Some(sha("c4c4")),
            history: vec![
                entry("c1c1", vec![], true),
                entry("c2c2", vec![], true),
                entry("c3c3", vec![intro_with_body("foo", "# foo\n")], false),
                entry("c4c4", vec![], true),
            ],
            feedback_files: vec![],
        };
        let state = derive_state(PathBuf::from("/r"), snap);
        assert_eq!(
            state.attribution[&sha("c1c1")],
            AttributionResult::Unattributed
        );
        assert_eq!(
            state.attribution[&sha("c2c2")],
            AttributionResult::Unattributed
        );
        assert!(matches!(
            state.attribution[&sha("c3c3")],
            AttributionResult::Attributed {
                plan_touch: Some(PlanTouchKind::Intro),
                ..
            }
        ));
        assert!(matches!(
            state.attribution[&sha("c4c4")],
            AttributionResult::Attributed {
                plan_touch: None,
                has_code_changes: true,
                ..
            }
        ));
    }

    // ===== Timeline =====

    #[test]
    fn timeline_unknown_session_is_empty() {
        let state = derive_state(PathBuf::from("/r"), DiskSnapshot::default());
        assert!(state.timeline_for(&sess("missing")).is_empty());
    }

    #[test]
    fn timeline_walks_commits_chronologically() {
        let snap = DiskSnapshot {
            head: Some(sha("c3c3")),
            history: vec![
                entry("c1c1", vec![intro_with_body("foo", "# foo\n")], false),
                entry("c2c2", vec![], true),
                entry("c3c3", vec![], true),
            ],
            feedback_files: vec![],
        };
        let state = derive_state(PathBuf::from("/r"), snap);
        let timeline = state.timeline_for(&sess("foo"));
        // Three commit events, in c1 → c2 → c3 order (not BTreeMap SHA-lex).
        let shas: Vec<CommitSha> = timeline
            .iter()
            .filter_map(|e| match e {
                crate::repo_state::TimelineEvent::Commit { sha, .. } => Some(sha.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(shas, vec![sha("c1c1"), sha("c2c2"), sha("c3c3")]);
    }

    #[test]
    fn timeline_attaches_plan_review_after_target_commit() {
        let snap = DiskSnapshot {
            head: Some(sha("c1c1")),
            history: vec![entry(
                "c1c1",
                vec![intro_with_body("foo", "# foo\n")],
                false,
            )],
            feedback_files: vec![feedback("foo", "c1c1", "alice", "APPROVE\n")],
        };
        let state = derive_state(PathBuf::from("/r"), snap);
        let timeline = state.timeline_for(&sess("foo"));
        assert_eq!(timeline.len(), 2);
        assert!(matches!(
            timeline[0],
            crate::repo_state::TimelineEvent::Commit {
                plan_touch: Some(PlanTouchKind::Intro),
                ..
            }
        ));
        match &timeline[1] {
            crate::repo_state::TimelineEvent::Review {
                author, verdict, ..
            } => {
                assert_eq!(author.as_str(), "alice");
                assert_eq!(*verdict, crate::repo_state::Verdict::Approve);
            }
            other => panic!("expected Review, got {other:?}"),
        }
    }

    #[test]
    fn timeline_attaches_impl_review_after_target_commit() {
        let snap = DiskSnapshot {
            head: Some(sha("c2c2")),
            history: vec![
                entry("c1c1", vec![intro_with_body("foo", "# foo\n")], false),
                entry("c2c2", vec![], true),
            ],
            feedback_files: vec![feedback("foo", "c2c2", "bob", "REQUEST_CHANGES\nproblem\n")],
        };
        let state = derive_state(PathBuf::from("/r"), snap);
        let timeline = state.timeline_for(&sess("foo"));
        // c1 commit, c2 commit, then bob's impl review on c2.
        assert_eq!(timeline.len(), 3);
        match &timeline[2] {
            crate::repo_state::TimelineEvent::Review {
                target,
                author,
                verdict,
            } => {
                assert_eq!(target, &sha("c2c2"));
                assert_eq!(author.as_str(), "bob");
                assert_eq!(*verdict, crate::repo_state::Verdict::RequestChanges);
            }
            other => panic!("expected impl review, got {other:?}"),
        }
    }

    #[test]
    fn timeline_includes_multi_plan_touches_without_code_ownership() {
        // Multi-plan commits remain unattributed for implementation
        // ownership, but each touched plan still gets its plan-touch event
        // in its own timeline.
        let snap = DiskSnapshot {
            head: Some(sha("c3c3")),
            history: vec![
                entry("c1c1", vec![intro_with_body("foo", "# foo\n")], false),
                entry(
                    "c2c2",
                    vec![
                        touch("foo", PlanTouchKind::Revision),
                        intro_with_body("bar", "# bar\n"),
                    ],
                    false,
                ),
                entry("c3c3", vec![], true),
            ],
            feedback_files: vec![],
        };
        let state = derive_state(PathBuf::from("/r"), snap);
        let foo_timeline = state.timeline_for(&sess("foo"));
        let shas: Vec<CommitSha> = foo_timeline
            .iter()
            .filter_map(|e| match e {
                crate::repo_state::TimelineEvent::Commit { sha, .. } => Some(sha.clone()),
                _ => None,
            })
            .collect();
        // c1 (foo intro) yes; c2 (multi-plan foo revision) yes; c3
        // walks through c2 transparently and attributes implementation to
        // foo, so yes.
        assert_eq!(shas, vec![sha("c1c1"), sha("c2c2"), sha("c3c3")]);
    }

    #[test]
    fn timeline_only_includes_target_session() {
        let snap = DiskSnapshot {
            head: Some(sha("c3c3")),
            history: vec![
                entry("c1c1", vec![intro_with_body("a", "# a\n")], false),
                entry("c2c2", vec![intro_with_body("b", "# b\n")], false),
                entry("c3c3", vec![], true),
            ],
            feedback_files: vec![],
        };
        let state = derive_state(PathBuf::from("/r"), snap);
        let a_tl = state.timeline_for(&sess("a"));
        let a_shas: Vec<CommitSha> = a_tl
            .iter()
            .filter_map(|e| match e {
                crate::repo_state::TimelineEvent::Commit { sha, .. } => Some(sha.clone()),
                _ => None,
            })
            .collect();
        let b_tl = state.timeline_for(&sess("b"));
        let b_shas: Vec<CommitSha> = b_tl
            .iter()
            .filter_map(|e| match e {
                crate::repo_state::TimelineEvent::Commit { sha, .. } => Some(sha.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(a_shas, vec![sha("c1c1")]);
        // b owns c2 (intro) and c3 (walks back through b).
        assert_eq!(b_shas, vec![sha("c2c2"), sha("c3c3")]);
    }

    #[test]
    fn timeline_multiple_reviews_on_one_commit() {
        let snap = DiskSnapshot {
            head: Some(sha("c1c1")),
            history: vec![entry(
                "c1c1",
                vec![intro_with_body("foo", "# foo\n")],
                false,
            )],
            feedback_files: vec![
                feedback("foo", "c1c1", "alice", "APPROVE\n"),
                feedback("foo", "c1c1", "bob", "REQUEST_CHANGES\n"),
            ],
        };
        let state = derive_state(PathBuf::from("/r"), snap);
        let timeline = state.timeline_for(&sess("foo"));
        // 1 commit + 2 reviews
        assert_eq!(timeline.len(), 3);
        // Reviews are alphabetical by author (BTreeMap key order).
        let authors: Vec<&str> = timeline
            .iter()
            .filter_map(|e| match e {
                crate::repo_state::TimelineEvent::Review { author, .. } => Some(author.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(authors, vec!["alice", "bob"]);
    }

    // The `same_stem_active_and_done_lands_in_plan_conflicts` test +
    // its `done_plan_file` fixture were retired with the
    // `.trinity/plans/done/` directory in Phase 5
    // (event-log-and-finished). `PlanKey::from_path` now rejects the
    // done/ variant outright.

    /// Reusable fixture: foo plan (plan_intro c1) + one impl commit c2,
    /// plus alice's APPROVE on c1 (plan) and bob's REQUEST_CHANGES on c2 (impl).
    fn full_workflow_snapshot() -> DiskSnapshot {
        DiskSnapshot {
            head: Some(sha("c2c2")),
            history: vec![
                entry("c1c1", vec![intro_with_body("foo", "# foo\n")], false),
                entry("c2c2", vec![], true),
            ],
            feedback_files: vec![
                feedback("foo", "c1c1", "alice", "APPROVE\n"),
                feedback("foo", "c2c2", "bob", "REQUEST_CHANGES\nstuff\n"),
            ],
        }
    }

    // ===================================================================
    // Phase 2: event-log fold + finalize snapshot reader.
    // Tests for the finalize rule (freeze on first commit whose tree
    // satisfies plan-file + .trinity/finished/<stem>/ APPROVE files),
    // monotone-after-freeze behaviour, and split-fold equivalence.
    // ===================================================================

    fn freeze_snap(history: Vec<CommitEvent>) -> DiskSnapshot {
        DiskSnapshot {
            head: history.last().map(|e| e.commit.clone()),
            history,
            feedback_files: vec![],
        }
    }

    #[test]
    fn freeze_plan_only_finish_with_one_approve() {
        let snap = freeze_snap(vec![
            entry("c1c1", vec![touch_at_active("foo", PlanTouchKind::Intro)], false),
            finalize_entry(
                "c2c2",
                vec![],
                false,
                vec![upsert_finalize("foo", "alice.md", "APPROVE")],
            ),
        ]);
        let state = derive_state(PathBuf::from("/r"), snap);
        let plan = &state.plans[&sess("foo")];
        assert_eq!(plan.frozen_at, Some(sha("c2c2")));
        assert_eq!(plan.freeze_events, vec![sha("c2c2")]);
    }

    #[test]
    fn freeze_with_two_approve_files() {
        let snap = freeze_snap(vec![
            entry("c1c1", vec![touch_at_active("foo", PlanTouchKind::Intro)], false),
            finalize_entry(
                "c2c2",
                vec![],
                false,
                vec![
                    upsert_finalize("foo", "alice.md", "APPROVE"),
                    upsert_finalize("foo", "bob.md", "APPROVE — lgtm"),
                ],
            ),
        ]);
        let state = derive_state(PathBuf::from("/r"), snap);
        assert_eq!(state.plans[&sess("foo")].frozen_at, Some(sha("c2c2")));
    }

    #[test]
    fn mixed_verdict_does_not_freeze() {
        let snap = freeze_snap(vec![
            entry("c1c1", vec![touch_at_active("foo", PlanTouchKind::Intro)], false),
            finalize_entry(
                "c2c2",
                vec![],
                false,
                vec![
                    upsert_finalize("foo", "alice.md", "APPROVE"),
                    upsert_finalize("foo", "bob.md", "REQUEST_CHANGES"),
                ],
            ),
        ]);
        let state = derive_state(PathBuf::from("/r"), snap);
        assert_eq!(state.plans[&sess("foo")].frozen_at, None);
    }

    #[test]
    fn mixed_verdict_then_cleanup_freezes_at_cleanup() {
        let snap = freeze_snap(vec![
            entry("c1c1", vec![touch_at_active("foo", PlanTouchKind::Intro)], false),
            finalize_entry(
                "c2c2",
                vec![],
                false,
                vec![
                    upsert_finalize("foo", "alice.md", "APPROVE"),
                    upsert_finalize("foo", "bob.md", "REQUEST_CHANGES"),
                ],
            ),
            finalize_entry(
                "c3c3",
                vec![],
                false,
                vec![remove_finalize("foo", "bob.md")],
            ),
        ]);
        let state = derive_state(PathBuf::from("/r"), snap);
        assert_eq!(state.plans[&sess("foo")].frozen_at, Some(sha("c3c3")));
    }

    #[test]
    fn empty_finished_dir_does_not_freeze() {
        // No finalize_changes applied → tree has no files → rule fails.
        let snap = freeze_snap(vec![entry(
            "c1c1",
            vec![touch_at_active("foo", PlanTouchKind::Intro)],
            false,
        )]);
        let state = derive_state(PathBuf::from("/r"), snap);
        assert_eq!(state.plans[&sess("foo")].frozen_at, None);
    }

    #[test]
    fn finalize_added_before_plan_file_freezes_when_plan_appears() {
        // c1 only adds finalize; the plan file doesn't exist yet → no freeze.
        // c2 adds the plan file → rule first holds at c2.
        let snap = freeze_snap(vec![
            finalize_entry(
                "c1c1",
                vec![],
                false,
                vec![upsert_finalize("foo", "alice.md", "APPROVE")],
            ),
            entry("c2c2", vec![touch_at_active("foo", PlanTouchKind::Intro)], false),
        ]);
        let state = derive_state(PathBuf::from("/r"), snap);
        assert_eq!(state.plans[&sess("foo")].frozen_at, Some(sha("c2c2")));
    }

    #[test]
    fn revert_of_finalize_keeps_plan_frozen() {
        // c1: plan intro. c2: finalize. c3: revert removes finalize files.
        // The fold freezes at c2; c3's removal does not un-freeze.
        let snap = freeze_snap(vec![
            entry("c1c1", vec![touch_at_active("foo", PlanTouchKind::Intro)], false),
            finalize_entry(
                "c2c2",
                vec![],
                false,
                vec![upsert_finalize("foo", "alice.md", "APPROVE")],
            ),
            finalize_entry(
                "c3c3",
                vec![],
                false,
                vec![remove_finalize("foo", "alice.md")],
            ),
        ]);
        let state = derive_state(PathBuf::from("/r"), snap);
        assert_eq!(state.plans[&sess("foo")].frozen_at, Some(sha("c2c2")));
    }

    #[test]
    fn post_freeze_code_commit_is_unattributed_for_frozen_plan() {
        let snap = freeze_snap(vec![
            entry("c1c1", vec![touch_at_active("foo", PlanTouchKind::Intro)], false),
            finalize_entry(
                "c2c2",
                vec![],
                false,
                vec![upsert_finalize("foo", "alice.md", "APPROVE")],
            ),
            entry("c3c3", vec![], true),
        ]);
        let state = derive_state(PathBuf::from("/r"), snap);
        assert_eq!(state.plans[&sess("foo")].frozen_at, Some(sha("c2c2")));
        assert_eq!(
            state.attribution[&sha("c3c3")],
            AttributionResult::Unattributed
        );
        // c3 is not a plan_touch and is not attributed to foo, so it
        // doesn't appear in foo.commits.
        assert!(!state.plans[&sess("foo")].commits.contains_key(&sha("c3c3")));
    }

    #[test]
    fn post_freeze_plan_revision_is_skipped_under_frozen_plan_key() {
        let snap = freeze_snap(vec![
            entry("c1c1", vec![touch_at_active("foo", PlanTouchKind::Intro)], false),
            finalize_entry(
                "c2c2",
                vec![],
                false,
                vec![upsert_finalize("foo", "alice.md", "APPROVE")],
            ),
            entry("c3c3", vec![touch_at_active("foo", PlanTouchKind::Revision)], false),
        ]);
        let state = derive_state(PathBuf::from("/r"), snap);
        // No plan_touches entry under foo for c3c3 (filtered).
        assert!(
            state
                .plan_touches
                .get(&sha("c3c3"))
                .map_or(true, |touches| touches
                    .iter()
                    .all(|(k, _)| k != &sess("foo")))
        );
    }

    #[test]
    fn post_freeze_working_tree_feedback_on_pre_freeze_sha_is_dropped_for_frozen_plan() {
        // Working-tree feedback targeting c1c1 (pre-freeze) is applied at
        // c1c1 during the chronological fold. At c1c1 the plan isn't
        // frozen yet, so the gate would include it — EXCEPT we filter
        // the per-plan gate step on `is_frozen(plan)`. At c1c1 the plan
        // isn't frozen, so the gate step runs and the gate gets the
        // feedback. The freeze happens at c2c2 (subsequent).
        //
        // This test documents the *sealing* behaviour: post-freeze
        // commits skip gate construction entirely. Pre-freeze gates that
        // received feedback before the freeze stay.
        let snap = DiskSnapshot {
            head: Some(sha("c3c3")),
            history: vec![
                entry("c1c1", vec![touch_at_active("foo", PlanTouchKind::Intro)], false),
                finalize_entry(
                    "c2c2",
                    vec![],
                    false,
                    vec![upsert_finalize("foo", "alice.md", "APPROVE")],
                ),
                entry("c3c3", vec![], true),
            ],
            feedback_files: vec![
                // Pre-freeze feedback on c1c1 — should land in foo.commits[c1c1].
                feedback("foo", "c1c1", "alice", "APPROVE\n"),
                // Post-freeze feedback on c3c3 (post-freeze SHA). c3c3's
                // gate is skipped (plan frozen), so this feedback has
                // nowhere to land — silently dropped.
                feedback("foo", "c3c3", "bob", "REQUEST_CHANGES\n"),
            ],
        };
        let state = derive_state(PathBuf::from("/r"), snap);
        let plan = &state.plans[&sess("foo")];
        assert_eq!(plan.frozen_at, Some(sha("c2c2")));
        // c1c1 has alice's APPROVE in its gate.
        let c1_gate = plan.commits.get(&sha("c1c1")).expect("c1 gate");
        assert!(c1_gate.feedback.contains_key(&AgentLabel::parse("alice").unwrap()));
        // c3c3 has no gate (skipped — frozen).
        assert!(!plan.commits.contains_key(&sha("c3c3")));
    }

    #[test]
    fn split_fold_equivalence_pre_freeze_only() {
        // Running the fold over the full history equals running it over a
        // prefix plus the suffix, for the frozen plan's slice. Validates
        // the caching property the fold is structured to support.
        let full = DiskSnapshot {
            head: Some(sha("c3c3")),
            history: vec![
                entry("c1c1", vec![touch_at_active("foo", PlanTouchKind::Intro)], false),
                finalize_entry(
                    "c2c2",
                    vec![],
                    false,
                    vec![upsert_finalize("foo", "alice.md", "APPROVE")],
                ),
                entry("c3c3", vec![], true),
            ],
            feedback_files: vec![],
        };
        let prefix_only = DiskSnapshot {
            head: Some(sha("c2c2")),
            history: full.history[..2].to_vec(),
            feedback_files: vec![],
        };
        let full_state = derive_state(PathBuf::from("/r"), full);
        let prefix_state = derive_state(PathBuf::from("/r"), prefix_only);
        let full_plan = &full_state.plans[&sess("foo")];
        let prefix_plan = &prefix_state.plans[&sess("foo")];
        // The frozen plan's slice is byte-identical between the two
        // folds — see plan §criterion 9.
        assert_eq!(full_plan.frozen_at, prefix_plan.frozen_at);
        assert_eq!(full_plan.freeze_events, prefix_plan.freeze_events);
        assert_eq!(full_plan.commits, prefix_plan.commits);
    }

    #[test]
    fn archived_cycles_overcount_regression_via_freeze_events() {
        // freeze at C, then post-freeze deletion, then phantom re-finalize
        // (snapshot files written again) -> freeze_events.len() == 1.
        let snap = freeze_snap(vec![
            entry("c1c1", vec![touch_at_active("foo", PlanTouchKind::Intro)], false),
            finalize_entry(
                "c2c2",
                vec![],
                false,
                vec![upsert_finalize("foo", "alice.md", "APPROVE")],
            ),
            finalize_entry(
                "c3c3",
                vec![],
                false,
                vec![remove_finalize("foo", "alice.md")],
            ),
            finalize_entry(
                "c4c4",
                vec![],
                false,
                vec![upsert_finalize("foo", "bob.md", "APPROVE")],
            ),
        ]);
        let state = derive_state(PathBuf::from("/r"), snap);
        let plan = &state.plans[&sess("foo")];
        assert_eq!(plan.frozen_at, Some(sha("c2c2")));
        assert_eq!(plan.freeze_events, vec![sha("c2c2")]);
    }
}
