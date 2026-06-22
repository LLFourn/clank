//! CLI-side IO: compare a plan file's working-tree copy to its
//! HEAD blob, returning a typed [`WorktreeFacts`] core can project
//! over.

use std::path::Path;

use crate::lifecycle::CommitSha;
use clank_core::plan_view::WorktreeFacts;
use clank_core::vocab::PlanWorktreeStatus;

/// Compare `<repo>/<plan_path>` to `<head>:<plan_path>`. The blob is
/// the source of truth; a missing worktree file with a blob in HEAD
/// is `PlanFileMissing`. No HEAD (empty repo) or an unreadable repo
/// is `Clean`. Delegates to [`body_status_vs_commit`] — the one
/// definition shared with the status-derive path.
pub async fn read_worktree_facts(
    repo: &Path,
    plan_path: &str,
    head: Option<&CommitSha>,
) -> WorktreeFacts {
    let status = match head {
        Some(h) => match gix::open(repo) {
            Ok(git) => body_status_vs_commit(&git, h, plan_path),
            Err(_) => PlanWorktreeStatus::Clean,
        },
        None => PlanWorktreeStatus::Clean,
    };
    WorktreeFacts { status }
}

/// The one definition of "is `rel_path`'s worktree copy dirty vs its
/// blob at `commit`", shared by the status-derive path
/// (`FsPlanStateLookup::worktree_status`) and the preview path. The
/// blob is the source of truth: a worktree file that differs is
/// `BodyDirty`; a missing worktree file whose blob exists at `commit`
/// is `PlanFileMissing`; a path absent at `commit` is `Clean`
/// regardless of the worktree (matching `git diff HEAD`, which
/// ignores paths not in the commit). Takes an already-open handle so
/// callers comparing many plans reuse one ODB.
pub fn body_status_vs_commit(
    git: &gix::Repository,
    commit: &CommitSha,
    rel_path: &str,
) -> PlanWorktreeStatus {
    let worktree = git
        .workdir()
        .map(|w| w.join(rel_path))
        .and_then(|abs| std::fs::read(abs).ok());
    match (commit_blob_bytes(git, commit, rel_path), worktree) {
        (Some(b), Some(w)) if b == w => PlanWorktreeStatus::Clean,
        (Some(_), Some(_)) => PlanWorktreeStatus::BodyDirty,
        (Some(_), None) => PlanWorktreeStatus::PlanFileMissing,
        (None, _) => PlanWorktreeStatus::Clean,
    }
}

/// `rel_path`'s blob bytes at `commit`, or `None` if the commit/path
/// can't be resolved (absent path, bad oid, unreadable object).
fn commit_blob_bytes(git: &gix::Repository, commit: &CommitSha, rel_path: &str) -> Option<Vec<u8>> {
    let oid = gix::ObjectId::from_hex(commit.as_str().as_bytes()).ok()?;
    let tree = git.find_commit(oid).ok()?.tree().ok()?;
    let entry = tree.lookup_entry_by_path(rel_path).ok().flatten()?;
    Some(git.find_blob(entry.oid()).ok()?.data.clone())
}
