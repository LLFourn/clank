//! The sans-IO boundary for repo-state derivation.
//!
//! `DiskSnapshot` is a structural representation of everything Trinity
//! reads from disk + git for one repo at one point in time: HEAD, the
//! plan files in HEAD's tree, the commit history along the first-parent
//! chain, and the feedback files in the working tree.
//!
//! `derive_state` is a pure function from `(repo_root, DiskSnapshot)`
//! to `RepoState`. No git, no filesystem, no async — synthetic snapshots
//! make the whole derivation testable in isolation.
//!
//! The IO layer (`git_io::snapshot`) builds a `DiskSnapshot` by running
//! `git ls-tree`, `git show`, `git log`, `git diff-tree`, and walking
//! `<repo>/.trinity/feedback/`. `rebuild::rebuild_repo` glues the two
//! together.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use crate::attribution::{CommitChanges, FinalizeChangeKind, classify, effective_session};
use crate::disk_format::{
    FeedbackPath, finalize_first_line_starts_with_approve, parse_verdict, plan_path_is_done,
};
use crate::lifecycle::{AgentLabel, CommitSha, PlanKey, content_hash};
use crate::repo_state::{
    AttributionResult, CommitKind, Feedback, Plan, PlanTouchKind, RepoState, Verdict,
};
use crate::review_state::{CommitGate, CommitGateState};

/// Everything Trinity needs to derive a repo's state, materialized into
/// structured types. Built by `git_io::snapshot`; consumed by
/// `derive_state`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DiskSnapshot {
    pub head: Option<CommitSha>,
    /// One entry per `git ls-tree HEAD -- .trinity/plans/` blob whose
    /// basename parses as a session id. Order is preserved from ls-tree.
    pub plan_files: Vec<PlanFileBlob>,
    /// First-parent commit chain from the root to HEAD, oldest-first.
    /// Each entry carries the structured diff against its parent (or
    /// against the empty tree for the root commit).
    pub history: Vec<HistoryEntry>,
    /// Working-tree feedback files. Each has been path-parsed; the body
    /// is included verbatim.
    pub feedback_files: Vec<FeedbackBlob>,
    /// Per-commit metadata (author timestamp, subject) for every commit
    /// in `history`. One batched `git log` populates this in
    /// `git_io::first_parent_commits`; `derive_state` carries it over to
    /// `RepoState::commit_meta` for use by `last_activity_ts` +
    /// timeline subject rendering.
    pub commit_meta: BTreeMap<CommitSha, CommitMetaEntry>,
}

/// Author timestamp + subject line for one commit. Mirrors
/// `git_io::CommitMeta` but decoupled — `git_io` is the producer; the
/// daemon stores its own copy so the lifecycle types don't reach into
/// the IO layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitMetaEntry {
    pub author_ts: i64,
    pub subject: String,
}

/// A plan file as it appears in HEAD's tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanFileBlob {
    pub plan_key: PlanKey,
    pub plan_path: PathBuf,
    /// Body from HEAD's blob (not the working tree).
    pub body: String,
    /// First commit that added this plan path (via `git log
    /// --diff-filter=A --follow`).
    pub plan_intro: CommitSha,
    /// First-parent of `plan_intro`, or `None` for the root commit.
    pub plan_intro_parent: Option<CommitSha>,
}

/// One commit in the first-parent chain plus its diff against the parent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEntry {
    pub commit: CommitSha,
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

    init_plans_from_head(&mut state, snapshot.plan_files);
    let plan_keys: Vec<PlanKey> = state.plans.keys().cloned().collect();

    // Pre-index feedback per plan: (target_sha, author) -> Feedback.
    let mut feedback_by_plan: BTreeMap<PlanKey, BTreeMap<(CommitSha, AgentLabel), Feedback>> =
        BTreeMap::new();
    for fb in snapshot.feedback_files {
        if !state.plans.contains_key(&fb.parsed.plan_key) {
            continue;
        }
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
    let mut plan_at_active_path: BTreeSet<PlanKey> = BTreeSet::new();
    let mut finalize_tree: BTreeMap<PlanKey, BTreeMap<String, String>> = BTreeMap::new();
    let mut gate_participants: BTreeMap<PlanKey, Vec<AgentLabel>> = plan_keys
        .iter()
        .map(|k| (k.clone(), Vec::new()))
        .collect();

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
                // Track plan-file-at-active-path from new_path. None ==
                // deletion (or test fixture without explicit path).
                match &touch.new_path {
                    Some(p) if !plan_path_is_done(p) => {
                        plan_at_active_path.insert(touch.session.clone());
                    }
                    Some(_) => {
                        plan_at_active_path.remove(&touch.session);
                    }
                    None => {
                        // Fixture didn't specify; fall back to HEAD's
                        // plan_path so synthetic tests without finalize
                        // changes still behave as before.
                        if let Some(plan) = state.plans.get(&touch.session) {
                            if !plan_path_is_done(&plan.plan_path) {
                                plan_at_active_path.insert(touch.session.clone());
                            }
                        }
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
        // frozen and whose plan file is at the active path now, fire if
        // the finalize tree contains ≥1 file and all first lines start
        // with APPROVE.
        for plan_key in &maybe_affected {
            if !state.plans.contains_key(plan_key) {
                continue;
            }
            if is_frozen(&state, plan_key) {
                continue;
            }
            if !plan_at_active_path.contains(plan_key) {
                continue;
            }
            let files = finalize_tree.get(plan_key);
            if finalize_rule_satisfied(files) {
                let approver_count = files.map(|m| m.len()).unwrap_or(0) as u32;
                if let Some(plan) = state.plans.get_mut(plan_key) {
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
        for plan_key in &plan_keys {
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
            let participants = gate_participants.get_mut(plan_key).expect("init above");
            let fb_map = feedback_by_plan.get(plan_key);
            let gate = build_gate_step(&commit_sha, fb_map, participants);
            if let Some(plan) = state.plans.get_mut(plan_key) {
                plan.commits.insert(commit_sha.clone(), gate);
            }
        }
    }

    state.commit_meta = snapshot.commit_meta;
    state
}

fn init_plans_from_head(state: &mut RepoState, plan_files: Vec<PlanFileBlob>) {
    let mut by_key: BTreeMap<PlanKey, Vec<PlanFileBlob>> = BTreeMap::new();
    for pf in plan_files {
        by_key.entry(pf.plan_key.clone()).or_default().push(pf);
    }
    for (key, mut entries) in by_key {
        if entries.len() >= 2 {
            let paths: Vec<PathBuf> = entries.iter().map(|pf| pf.plan_path.clone()).collect();
            state.plan_conflicts.insert(key, paths);
            continue;
        }
        let pf = entries.pop().expect("exactly one entry");
        let body_hash = content_hash(&pf.body);
        let plan_state = crate::repo_state::PlanState::from_plan_path(&pf.plan_path);
        state.plans.insert(
            pf.plan_key.clone(),
            Plan {
                id: pf.plan_key,
                plan_path: pf.plan_path,
                state: plan_state,
                body: pf.body,
                body_hash,
                plan_intro: pf.plan_intro,
                plan_intro_parent: pf.plan_intro_parent,
                commits: BTreeMap::new(),
                frozen_at: None,
                freeze_events: Vec::new(),
                archived_cycles: Vec::new(),
            },
        );
    }
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
    use crate::lifecycle::{AgentLabel, is_done_plan_path};
    use crate::repo_state::{AttributionResult, PlanTouchKind};

    fn sha(s: &str) -> CommitSha {
        CommitSha::parse(s).unwrap_or_else(|e| panic!("invalid test SHA {s:?}: {e}"))
    }

    fn sess(s: &str) -> PlanKey {
        PlanKey::parse(s).unwrap()
    }

    fn plan_file(stem: &str, intro: &str, parent: Option<&str>, body: &str) -> PlanFileBlob {
        PlanFileBlob {
            plan_key: sess(stem),
            plan_path: PathBuf::from(format!(".trinity/plans/{stem}.md")),
            body: body.to_string(),
            plan_intro: sha(intro),
            plan_intro_parent: parent.map(sha),
        }
    }

    fn touch(stem: &str, kind: PlanTouchKind) -> PlanTouch {
        PlanTouch {
            session: sess(stem),
            kind,
            new_path: None,
        }
    }

    fn touch_at_active(stem: &str, kind: PlanTouchKind) -> PlanTouch {
        PlanTouch {
            session: sess(stem),
            kind,
            new_path: Some(PathBuf::from(format!(".trinity/plans/{stem}.md"))),
        }
    }

    fn entry(commit: &str, touches: Vec<PlanTouch>, code: bool) -> HistoryEntry {
        HistoryEntry {
            commit: sha(commit),
            changes: CommitChanges {
                plan_touches: touches,
                has_non_plan_code_changes: code,
                finalize_changes: Vec::new(),
            },
        }
    }

    fn upsert_finalize(stem: &str, file: &str, first_line: &str) -> crate::attribution::FinalizeChange {
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

    fn finalize_entry(
        commit: &str,
        touches: Vec<PlanTouch>,
        code: bool,
        finalize_changes: Vec<crate::attribution::FinalizeChange>,
    ) -> HistoryEntry {
        HistoryEntry {
            commit: sha(commit),
            changes: CommitChanges {
                plan_touches: touches,
                has_non_plan_code_changes: code,
                finalize_changes,
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

    #[test]
    fn empty_snapshot_yields_empty_state() {
        let state = derive_state(PathBuf::from("/r"), DiskSnapshot::default());
        assert!(state.plans.is_empty());
        assert!(state.attribution.is_empty());
        assert!(state.head.is_none());
    }

    #[test]
    fn single_plan_creates_session_with_hash_and_intro() {
        let snap = DiskSnapshot {
            head: Some(sha("aaaa")),
            plan_files: vec![plan_file("foo", "1231", Some("9991"), "# foo\n")],
            history: vec![],
            feedback_files: vec![],
            commit_meta: BTreeMap::new(),
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
            plan_files: vec![plan_file("foo", "c1c1", None, "# foo\n")],
            history: vec![
                entry("c1c1", vec![touch("foo", PlanTouchKind::Intro)], false),
                entry("c2c2", vec![], true),
                entry("c3c3", vec![], true),
            ],
            feedback_files: vec![],
            commit_meta: BTreeMap::new(),
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
            plan_files: vec![
                plan_file("a", "c1c1", None, "# a\n"),
                plan_file("b", "c2c2", Some("c1c1"), "# b\n"),
            ],
            history: vec![
                entry("c1c1", vec![touch("a", PlanTouchKind::Intro)], false),
                entry(
                    "c2c2",
                    vec![
                        touch("a", PlanTouchKind::Revision),
                        touch("b", PlanTouchKind::Intro),
                    ],
                    false,
                ),
                entry("c3c3", vec![], true),
            ],
            feedback_files: vec![],
            commit_meta: BTreeMap::new(),
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
        let mut done = plan_file("done-one", "c1c1", None, "# done\n");
        done.plan_path = PathBuf::from(".trinity/plans/done/done-one.md");
        let snap = DiskSnapshot {
            head: Some(sha("c3c3")),
            plan_files: vec![
                done,
                plan_file("active-one", "c2c2", Some("c1c1"), "# active v2\n"),
            ],
            history: vec![
                entry("c1c1", vec![touch("done-one", PlanTouchKind::Intro)], false),
                entry(
                    "c2c2",
                    vec![touch("active-one", PlanTouchKind::Intro)],
                    false,
                ),
                entry(
                    "c3c3",
                    vec![
                        touch("done-one", PlanTouchKind::DoneMove),
                        touch("active-one", PlanTouchKind::Revision),
                    ],
                    false,
                ),
            ],
            feedback_files: vec![],
            commit_meta: BTreeMap::new(),
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
            crate::projection::all_plan_revisions(&state.plans[&sess("done-one")], &state),
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
            plan_files: vec![plan_file("foo", "c1c1", None, "# foo\n")],
            history: vec![entry(
                "c1c1",
                vec![touch("foo", PlanTouchKind::Intro)],
                true,
            )],
            feedback_files: vec![],
            commit_meta: BTreeMap::new(),
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
            plan_files: vec![plan_file("foo", "c1c1", None, "# foo\n")],
            history: vec![entry(
                "c1c1",
                vec![touch("foo", PlanTouchKind::Intro)],
                false,
            )],
            feedback_files: vec![feedback("foo", "c1c1", "alice", "APPROVE\n")],
            commit_meta: BTreeMap::new(),
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
            plan_files: vec![],
            history: vec![],
            feedback_files: vec![feedback("ghost", "c1c1", "alice", "APPROVE\n")],
            commit_meta: BTreeMap::new(),
        };
        let state = derive_state(PathBuf::from("/r"), snap);
        assert!(state.plans.is_empty());
    }

    #[test]
    fn session_in_done_subdir_uses_done_path() {
        let mut pf = plan_file("foo", "c1c1", None, "# foo\n");
        pf.plan_path = PathBuf::from(".trinity/plans/done/foo.md");
        let snap = DiskSnapshot {
            head: Some(sha("c2c2")),
            plan_files: vec![pf],
            history: vec![],
            feedback_files: vec![],
            commit_meta: BTreeMap::new(),
        };
        let state = derive_state(PathBuf::from("/r"), snap);
        assert_eq!(
            state.plans[&sess("foo")].plan_path,
            PathBuf::from(".trinity/plans/done/foo.md")
        );
    }

    #[test]
    fn root_with_no_plan_touch_is_unattributed() {
        let snap = DiskSnapshot {
            head: Some(sha("c1c1")),
            plan_files: vec![],
            history: vec![entry("c1c1", vec![], true)],
            feedback_files: vec![],
            commit_meta: BTreeMap::new(),
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
        alt.plan_files
            .push(plan_file("bar", "b1b1", None, "# bar\n"));
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
            plan_files: vec![plan_file("foo", "c1c1", None, "# v1\n")],
            history: vec![
                entry("c1c1", vec![touch("foo", PlanTouchKind::Intro)], false),
                entry("c2c2", vec![touch("foo", PlanTouchKind::Revision)], false),
                entry("c3c3", vec![], true),
            ],
            // stale: c2 is the latest plan rev now
            feedback_files: vec![feedback("foo", "c1c1", "alice", "APPROVE\n")],
            commit_meta: BTreeMap::new(),
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
            plan_files: vec![
                plan_file("alpha", "c1c1", None, "# alpha\n"),
                plan_file("beta", "c2c2", Some("c1c1"), "# beta\n"),
            ],
            history: vec![
                entry("c1c1", vec![touch("alpha", PlanTouchKind::Intro)], false),
                entry("c2c2", vec![touch("beta", PlanTouchKind::Intro)], false),
                entry("c3c3", vec![], true),
            ],
            feedback_files: vec![],
            commit_meta: BTreeMap::new(),
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
    fn done_session_keeps_plan_revisions_and_impl_commits() {
        // After moving to plans/done/, the session is still present;
        // historical commits remain attributed.
        let mut pf = plan_file("foo", "c1c1", None, "# foo\n");
        pf.plan_path = PathBuf::from(".trinity/plans/done/foo.md");
        let snap = DiskSnapshot {
            head: Some(sha("c4c4")),
            plan_files: vec![pf],
            history: vec![
                entry("c1c1", vec![touch("foo", PlanTouchKind::Intro)], false),
                entry("c2c2", vec![], true),
                entry("c3c3", vec![], true),
                entry("c4c4", vec![touch("foo", PlanTouchKind::DoneMove)], false),
            ],
            feedback_files: vec![],
            commit_meta: BTreeMap::new(),
        };
        let state = derive_state(PathBuf::from("/r"), snap);
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
        assert_eq!(plan_touches.len(), 2, "intro + done_move");
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
            plan_files: vec![plan_file("foo", "c1c1", None, "")],
            history: vec![entry(
                "c1c1",
                vec![touch("foo", PlanTouchKind::Intro)],
                false,
            )],
            feedback_files: vec![],
            commit_meta: BTreeMap::new(),
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
            plan_files: vec![plan_file("foo", "c3c3", Some("c2c2"), "# foo\n")],
            history: vec![
                entry("c1c1", vec![], true),
                entry("c2c2", vec![], true),
                entry("c3c3", vec![touch("foo", PlanTouchKind::Intro)], false),
                entry("c4c4", vec![], true),
            ],
            feedback_files: vec![],
            commit_meta: BTreeMap::new(),
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
            plan_files: vec![plan_file("foo", "c1c1", None, "# foo\n")],
            history: vec![
                entry("c1c1", vec![touch("foo", PlanTouchKind::Intro)], false),
                entry("c2c2", vec![], true),
                entry("c3c3", vec![], true),
            ],
            feedback_files: vec![],
            commit_meta: BTreeMap::new(),
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
            plan_files: vec![plan_file("foo", "c1c1", None, "# foo\n")],
            history: vec![entry(
                "c1c1",
                vec![touch("foo", PlanTouchKind::Intro)],
                false,
            )],
            feedback_files: vec![feedback("foo", "c1c1", "alice", "APPROVE\n")],
            commit_meta: BTreeMap::new(),
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
            plan_files: vec![plan_file("foo", "c1c1", None, "# foo\n")],
            history: vec![
                entry("c1c1", vec![touch("foo", PlanTouchKind::Intro)], false),
                entry("c2c2", vec![], true),
            ],
            feedback_files: vec![feedback("foo", "c2c2", "bob", "REQUEST_CHANGES\nproblem\n")],
            commit_meta: BTreeMap::new(),
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
            plan_files: vec![
                plan_file("foo", "c1c1", None, "# foo\n"),
                plan_file("bar", "c2c2", Some("c1c1"), "# bar\n"),
            ],
            history: vec![
                entry("c1c1", vec![touch("foo", PlanTouchKind::Intro)], false),
                entry(
                    "c2c2",
                    vec![
                        touch("foo", PlanTouchKind::Revision),
                        touch("bar", PlanTouchKind::Intro),
                    ],
                    false,
                ),
                entry("c3c3", vec![], true),
            ],
            feedback_files: vec![],
            commit_meta: BTreeMap::new(),
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
            plan_files: vec![
                plan_file("a", "c1c1", None, "# a\n"),
                plan_file("b", "c2c2", Some("c1c1"), "# b\n"),
            ],
            history: vec![
                entry("c1c1", vec![touch("a", PlanTouchKind::Intro)], false),
                entry("c2c2", vec![touch("b", PlanTouchKind::Intro)], false),
                entry("c3c3", vec![], true),
            ],
            feedback_files: vec![],
            commit_meta: BTreeMap::new(),
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
            plan_files: vec![plan_file("foo", "c1c1", None, "# foo\n")],
            history: vec![entry(
                "c1c1",
                vec![touch("foo", PlanTouchKind::Intro)],
                false,
            )],
            feedback_files: vec![
                feedback("foo", "c1c1", "alice", "APPROVE\n"),
                feedback("foo", "c1c1", "bob", "REQUEST_CHANGES\n"),
            ],
            commit_meta: BTreeMap::new(),
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

    // ===== plan_conflicts =====

    fn done_plan_file(stem: &str, intro: &str, body: &str) -> PlanFileBlob {
        PlanFileBlob {
            plan_key: sess(stem),
            plan_path: PathBuf::from(format!(".trinity/plans/done/{stem}.md")),
            body: body.to_string(),
            plan_intro: sha(intro),
            plan_intro_parent: None,
        }
    }

    #[test]
    fn same_stem_active_and_done_lands_in_plan_conflicts() {
        let snap = DiskSnapshot {
            head: Some(sha("c2c2")),
            plan_files: vec![
                plan_file("foo", "c1c1", None, "# active\n"),
                done_plan_file("foo", "c2c2", "# done\n"),
            ],
            history: vec![],
            feedback_files: vec![],
            commit_meta: BTreeMap::new(),
        };
        let state = derive_state(PathBuf::from("/r"), snap);
        assert!(
            !state.plans.contains_key(&sess("foo")),
            "conflicting plans must not be routed"
        );
        let paths = state
            .plan_conflicts
            .get(&sess("foo"))
            .expect("conflict surfaced");
        assert_eq!(paths.len(), 2);
        assert!(paths.iter().any(|p| !is_done_plan_path(p)));
        assert!(paths.iter().any(|p| is_done_plan_path(p)));
    }

    /// Reusable fixture: foo plan (plan_intro c1) + one impl commit c2,
    /// plus alice's APPROVE on c1 (plan) and bob's REQUEST_CHANGES on c2 (impl).
    fn full_workflow_snapshot() -> DiskSnapshot {
        DiskSnapshot {
            head: Some(sha("c2c2")),
            plan_files: vec![plan_file("foo", "c1c1", None, "# foo\n")],
            history: vec![
                entry("c1c1", vec![touch("foo", PlanTouchKind::Intro)], false),
                entry("c2c2", vec![], true),
            ],
            feedback_files: vec![
                feedback("foo", "c1c1", "alice", "APPROVE\n"),
                feedback("foo", "c2c2", "bob", "REQUEST_CHANGES\nstuff\n"),
            ],
            commit_meta: BTreeMap::new(),
        }
    }

    // ===================================================================
    // Phase 2: event-log fold + finalize snapshot reader.
    // Tests for the finalize rule (freeze on first commit whose tree
    // satisfies plan-file + .trinity/finished/<stem>/ APPROVE files),
    // monotone-after-freeze behaviour, and split-fold equivalence.
    // ===================================================================

    fn freeze_snap(history: Vec<HistoryEntry>) -> DiskSnapshot {
        DiskSnapshot {
            head: history.last().map(|e| e.commit.clone()),
            plan_files: vec![plan_file("foo", "c1c1", None, "# foo\n")],
            history,
            feedback_files: vec![],
            commit_meta: BTreeMap::new(),
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
            plan_files: vec![plan_file("foo", "c1c1", None, "# foo\n")],
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
            commit_meta: BTreeMap::new(),
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
            plan_files: vec![plan_file("foo", "c1c1", None, "# foo\n")],
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
            commit_meta: BTreeMap::new(),
        };
        let prefix_only = DiskSnapshot {
            head: Some(sha("c2c2")),
            plan_files: full.plan_files.clone(),
            history: full.history[..2].to_vec(),
            feedback_files: vec![],
            commit_meta: BTreeMap::new(),
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
