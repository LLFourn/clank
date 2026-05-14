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

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::attribution::{CommitChanges, classify, effective_session};
use crate::disk_format::{FeedbackPath, FeedbackPhase, parse_verdict};
use crate::lifecycle::{CommitSha, SessionId, content_hash};
use crate::repo_state::{Feedback, HeldFeedback, RepoState, Session};

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
}

/// A plan file as it appears in HEAD's tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanFileBlob {
    pub session_id: SessionId,
    /// Path relative to repo root: `.trinity/plans/<id>.md` or
    /// `.trinity/plans/done/<id>.md`.
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
}

/// Pure derivation of `RepoState` from a snapshot. No IO. Tests in this
/// module construct synthetic snapshots and assert on the result.
pub fn derive_state(repo_root: PathBuf, snapshot: DiskSnapshot) -> RepoState {
    let mut state = RepoState::empty(repo_root);
    state.head = snapshot.head;

    // 1. Sessions from plan-file blobs.
    for pf in snapshot.plan_files {
        let body_hash = content_hash(&pf.body);
        state.sessions.insert(
            pf.session_id.clone(),
            Session {
                id: pf.session_id,
                plan_path: pf.plan_path,
                body: pf.body,
                body_hash,
                plan_intro: pf.plan_intro,
                plan_intro_parent: pf.plan_intro_parent,
                plan_feedback: BTreeMap::new(),
                impl_feedback: BTreeMap::new(),
                held_plan_feedback: Vec::new(),
            },
        );
    }

    // 2. Attribution walk along the first-parent chain.
    let mut current_effective: Option<SessionId> = None;
    for entry in &snapshot.history {
        let result = classify(&entry.changes, current_effective.as_ref());
        current_effective = effective_session(&entry.changes, current_effective.as_ref());
        state.attribution.insert(entry.commit.clone(), result);
    }

    // 3. Feedback ingestion. Files for unknown sessions are dropped
    //    (they'd be flagged elsewhere if needed); files with a target
    //    SHA land in `plan_feedback` / `impl_feedback`; flat drops are
    //    held with a reason key the runner uses to decide next steps.
    for fb in snapshot.feedback_files {
        let Some(session) = state.sessions.get_mut(&fb.parsed.session_id) else {
            continue;
        };
        ingest_feedback(session, fb);
    }

    state
}

fn ingest_feedback(session: &mut Session, fb: FeedbackBlob) {
    let verdict = parse_verdict(&fb.body);
    match fb.parsed.target_sha {
        Some(target_sha) => {
            let map = match fb.parsed.phase {
                FeedbackPhase::Plan => &mut session.plan_feedback,
                FeedbackPhase::Impl => &mut session.impl_feedback,
            };
            map.insert(
                (target_sha, fb.parsed.author),
                Feedback {
                    path: fb.abs_path,
                    body: fb.body,
                    verdict,
                },
            );
        }
        None => {
            let reason = match fb.parsed.phase {
                FeedbackPhase::Plan => "plan_dirty",
                FeedbackPhase::Impl => "impl_flat_drop",
            };
            session.held_plan_feedback.push(HeldFeedback {
                path: fb.abs_path,
                author: fb.parsed.author,
                body: fb.body,
                reason,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attribution::PlanTouch;
    use crate::lifecycle::AgentLabel;
    use crate::repo_state::{AttributionResult, PlanTouchKind};

    fn sha(s: &str) -> CommitSha {
        CommitSha::from(s.to_string())
    }

    fn sess(s: &str) -> SessionId {
        SessionId::from(s.to_string())
    }

    fn plan_file(session: &str, intro: &str, parent: Option<&str>, body: &str) -> PlanFileBlob {
        PlanFileBlob {
            session_id: sess(session),
            plan_path: PathBuf::from(format!(".trinity/plans/{session}.md")),
            body: body.to_string(),
            plan_intro: sha(intro),
            plan_intro_parent: parent.map(sha),
        }
    }

    fn touch(session: &str, kind: PlanTouchKind) -> PlanTouch {
        PlanTouch {
            session: sess(session),
            kind,
        }
    }

    fn entry(commit: &str, touches: Vec<PlanTouch>, code: bool) -> HistoryEntry {
        HistoryEntry {
            commit: sha(commit),
            changes: CommitChanges {
                plan_touches: touches,
                has_non_plan_code_changes: code,
            },
        }
    }

    fn feedback(
        session: &str,
        phase: FeedbackPhase,
        target: Option<&str>,
        author: &str,
        body: &str,
    ) -> FeedbackBlob {
        FeedbackBlob {
            abs_path: PathBuf::from(format!("/r/.trinity/feedback/{session}/...")),
            parsed: FeedbackPath {
                session_id: sess(session),
                phase,
                target_sha: target.map(sha),
                author: AgentLabel::from(author.to_string()),
                raw: PathBuf::from("..."),
            },
            body: body.to_string(),
        }
    }

    #[test]
    fn empty_snapshot_yields_empty_state() {
        let state = derive_state(PathBuf::from("/r"), DiskSnapshot::default());
        assert!(state.sessions.is_empty());
        assert!(state.attribution.is_empty());
        assert!(state.head.is_none());
    }

    #[test]
    fn single_plan_creates_session_with_hash_and_intro() {
        let snap = DiskSnapshot {
            head: Some(sha("aaa")),
            plan_files: vec![plan_file("foo", "intro1", Some("parent1"), "# foo\n")],
            history: vec![],
            feedback_files: vec![],
        };
        let state = derive_state(PathBuf::from("/r"), snap);
        let s = &state.sessions[&sess("foo")];
        assert_eq!(s.body, "# foo\n");
        assert_eq!(s.body_hash, content_hash("# foo\n"));
        assert_eq!(s.plan_intro, sha("intro1"));
        assert_eq!(s.plan_intro_parent, Some(sha("parent1")));
        assert_eq!(s.plan_path, PathBuf::from(".trinity/plans/foo.md"));
    }

    #[test]
    fn linear_history_attributes_per_walk_back_rules() {
        let snap = DiskSnapshot {
            head: Some(sha("c3")),
            plan_files: vec![plan_file("foo", "c1", None, "# foo\n")],
            history: vec![
                entry("c1", vec![touch("foo", PlanTouchKind::Intro)], false),
                entry("c2", vec![], true),
                entry("c3", vec![], true),
            ],
            feedback_files: vec![],
        };
        let state = derive_state(PathBuf::from("/r"), snap);
        // c1: plan intro for foo
        assert!(matches!(
            state.attribution[&sha("c1")],
            AttributionResult::Attributed { plan_touch: Some(PlanTouchKind::Intro), .. }
        ));
        // c2 + c3: walk-back inherit foo
        for c in ["c2", "c3"] {
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
            head: Some(sha("c3")),
            plan_files: vec![
                plan_file("a", "c1", None, "# a\n"),
                plan_file("b", "c2", Some("c1"), "# b\n"),
            ],
            history: vec![
                entry("c1", vec![touch("a", PlanTouchKind::Intro)], false),
                entry(
                    "c2",
                    vec![
                        touch("a", PlanTouchKind::Revision),
                        touch("b", PlanTouchKind::Intro),
                    ],
                    false,
                ),
                entry("c3", vec![], true),
            ],
            feedback_files: vec![],
        };
        let state = derive_state(PathBuf::from("/r"), snap);
        // c2 is multi-plan → unattributed.
        assert_eq!(state.attribution[&sha("c2")], AttributionResult::Unattributed);
        // c3 walks back through c2 transparently → attributes to a.
        match &state.attribution[&sha("c3")] {
            AttributionResult::Attributed {
                session,
                plan_touch: None,
                has_code_changes: true,
            } => assert_eq!(session, &sess("a")),
            other => panic!("c3: unexpected {other:?}"),
        }
    }

    #[test]
    fn mixed_commit_carries_both_plan_touch_and_code() {
        let snap = DiskSnapshot {
            head: Some(sha("c1")),
            plan_files: vec![plan_file("foo", "c1", None, "# foo\n")],
            history: vec![entry(
                "c1",
                vec![touch("foo", PlanTouchKind::Intro)],
                true,
            )],
            feedback_files: vec![],
        };
        let state = derive_state(PathBuf::from("/r"), snap);
        match &state.attribution[&sha("c1")] {
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
            head: Some(sha("c1")),
            plan_files: vec![plan_file("foo", "c1", None, "# foo\n")],
            history: vec![entry("c1", vec![touch("foo", PlanTouchKind::Intro)], false)],
            feedback_files: vec![feedback(
                "foo",
                FeedbackPhase::Plan,
                Some("c1"),
                "alice",
                "APPROVE\n",
            )],
        };
        let state = derive_state(PathBuf::from("/r"), snap);
        let entry = state.sessions[&sess("foo")]
            .plan_feedback
            .get(&(sha("c1"), AgentLabel::from("alice".to_string())))
            .expect("alice's plan feedback");
        assert_eq!(entry.verdict, crate::repo_state::Verdict::Approve);
    }

    #[test]
    fn flat_drop_plan_feedback_is_held() {
        let snap = DiskSnapshot {
            head: Some(sha("c1")),
            plan_files: vec![plan_file("foo", "c1", None, "# foo\n")],
            history: vec![entry("c1", vec![touch("foo", PlanTouchKind::Intro)], false)],
            feedback_files: vec![feedback(
                "foo",
                FeedbackPhase::Plan,
                None,
                "alice",
                "APPROVE\n",
            )],
        };
        let state = derive_state(PathBuf::from("/r"), snap);
        let session = &state.sessions[&sess("foo")];
        assert!(session.plan_feedback.is_empty());
        assert_eq!(session.held_plan_feedback.len(), 1);
        assert_eq!(session.held_plan_feedback[0].reason, "plan_dirty");
    }

    #[test]
    fn flat_drop_impl_feedback_is_held_with_different_reason() {
        let snap = DiskSnapshot {
            head: Some(sha("c2")),
            plan_files: vec![plan_file("foo", "c1", None, "# foo\n")],
            history: vec![
                entry("c1", vec![touch("foo", PlanTouchKind::Intro)], false),
                entry("c2", vec![], true),
            ],
            feedback_files: vec![feedback(
                "foo",
                FeedbackPhase::Impl,
                None,
                "alice",
                "APPROVE\n",
            )],
        };
        let state = derive_state(PathBuf::from("/r"), snap);
        let session = &state.sessions[&sess("foo")];
        assert!(session.impl_feedback.is_empty());
        assert_eq!(session.held_plan_feedback[0].reason, "impl_flat_drop");
    }

    #[test]
    fn feedback_for_unknown_session_is_dropped() {
        let snap = DiskSnapshot {
            head: Some(sha("c1")),
            plan_files: vec![],
            history: vec![],
            feedback_files: vec![feedback(
                "ghost",
                FeedbackPhase::Plan,
                Some("c1"),
                "alice",
                "APPROVE\n",
            )],
        };
        let state = derive_state(PathBuf::from("/r"), snap);
        assert!(state.sessions.is_empty());
    }

    #[test]
    fn session_in_done_subdir_uses_done_path() {
        let mut pf = plan_file("foo", "c1", None, "# foo\n");
        pf.plan_path = PathBuf::from(".trinity/plans/done/foo.md");
        let snap = DiskSnapshot {
            head: Some(sha("c2")),
            plan_files: vec![pf],
            history: vec![],
            feedback_files: vec![],
        };
        let state = derive_state(PathBuf::from("/r"), snap);
        assert_eq!(
            state.sessions[&sess("foo")].plan_path,
            PathBuf::from(".trinity/plans/done/foo.md")
        );
    }

    #[test]
    fn root_with_no_plan_touch_is_unattributed() {
        let snap = DiskSnapshot {
            head: Some(sha("c1")),
            plan_files: vec![],
            history: vec![entry("c1", vec![], true)],
            feedback_files: vec![],
        };
        let state = derive_state(PathBuf::from("/r"), snap);
        assert_eq!(state.attribution[&sha("c1")], AttributionResult::Unattributed);
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
        alt.head = Some(sha("different"));
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
        alt.history.push(entry("new_impl", vec![], true));
        assert_ne!(
            derive_state(PathBuf::from("/r"), base).digest(),
            derive_state(PathBuf::from("/r"), alt).digest()
        );
    }

    #[test]
    fn digest_changes_when_a_new_session_lands() {
        let base = full_workflow_snapshot();
        let mut alt = base.clone();
        alt.plan_files.push(plan_file("bar", "b1", None, "# bar\n"));
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
        assert!(state.sessions.contains_key(&sess("foo")));
        let foo = &state.sessions[&sess("foo")];
        // Plan APPROVE from alice on commit c1, impl REQUEST_CHANGES from bob on c2.
        assert_eq!(foo.plan_feedback.len(), 1);
        assert_eq!(foo.impl_feedback.len(), 1);
        // Attribution: c1 = plan_intro for foo, c2 = impl commit for foo.
        assert!(matches!(
            state.attribution[&sha("c1")],
            AttributionResult::Attributed {
                plan_touch: Some(PlanTouchKind::Intro),
                ..
            }
        ));
        assert!(matches!(
            state.attribution[&sha("c2")],
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
            head: Some(sha("c3")),
            plan_files: vec![plan_file("foo", "c1", None, "# v1\n")],
            history: vec![
                entry("c1", vec![touch("foo", PlanTouchKind::Intro)], false),
                entry("c2", vec![touch("foo", PlanTouchKind::Revision)], false),
                entry("c3", vec![], true),
            ],
            feedback_files: vec![feedback(
                "foo",
                FeedbackPhase::Plan,
                Some("c1"),  // stale: c2 is the latest plan rev now
                "alice",
                "APPROVE\n",
            )],
        };
        let state = derive_state(PathBuf::from("/r"), snap);
        let foo = &state.sessions[&sess("foo")];
        assert_eq!(foo.plan_feedback.len(), 1);
        let key = foo.plan_feedback.keys().next().unwrap();
        assert_eq!(key.0.as_str(), "c1", "stale feedback keeps its target");
    }

    #[test]
    fn many_plans_one_repo() {
        let snap = DiskSnapshot {
            head: Some(sha("c3")),
            plan_files: vec![
                plan_file("alpha", "c1", None, "# alpha\n"),
                plan_file("beta", "c2", Some("c1"), "# beta\n"),
            ],
            history: vec![
                entry("c1", vec![touch("alpha", PlanTouchKind::Intro)], false),
                entry("c2", vec![touch("beta", PlanTouchKind::Intro)], false),
                entry("c3", vec![], true),
            ],
            feedback_files: vec![],
        };
        let state = derive_state(PathBuf::from("/r"), snap);
        assert_eq!(state.sessions.len(), 2);
        // c3 walks back through c2 (beta's intro), so attributes to beta.
        match &state.attribution[&sha("c3")] {
            AttributionResult::Attributed { session, .. } => assert_eq!(session, &sess("beta")),
            other => panic!("c3 unexpected: {other:?}"),
        }
    }

    #[test]
    fn done_session_keeps_plan_revisions_and_impl_commits() {
        // After moving to plans/done/, the session is still present;
        // historical commits remain attributed.
        let mut pf = plan_file("foo", "c1", None, "# foo\n");
        pf.plan_path = PathBuf::from(".trinity/plans/done/foo.md");
        let snap = DiskSnapshot {
            head: Some(sha("c4")),
            plan_files: vec![pf],
            history: vec![
                entry("c1", vec![touch("foo", PlanTouchKind::Intro)], false),
                entry("c2", vec![], true),
                entry("c3", vec![], true),
                entry("c4", vec![touch("foo", PlanTouchKind::DoneMove)], false),
            ],
            feedback_files: vec![],
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
            head: Some(sha("c1")),
            plan_files: vec![plan_file("foo", "c1", None, "")],
            history: vec![entry("c1", vec![touch("foo", PlanTouchKind::Intro)], false)],
            feedback_files: vec![],
        };
        let state = derive_state(PathBuf::from("/r"), snap);
        assert!(state.sessions.contains_key(&sess("foo")));
        assert_eq!(state.sessions[&sess("foo")].body, "");
    }

    #[test]
    fn unattributed_chain_at_head_does_not_break_sessions() {
        // The first few commits aren't attributable (pre-Trinity). When
        // the plan-intro commit lands later, the session still gets
        // attribution for that and subsequent walks.
        let snap = DiskSnapshot {
            head: Some(sha("c4")),
            plan_files: vec![plan_file("foo", "c3", Some("c2"), "# foo\n")],
            history: vec![
                entry("c1", vec![], true),
                entry("c2", vec![], true),
                entry("c3", vec![touch("foo", PlanTouchKind::Intro)], false),
                entry("c4", vec![], true),
            ],
            feedback_files: vec![],
        };
        let state = derive_state(PathBuf::from("/r"), snap);
        assert_eq!(state.attribution[&sha("c1")], AttributionResult::Unattributed);
        assert_eq!(state.attribution[&sha("c2")], AttributionResult::Unattributed);
        assert!(matches!(
            state.attribution[&sha("c3")],
            AttributionResult::Attributed { plan_touch: Some(PlanTouchKind::Intro), .. }
        ));
        assert!(matches!(
            state.attribution[&sha("c4")],
            AttributionResult::Attributed {
                plan_touch: None,
                has_code_changes: true,
                ..
            }
        ));
    }

    /// Reusable fixture: foo plan (plan_intro c1) + one impl commit c2,
    /// plus alice's APPROVE on c1 (plan) and bob's REQUEST_CHANGES on c2 (impl).
    fn full_workflow_snapshot() -> DiskSnapshot {
        DiskSnapshot {
            head: Some(sha("c2")),
            plan_files: vec![plan_file("foo", "c1", None, "# foo\n")],
            history: vec![
                entry("c1", vec![touch("foo", PlanTouchKind::Intro)], false),
                entry("c2", vec![], true),
            ],
            feedback_files: vec![
                feedback(
                    "foo",
                    FeedbackPhase::Plan,
                    Some("c1"),
                    "alice",
                    "APPROVE\n",
                ),
                feedback(
                    "foo",
                    FeedbackPhase::Impl,
                    Some("c2"),
                    "bob",
                    "REQUEST_CHANGES\nstuff\n",
                ),
            ],
        }
    }
}
