//! Per-plan projection: gate state + who's blocking + worktree
//! state. Pure — `project` takes a typed `FeedbackView` and
//! `WorktreeFacts` from the CLI and synthesizes everything else
//! from the fold.
//!
//! Both `clank status` and `clank wfw` consume this projection;
//! there is no parallel computation of gate/waiting_on anywhere
//! else.

use serde::{Deserialize, Serialize};

use crate::feedback_view::FeedbackView;
use crate::ids::{AgentLabel, CommitSha, PlanKey};
use crate::repo_state::{NonEmptyVec, RepoState};
use crate::vocab::{CommitGateState, PlanWorktreeStatus, Verdict};

/// Live worktree facts the CLI computes for one plan's plan file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeFacts {
    pub status: PlanWorktreeStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanView {
    pub plan: PlanKey,
    pub latest_reviewable_sha: CommitSha,
    /// Every reviewable commit on this plan's timeline, in
    /// chronological order. The set the
    /// short-vs-long-feedback-filename mode is decided over.
    pub reviewable_shas: Vec<CommitSha>,
    pub gate_state: CommitGateState,
    pub waiting_on: WaitingOn,
    pub worktree_status: PlanWorktreeStatus,
    pub last_activity_ts: i64,
}

/// Human-meaningful summary of "what's blocking this plan."
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WaitingOn {
    /// Nobody has reviewed any commit in this plan yet. Any
    /// reviewer agent is eligible.
    FirstReview,
    /// Latest reviewable commit lacks feedback from one or more
    /// existing participants. `missing` is non-empty by construction
    /// — the empty case is `FirstReview`.
    ReviewerApprovalsMissing { missing: NonEmptyVec<AgentLabel> },
    /// A reviewer requested changes (or left an ambiguous verdict).
    /// Master needs to address.
    MasterToRevise {
        requesters: Vec<AgentLabel>,
        ambiguous: Vec<AgentLabel>,
    },
    /// Gate is approved on a plan-only commit (the approved
    /// commit didn't attribute code to this plan). Next move is
    /// the implementation under `[<stem>]`.
    MasterToImplement,
    /// Gate is approved on a code-touching commit and the plan
    /// worktree is clean — master just needs to run `clank finish`.
    MasterToFinalize,
    /// Gate is approved but the plan file has uncommitted edits.
    /// Master needs to commit the next revision.
    MasterToCommit,
}

/// Project a plan into the human-meaningful view both surfaces
/// share. Returns `None` if the plan has no reviewable commit
/// yet (which never happens for an active plan — the intro itself
/// touches the plan file — but the type stays honest).
pub fn project(
    state: &RepoState,
    plan_key: &PlanKey,
    feedback: &FeedbackView,
    worktree: &WorktreeFacts,
) -> Option<PlanView> {
    let ps = state.plans.get(plan_key)?;

    let latest = ps
        .commits
        .iter()
        .rev()
        .find(|e| e.touched_plan || e.touched_code)?;

    let last_activity_ts = ps.commits.iter().map(|e| e.ts).max().unwrap_or(latest.ts);

    let reviewable_shas: Vec<CommitSha> = ps.reviewable_shas();

    let (gate_state, waiting_on) =
        evaluate(feedback, &latest.sha, latest.touched_code, worktree.status);

    Some(PlanView {
        plan: plan_key.clone(),
        latest_reviewable_sha: latest.sha.clone(),
        reviewable_shas,
        gate_state,
        waiting_on,
        worktree_status: worktree.status,
        last_activity_ts,
    })
}

/// Compute `(gate_state, waiting_on)` from the cumulative
/// participant set, the latest reviewable commit's verdicts, and
/// the boolean flags that say what kind of change that commit was.
/// `latest_touched_code = true` means the approved commit
/// attributed code to this plan; that routes Approved+Clean to
/// `MasterToFinalize`. Otherwise (the approved commit only touched
/// the plan file) the route is `MasterToImplement`.
fn evaluate(
    feedback: &FeedbackView,
    target_sha: &CommitSha,
    latest_touched_code: bool,
    worktree: PlanWorktreeStatus,
) -> (CommitGateState, WaitingOn) {
    let mut participants: Vec<AgentLabel> = Vec::new();
    let mut target_entries: Option<
        &std::collections::BTreeMap<AgentLabel, crate::feedback_view::FeedbackEntry>,
    > = None;

    for commit in &feedback.per_commit {
        for author in commit.entries.keys() {
            if !participants.contains(author) {
                participants.push(author.clone());
            }
        }
        if &commit.sha == target_sha {
            target_entries = Some(&commit.entries);
        }
    }

    let empty = std::collections::BTreeMap::new();
    let target = target_entries.unwrap_or(&empty);

    if participants.is_empty() {
        return (CommitGateState::Unreviewed, WaitingOn::FirstReview);
    }

    let mut approvers: Vec<AgentLabel> = Vec::new();
    let mut requesters: Vec<AgentLabel> = Vec::new();
    let mut ambiguous: Vec<AgentLabel> = Vec::new();
    let mut missing: Vec<AgentLabel> = Vec::new();
    for p in &participants {
        match target.get(p).map(|e| e.verdict) {
            Some(Verdict::Approve) => approvers.push(p.clone()),
            Some(Verdict::RequestChanges) => requesters.push(p.clone()),
            Some(Verdict::Unmarked) => ambiguous.push(p.clone()),
            None => missing.push(p.clone()),
        }
    }

    let gate_state = if !requesters.is_empty() || !ambiguous.is_empty() {
        CommitGateState::ChangesRequested
    } else if !approvers.is_empty() && missing.is_empty() {
        CommitGateState::Approved
    } else {
        CommitGateState::Unreviewed
    };

    let waiting_on = match gate_state {
        CommitGateState::ChangesRequested => WaitingOn::MasterToRevise {
            requesters,
            ambiguous,
        },
        CommitGateState::Approved => match worktree {
            PlanWorktreeStatus::BodyDirty => WaitingOn::MasterToCommit,
            PlanWorktreeStatus::Clean | PlanWorktreeStatus::PlanFileMissing => {
                if latest_touched_code {
                    WaitingOn::MasterToFinalize
                } else {
                    WaitingOn::MasterToImplement
                }
            }
        },
        CommitGateState::Unreviewed => {
            let missing_nev = NonEmptyVec::new(missing)
                .expect("missing non-empty: participants exist and at least one hasn't voted");
            WaitingOn::ReviewerApprovalsMissing {
                missing: missing_nev,
            }
        }
    };

    (gate_state, waiting_on)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feedback_view::{CommitFeedback, FeedbackEntry};
    use crate::ids::ContentHash;
    use crate::repo_state::{PlanState, PlanTimelineEvent};
    use std::collections::BTreeMap;

    fn plan(s: &str) -> PlanKey {
        PlanKey::parse(s).unwrap()
    }
    fn sha(s: &str) -> CommitSha {
        CommitSha::parse(&format!("{s:0<40}")).unwrap()
    }
    fn label(s: &str) -> AgentLabel {
        AgentLabel::parse(s).unwrap()
    }
    fn ch() -> ContentHash {
        ContentHash::parse(&"a".repeat(64)).unwrap()
    }
    fn entry(v: Verdict) -> FeedbackEntry {
        FeedbackEntry {
            verdict: v,
            body_hash: ch(),
            source_path: "ignored".into(),
        }
    }

    fn state_with_one_plan(key: &PlanKey, events: Vec<PlanTimelineEvent>) -> RepoState {
        let mut s = RepoState::default();
        s.plans.insert(key.clone(), PlanState { commits: events });
        s
    }

    fn evt(sha_hex: &str, ts: i64, touched_plan: bool, touched_code: bool) -> PlanTimelineEvent {
        PlanTimelineEvent {
            sha: sha(sha_hex),
            ts,
            touched_plan,
            touched_code,
        }
    }

    #[test]
    fn first_review_when_no_feedback_exists() {
        let key = plan("foo");
        let s = state_with_one_plan(&key, vec![evt("1111", 1, true, false)]);
        let fb = FeedbackView::default();
        let wt = WorktreeFacts {
            status: PlanWorktreeStatus::Clean,
        };
        let view = project(&s, &key, &fb, &wt).unwrap();
        assert_eq!(view.gate_state, CommitGateState::Unreviewed);
        assert!(matches!(view.waiting_on, WaitingOn::FirstReview));
        assert_eq!(view.latest_reviewable_sha, sha("1111"));
    }

    #[test]
    fn reviewer_approvals_missing_lists_only_missing_participants() {
        let key = plan("foo");
        // Two reviewable commits. alice and bob reviewed the first;
        // alice reviewed the second. bob is missing on the latest.
        let s = state_with_one_plan(
            &key,
            vec![evt("1111", 1, true, false), evt("2222", 2, false, true)],
        );
        let mut alice1 = BTreeMap::new();
        alice1.insert(label("alice"), entry(Verdict::Approve));
        alice1.insert(label("bob"), entry(Verdict::Approve));
        let mut alice2 = BTreeMap::new();
        alice2.insert(label("alice"), entry(Verdict::Approve));
        let fb = FeedbackView {
            per_commit: vec![
                CommitFeedback {
                    sha: sha("1111"),
                    entries: alice1,
                },
                CommitFeedback {
                    sha: sha("2222"),
                    entries: alice2,
                },
            ],
        };
        let view = project(
            &s,
            &key,
            &fb,
            &WorktreeFacts {
                status: PlanWorktreeStatus::Clean,
            },
        )
        .unwrap();
        assert_eq!(view.gate_state, CommitGateState::Unreviewed);
        match view.waiting_on {
            WaitingOn::ReviewerApprovalsMissing { missing } => {
                assert_eq!(missing.len(), 1);
                assert_eq!(missing.first(), &label("bob"));
            }
            other => panic!("expected ReviewerApprovalsMissing, got {other:?}"),
        }
    }

    #[test]
    fn master_to_revise_on_request_changes() {
        let key = plan("foo");
        let s = state_with_one_plan(&key, vec![evt("1111", 1, true, false)]);
        let mut entries = BTreeMap::new();
        entries.insert(label("alice"), entry(Verdict::RequestChanges));
        let fb = FeedbackView {
            per_commit: vec![CommitFeedback {
                sha: sha("1111"),
                entries,
            }],
        };
        let view = project(
            &s,
            &key,
            &fb,
            &WorktreeFacts {
                status: PlanWorktreeStatus::Clean,
            },
        )
        .unwrap();
        assert_eq!(view.gate_state, CommitGateState::ChangesRequested);
        assert!(matches!(view.waiting_on, WaitingOn::MasterToRevise { .. }));
    }

    #[test]
    fn approved_plan_only_routes_to_master_to_implement() {
        let key = plan("foo");
        let s = state_with_one_plan(&key, vec![evt("1111", 1, true, false)]);
        let mut entries = BTreeMap::new();
        entries.insert(label("alice"), entry(Verdict::Approve));
        let fb = FeedbackView {
            per_commit: vec![CommitFeedback {
                sha: sha("1111"),
                entries,
            }],
        };
        let view = project(
            &s,
            &key,
            &fb,
            &WorktreeFacts {
                status: PlanWorktreeStatus::Clean,
            },
        )
        .unwrap();
        assert_eq!(view.gate_state, CommitGateState::Approved);
        assert!(matches!(view.waiting_on, WaitingOn::MasterToImplement));
    }

    #[test]
    fn approved_code_only_routes_to_master_to_finalize() {
        let key = plan("foo");
        let s = state_with_one_plan(
            &key,
            vec![
                evt("1111", 1, true, false), // plan intro
                evt("2222", 2, false, true), // pure impl commit
            ],
        );
        let mut entries = BTreeMap::new();
        entries.insert(label("alice"), entry(Verdict::Approve));
        let fb = FeedbackView {
            per_commit: vec![
                CommitFeedback {
                    sha: sha("1111"),
                    entries: {
                        let mut m = BTreeMap::new();
                        m.insert(label("alice"), entry(Verdict::Approve));
                        m
                    },
                },
                CommitFeedback {
                    sha: sha("2222"),
                    entries,
                },
            ],
        };
        let view = project(
            &s,
            &key,
            &fb,
            &WorktreeFacts {
                status: PlanWorktreeStatus::Clean,
            },
        )
        .unwrap();
        assert_eq!(view.gate_state, CommitGateState::Approved);
        assert!(matches!(view.waiting_on, WaitingOn::MasterToFinalize));
    }

    #[test]
    fn approved_mixed_commit_routes_to_master_to_finalize() {
        // Mixed commit (touched_plan + touched_code). Code
        // attribution wins — once code is approved, finalize is
        // on the table.
        let key = plan("foo");
        let s = state_with_one_plan(&key, vec![evt("1111", 1, true, true)]);
        let mut entries = BTreeMap::new();
        entries.insert(label("alice"), entry(Verdict::Approve));
        let fb = FeedbackView {
            per_commit: vec![CommitFeedback {
                sha: sha("1111"),
                entries,
            }],
        };
        let view = project(
            &s,
            &key,
            &fb,
            &WorktreeFacts {
                status: PlanWorktreeStatus::Clean,
            },
        )
        .unwrap();
        assert!(matches!(view.waiting_on, WaitingOn::MasterToFinalize));
    }

    #[test]
    fn master_to_commit_when_approved_and_dirty() {
        let key = plan("foo");
        let s = state_with_one_plan(&key, vec![evt("1111", 1, true, false)]);
        let mut entries = BTreeMap::new();
        entries.insert(label("alice"), entry(Verdict::Approve));
        let fb = FeedbackView {
            per_commit: vec![CommitFeedback {
                sha: sha("1111"),
                entries,
            }],
        };
        let view = project(
            &s,
            &key,
            &fb,
            &WorktreeFacts {
                status: PlanWorktreeStatus::BodyDirty,
            },
        )
        .unwrap();
        assert_eq!(view.gate_state, CommitGateState::Approved);
        assert!(matches!(view.waiting_on, WaitingOn::MasterToCommit));
    }

    #[test]
    fn ambiguous_verdict_routes_to_master_to_revise() {
        let key = plan("foo");
        let s = state_with_one_plan(&key, vec![evt("1111", 1, true, false)]);
        let mut entries = BTreeMap::new();
        entries.insert(label("alice"), entry(Verdict::Unmarked));
        let fb = FeedbackView {
            per_commit: vec![CommitFeedback {
                sha: sha("1111"),
                entries,
            }],
        };
        let view = project(
            &s,
            &key,
            &fb,
            &WorktreeFacts {
                status: PlanWorktreeStatus::Clean,
            },
        )
        .unwrap();
        match view.waiting_on {
            WaitingOn::MasterToRevise {
                requesters,
                ambiguous,
            } => {
                assert!(requesters.is_empty());
                assert_eq!(ambiguous, vec![label("alice")]);
            }
            other => panic!("expected MasterToRevise, got {other:?}"),
        }
    }

    #[test]
    fn missing_plan_returns_none() {
        let s = RepoState::default();
        let view = project(
            &s,
            &plan("ghost"),
            &FeedbackView::default(),
            &WorktreeFacts {
                status: PlanWorktreeStatus::Clean,
            },
        );
        assert!(view.is_none());
    }
}
