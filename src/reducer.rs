//! Pure reducer for the filesystem-truth model. Observations come in;
//! `Effect` lists come out. No I/O.
//!
//! See `.trinity/plans/filesystem-truth-rewrite.md` for the full architecture.
//!
//! Three properties enforced here:
//! - `step` is the only state-mutation point in the daemon.
//! - `step` is pure. The caller supplies the current time; the caller
//!   runs effects against the filesystem.
//! - Effects are filesystem operations + broadcasts + a rebuild signal.
//!   Never SQL, never git index writes, never commits, never amends.

use std::path::PathBuf;

use crate::disk_format::FeedbackPhase;
use crate::lifecycle::{AgentLabel, CommitSha, SessionId};
use crate::repo_state::{LiveEvent, RepoRoot, RepoState};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Observation {
    /// Any change to `.git/HEAD` or `.git/logs/HEAD`. Triggers a full
    /// repo rebuild (cold-start-style) since incremental walks aren't safe
    /// across non-linear HEAD movements (branch switch, reset, rebase).
    HeadChanged,

    /// Narrow: a working-tree change to a plan file in a session that
    /// already exists in HEAD. Updates `plan_worktree_status`-derived
    /// signals and rebroadcasts `waiting_on`. Does **not** trigger a
    /// rebuild, never creates a session, never records a plan revision.
    /// Untracked-draft plan files (no matching HEAD entry) drop here.
    PlanWorktreeMaybeChanged { session_id: SessionId },

    FeedbackChanged {
        session_id: SessionId,
        phase: FeedbackPhase,
        target_sha: Option<CommitSha>,
        author: AgentLabel,
        body: String,
        path: PathBuf,
    },

    FeedbackGone {
        session_id: SessionId,
        phase: FeedbackPhase,
        target_sha: Option<CommitSha>,
        author: AgentLabel,
    },

    /// Operator clicked "move to done" in the UI / hit the HTTP route.
    /// The route runs a working-tree `std::fs::rename`; Trinity does not
    /// stage or commit. The HEAD change after the operator commits drives
    /// the eventual `HeadChanged` rebuild.
    OperatorMoveToDone { session_id: SessionId },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Auto-organize a flat-path plan/impl feedback file under the current
    /// target SHA's directory. Emitted by the reducer on `FeedbackChanged`
    /// for flat drops, and by the rebuild's normalization sweep for
    /// previously-held files now that the plan is clean.
    MoveFeedbackFile { from: PathBuf, to: PathBuf },

    /// Plain `std::fs::rename` from `.trinity/plans/<id>.md` to
    /// `.trinity/plans/done/<id>.md`. Operator stages and commits.
    /// **Never** runs `git mv` or touches the index.
    MovePlanToDone { from: PathBuf, to: PathBuf },

    /// Trigger a full repo rebuild (recompute attribution, reload feedback,
    /// run the held-feedback normalization sweep).
    RebuildRepo { repo: RepoRoot },

    /// Append to the live-events ring buffer and broadcast on SSE.
    /// Ephemeral; restart drops the ring.
    BroadcastLive { event: LiveEvent },
}

/// Apply a single `Observation` to a `RepoState`. The repo's invariants
/// are the only thing this function mutates; everything else (filesystem
/// edits, SSE broadcasts, rebuild kicks) is returned as an effect for the
/// runner to apply.
///
/// `now` is the timestamp the caller wants to use for any emitted live
/// events. The reducer never reads the clock.
pub fn step(_repo: &mut RepoState, obs: Observation, _now: i64) -> Vec<Effect> {
    // First-cut implementation. The full implementation is added in step
    // 2 of the rewrite plan (new watchers + main loop). For now this
    // returns the canonical effect shape so the types compile and tests
    // can exercise the surface.
    match obs {
        Observation::HeadChanged => vec![Effect::RebuildRepo {
            // Real implementation will copy the repo root from `_repo.root`;
            // for the type-level pass, return an empty path.
            repo: PathBuf::new(),
        }],
        Observation::PlanWorktreeMaybeChanged { .. } => Vec::new(),
        Observation::FeedbackChanged { .. } => Vec::new(),
        Observation::FeedbackGone { .. } => Vec::new(),
        Observation::OperatorMoveToDone { .. } => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo_state::RepoState;
    use std::path::PathBuf;

    fn empty_state() -> RepoState {
        RepoState::empty(PathBuf::from("/repo"))
    }

    #[test]
    fn head_changed_emits_rebuild_repo() {
        let mut state = empty_state();
        let effects = step(&mut state, Observation::HeadChanged, 0);
        assert_eq!(effects.len(), 1);
        assert!(matches!(effects[0], Effect::RebuildRepo { .. }));
    }

    #[test]
    fn plan_worktree_maybe_changed_emits_nothing_yet() {
        // Step-1 placeholder: the full handler (recompute status + broadcast)
        // lands in step 2. For now, assert the shape compiles.
        let mut state = empty_state();
        let effects = step(
            &mut state,
            Observation::PlanWorktreeMaybeChanged {
                session_id: SessionId::from("foo".to_string()),
            },
            0,
        );
        assert!(effects.is_empty());
    }

}
