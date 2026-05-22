//! Agent-perspective work derivation.
//!
//! Pure filter over `&[PlanView]`: given an author and role, return
//! the actionable work for that agent right now. `clank wfw`
//! consumes this; `clank status` does not (status is repo-wide and
//! role-agnostic).

use serde::{Deserialize, Serialize};

use crate::ids::{AgentLabel, CommitSha, PlanKey};
use crate::plan_view::{PlanView, WaitingOn};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Master,
    Reviewers,
}

/// What master should do next on a plan whose gate has flipped
/// out from under them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MasterNext {
    /// Reviewer asked for changes — revise the plan / code.
    Revise,
    /// Plan-file dirty in the worktree — commit the next revision.
    Commit,
    /// Gate approved + worktree clean — run `clank finish`.
    Finalize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkItem {
    /// Plan owner has the next move.
    MasterAction {
        plan: PlanKey,
        sha: CommitSha,
        next: MasterNext,
    },
    /// Reviewer has a commit to look at; `feedback_path` is where
    /// to write the verdict.
    ReviewerAction {
        plan: PlanKey,
        sha: CommitSha,
        feedback_path: String,
    },
}

pub fn derive_work(
    views: &[PlanView],
    author: &AgentLabel,
    role: Role,
) -> Vec<WorkItem> {
    let mut out = Vec::new();
    for view in views {
        match (role, &view.waiting_on) {
            (Role::Master, WaitingOn::MasterToRevise { .. }) => {
                out.push(WorkItem::MasterAction {
                    plan: view.plan.clone(),
                    sha: view.latest_reviewable_sha.clone(),
                    next: MasterNext::Revise,
                });
            }
            (Role::Master, WaitingOn::MasterToCommit) => {
                out.push(WorkItem::MasterAction {
                    plan: view.plan.clone(),
                    sha: view.latest_reviewable_sha.clone(),
                    next: MasterNext::Commit,
                });
            }
            (Role::Master, WaitingOn::MasterToFinalize) => {
                out.push(WorkItem::MasterAction {
                    plan: view.plan.clone(),
                    sha: view.latest_reviewable_sha.clone(),
                    next: MasterNext::Finalize,
                });
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

fn reviewer_item(view: &PlanView, author: &AgentLabel) -> WorkItem {
    let path = format!(
        ".clank/feedback/{}/{}/{}.md",
        view.plan.as_str(),
        view.latest_reviewable_sha.as_str(),
        author.as_str(),
    );
    WorkItem::ReviewerAction {
        plan: view.plan.clone(),
        sha: view.latest_reviewable_sha.clone(),
        feedback_path: path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo_state::NonEmptyVec;
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
        PlanView {
            plan: plan(plan_key),
            latest_reviewable_sha: sha(sha_hex),
            gate_state: CommitGateState::Unreviewed,
            waiting_on: waiting,
            worktree_status: PlanWorktreeStatus::Clean,
            last_activity_ts: 0,
        }
    }

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
            WorkItem::MasterAction {
                next: MasterNext::Finalize,
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
            WorkItem::ReviewerAction { feedback_path, .. } => {
                assert!(feedback_path.ends_with("/anyone.md"));
            }
            other => panic!("expected ReviewerAction, got {other:?}"),
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
    fn reviewer_ineligible_returns_empty() {
        let views = vec![view("a", "aaaa", WaitingOn::MasterToFinalize)];
        let out = derive_work(&views, &label("alice"), Role::Reviewers);
        assert!(out.is_empty());
    }

    #[test]
    fn master_skips_reviewer_states() {
        let views = vec![view("a", "aaaa", WaitingOn::FirstReview)];
        let out = derive_work(&views, &label("master"), Role::Master);
        assert!(out.is_empty());
    }

    #[test]
    fn reviewer_feedback_path_canonical() {
        let views = vec![view("foo", "abcd", WaitingOn::FirstReview)];
        let out = derive_work(&views, &label("alice"), Role::Reviewers);
        match &out[0] {
            WorkItem::ReviewerAction { feedback_path, .. } => {
                assert!(
                    feedback_path.starts_with(".clank/feedback/foo/abcd"),
                    "got {feedback_path}"
                );
                assert!(feedback_path.ends_with("/alice.md"));
            }
            _ => unreachable!(),
        }
    }
}
