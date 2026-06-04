//! CLI-side IO: compare a plan file's working-tree copy to its
//! HEAD blob, returning a typed [`WorktreeFacts`] core can project
//! over.

use std::path::Path;

use crate::git_io::{self, GitIoError};
use crate::lifecycle::CommitSha;
use clank_core::plan_view::WorktreeFacts;
use clank_core::vocab::PlanWorktreeStatus;

#[derive(Debug, thiserror::Error)]
pub enum WorktreeFactsError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("git: {0}")]
    Git(#[from] GitIoError),
}

/// Compare `<repo>/<plan_path>` to `<head>:<plan_path>`. The blob
/// is the source of truth; a missing worktree file with a blob in
/// HEAD is `PlanFileMissing`. No HEAD (empty repo) is `Clean`.
pub async fn read_worktree_facts(
    repo: &Path,
    plan_path: &str,
    head: Option<&CommitSha>,
) -> Result<WorktreeFacts, WorktreeFactsError> {
    let abs = repo.join(plan_path);
    let worktree_body = match std::fs::read_to_string(&abs) {
        Ok(b) => Some(b),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    let head_body = match head {
        Some(h) => git_io::show_blob(repo, h, Path::new(plan_path)).ok(),
        None => None,
    };
    let status = match (head_body.as_deref(), worktree_body.as_deref()) {
        (Some(h), Some(w)) if h == w => PlanWorktreeStatus::Clean,
        (Some(_), Some(_)) => PlanWorktreeStatus::BodyDirty,
        (Some(_), None) => PlanWorktreeStatus::PlanFileMissing,
        (None, _) => PlanWorktreeStatus::Clean,
    };
    Ok(WorktreeFacts { status })
}
