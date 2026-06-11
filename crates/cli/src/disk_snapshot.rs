//! Daemon-side IO layer for the sans-io fold. Walks git history,
//! builds [`clank_core::repo_state::CommitEvent`]s, and calls
//! [`clank_core::repo_state::RepoState::apply_commit`] on each.

use std::path::PathBuf;

use crate::disk_format::FeedbackPath;
use crate::lifecycle::{CommitSha, PlanKey};
use crate::repo_state::RepoState;
use clank_core::repo_state as fold;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitEvent {
    pub commit: CommitSha,
    pub author_ts: i64,
    pub subject: String,
    pub changes: CommitChanges,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CommitChanges {
    pub plan_touches: Vec<PlanTouch>,
    pub has_non_plan_code_changes: bool,
    pub clank_paths: Vec<String>,
    pub touched_clank: bool,
    pub clank_paths_touched: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanTouch {
    pub plan: PlanKey,
    pub kind: PlanTouchKind,
    pub new_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanTouchKind {
    Intro,
    Revision,
    Finish,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedbackBlob {
    pub abs_path: PathBuf,
    pub parsed: FeedbackPath,
    pub body: String,
    pub created_at: i64,
}

pub fn apply_commit(state: &mut RepoState, event: &CommitEvent) -> Vec<fold::LogEvent> {
    let plan_touches: Vec<fold::PlanTouchInput> = event
        .changes
        .plan_touches
        .iter()
        .map(|t| fold::PlanTouchInput {
            plan: t.plan.clone(),
            kind: match (t.kind, t.new_path.is_some()) {
                (PlanTouchKind::Intro, true) => fold::TouchKind::Intro,
                (PlanTouchKind::Intro, false) => fold::TouchKind::Delete,
                (PlanTouchKind::Revision, true) => fold::TouchKind::Revise,
                (PlanTouchKind::Revision, false) => fold::TouchKind::Delete,
                (PlanTouchKind::Finish, _) => fold::TouchKind::Finish,
            },
        })
        .collect();

    let new_event = fold::CommitEvent {
        sha: event.commit.clone(),
        author_ts: event.author_ts,
        subject: event.subject.clone(),
        plan_touches,
        has_code_changes: event.changes.has_non_plan_code_changes,
    };

    state.fold.apply_commit(&new_event)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sha(s: &str) -> CommitSha {
        CommitSha::parse(s).unwrap()
    }
    fn plan(s: &str) -> PlanKey {
        PlanKey::parse(s).unwrap()
    }

    fn intro(p: &str) -> PlanTouch {
        PlanTouch {
            plan: plan(p),
            kind: PlanTouchKind::Intro,
            new_path: Some(PathBuf::from(format!(".clank/plans/{p}.md"))),
        }
    }

    fn ev(commit: &str, changes: CommitChanges) -> CommitEvent {
        CommitEvent {
            commit: sha(commit),
            author_ts: 0,
            subject: String::new(),
            changes,
        }
    }

    fn finish_touch(p: &str) -> PlanTouch {
        PlanTouch {
            plan: plan(p),
            kind: PlanTouchKind::Finish,
            new_path: None,
        }
    }

    #[test]
    fn intro_then_finish_archives_plan() {
        let mut state = RepoState::empty(PathBuf::from("/r"));
        apply_commit(
            &mut state,
            &ev(
                "1111111111111111111111111111111111111111",
                CommitChanges {
                    plan_touches: vec![intro("foo")],
                    ..Default::default()
                },
            ),
        );
        assert!(state.fold.plans.contains_key(&plan("foo")));

        apply_commit(
            &mut state,
            &ev(
                "2222222222222222222222222222222222222222",
                CommitChanges {
                    plan_touches: vec![finish_touch("foo")],
                    ..Default::default()
                },
            ),
        );
        assert!(!state.fold.plans.contains_key(&plan("foo")));
        assert_eq!(state.fold.finished_plans.len(), 1);
    }
}
