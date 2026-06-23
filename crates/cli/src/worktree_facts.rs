//! CLI-side IO: compare a plan file's working-tree copy to its
//! HEAD blob, returning a typed [`WorktreeFacts`] core can project
//! over. The git access itself lives in [`crate::git_io`] (the gix
//! boundary); this is just the async projection over it.

use std::path::Path;

use crate::git_io;
use crate::lifecycle::CommitSha;
use clank_core::plan_view::WorktreeFacts;
use clank_core::vocab::PlanWorktreeStatus;

/// Compare `<repo>/<plan_path>` to `<head>:<plan_path>`. The blob is
/// the source of truth; a missing worktree file with a blob in HEAD
/// is `PlanFileMissing`. No HEAD (empty repo) or an unreadable repo
/// is `Clean`. Delegates to [`git_io::plan_body_status`] — the one
/// definition shared with the status-derive path.
pub async fn read_worktree_facts(
    repo: &Path,
    plan_path: &str,
    head: Option<&CommitSha>,
) -> WorktreeFacts {
    let status = match head {
        Some(h) => match git_io::open(repo) {
            Ok(git) => git.plan_body_status(h, plan_path),
            // An unreadable repo can't have a blob to be dirty against.
            Err(_) => PlanWorktreeStatus::Clean,
        },
        None => PlanWorktreeStatus::Clean,
    };
    WorktreeFacts { status }
}
