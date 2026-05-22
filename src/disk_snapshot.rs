//! Daemon-side IO layer for the sans-io fold. Walks git history,
//! builds [`trinity_core::repo_state::CommitEvent`]s, and calls
//! [`trinity_core::repo_state::RepoState::apply_commit`] on each.

use std::collections::BTreeSet;
use std::path::PathBuf;

use crate::disk_format::FeedbackPath;
use crate::git_io::{self, GitIoError};
use crate::lifecycle::{CommitSha, PlanKey};
use crate::repo_state::RepoState;
use trinity_core::repo_state as fold;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CommitSnapshot {
    pub head: Option<CommitSha>,
    pub history: Vec<CommitEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitEvent {
    pub commit: CommitSha,
    pub author_ts: i64,
    pub subject: String,
    pub changes: CommitChanges,
    pub newly_finished: BTreeSet<PlanKey>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CommitChanges {
    pub plan_touches: Vec<PlanTouch>,
    pub has_non_plan_code_changes: bool,
    pub finalize_changes: Vec<FinalizeChange>,
    pub trinity_paths: Vec<String>,
    pub touched_trinity: bool,
    pub trinity_paths_touched: Vec<String>,
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinalizeChange {
    pub plan_key: PlanKey,
    pub file_name: String,
    pub kind: FinalizeChangeKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinalizeChangeKind {
    Upsert { first_line: String },
    Remove,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedbackBlob {
    pub abs_path: PathBuf,
    pub parsed: FeedbackPath,
    pub body: String,
    pub created_at: i64,
}

/// Cold-start derivation: walk HEAD's first-parent chain, apply
/// each commit through the sans-io fold.
pub async fn derive_state(
    repo_root: PathBuf,
    snapshot: CommitSnapshot,
) -> Result<RepoState, GitIoError> {
    let mut state = RepoState::empty(repo_root.clone());
    state.head = snapshot.head.clone();
    for event in &snapshot.history {
        let enriched = enrich_with_newly_finished(&repo_root, event).await?;
        apply_commit(&mut state, &enriched);
    }
    Ok(state)
}

/// Apply one commit to `state.fold`. Pure: every input the fold
/// needs is encoded in `event`.
pub fn apply_commit(state: &mut RepoState, event: &CommitEvent) {
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
            },
        })
        .collect();

    let new_event = fold::CommitEvent {
        sha: event.commit.clone(),
        author_ts: event.author_ts,
        subject: event.subject.clone(),
        plan_touches,
        newly_finished: event.newly_finished.clone(),
        has_code_changes: event.changes.has_non_plan_code_changes,
    };

    state.fold.apply_commit(&new_event);
}

/// Compute the stateless `newly_finished` set for a commit by
/// comparing parent-tree and commit-tree finish predicates.
pub async fn enrich_with_newly_finished(
    repo_root: &std::path::Path,
    event: &CommitEvent,
) -> Result<CommitEvent, GitIoError> {
    let mut candidates: BTreeSet<PlanKey> = BTreeSet::new();
    for fc in &event.changes.finalize_changes {
        candidates.insert(fc.plan_key.clone());
    }
    for touch in &event.changes.plan_touches {
        candidates.insert(touch.plan.clone());
    }
    if candidates.is_empty() {
        return Ok(event.clone());
    }

    let parent = git_io::parent_of(repo_root, &event.commit).await?;
    let mut newly_finished: BTreeSet<PlanKey> = BTreeSet::new();
    for plan in candidates {
        let parent_satisfied = match parent.as_ref() {
            Some(p) => git_io::finish_predicate_at(repo_root, p, &plan).await?,
            None => false,
        };
        let commit_satisfied = git_io::finish_predicate_at(repo_root, &event.commit, &plan).await?;
        if !parent_satisfied && commit_satisfied {
            newly_finished.insert(plan);
        }
    }
    let mut out = event.clone();
    out.newly_finished = newly_finished;
    Ok(out)
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
            new_path: Some(PathBuf::from(format!(".trinity/plans/{p}.md"))),
        }
    }

    fn ev(commit: &str, changes: CommitChanges, newly_finished: BTreeSet<PlanKey>) -> CommitEvent {
        CommitEvent {
            commit: sha(commit),
            author_ts: 0,
            subject: String::new(),
            changes,
            newly_finished,
        }
    }

    #[test]
    fn intro_then_finalize_drives_newly_finished() {
        let mut state = RepoState::empty(PathBuf::from("/r"));
        apply_commit(
            &mut state,
            &ev(
                "1111111111111111111111111111111111111111",
                CommitChanges {
                    plan_touches: vec![intro("foo")],
                    ..Default::default()
                },
                BTreeSet::new(),
            ),
        );
        assert!(state.fold.plans.contains_key(&plan("foo")));

        apply_commit(
            &mut state,
            &ev(
                "2222222222222222222222222222222222222222",
                CommitChanges::default(),
                std::iter::once(plan("foo")).collect(),
            ),
        );
        assert!(!state.fold.plans.contains_key(&plan("foo")));
        assert_eq!(state.fold.finished_plans.len(), 1);
    }
}
