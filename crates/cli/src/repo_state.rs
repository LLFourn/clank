//! The pure fold with repository identity supplied by I/O.

use std::path::PathBuf;

use crate::lifecycle::CommitSha;

pub use clank_core::PlanLifecycle;
pub use clank_core::PlanWorktreeStatus;
pub use clank_core::Verdict;
pub use clank_core::WaitingReason;

/// Cache payloads carry only the pure fold; repository root and head
/// are supplied when loading it for a command or monitor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoState {
    pub root: PathBuf,
    pub head: Option<CommitSha>,
    pub fold: clank_core::repo_state::RepoState,
}

impl RepoState {
    pub fn empty(root: PathBuf) -> Self {
        Self {
            root,
            head: None,
            fold: clank_core::repo_state::RepoState::default(),
        }
    }
}
