//! Serial admission to a repo's one active-plan slot.
//!
//! Admission paths (promote, restore, plan-preserving pick and unfinish) hold
//! this lock from the fresh fold through the Git mutation. Stash push, finalize
//! and purge hold it while changing/emptying the slot. The kernel
//! parks contenders; there is no polling, helper process or idle work. Locks
//! live in the existing uid runtime namespace, never in the working tree.

use std::path::{Path, PathBuf};

use anyhow::Context;

pub(crate) struct PlanMutationLock {
    _file: std::fs::File,
    repo: PathBuf,
}

pub(crate) fn lock(repo: &Path) -> anyhow::Result<PlanMutationLock> {
    let (file, repo) = runtime_lock(repo, "plan-mutation")?;
    Ok(PlanMutationLock { _file: file, repo })
}

/// Hold this repo's `name` lock in the uid runtime namespace, blocking
/// until it is free, and return it with the canonical repo it is keyed
/// by: CLI, TUI, aliases and linked callers all reach the same inode.
pub(crate) fn runtime_lock(repo: &Path, name: &str) -> anyhow::Result<(std::fs::File, PathBuf)> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;

    let repo = dunce::canonicalize(repo)
        .with_context(|| format!("resolving repo `{}`", repo.display()))?;
    super::monitor::ensure_runtime_dir().map_err(anyhow::Error::msg)?;
    let path = super::monitor::runtime_path(&repo, name);
    // Never unlink in production: replacing a locked inode could admit a
    // second holder at the same name while the first still holds it.
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .with_context(|| format!("opening lock `{}`", path.display()))?;
    loop {
        // SAFETY: valid owned fd; flock has no memory effects.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
            return Ok((file, repo));
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error).with_context(|| format!("locking `{}`", path.display()));
        }
    }
}

impl PlanMutationLock {
    pub(crate) fn state(&self) -> anyhow::Result<crate::repo_state::RepoState> {
        crate::rebuild::rebuild_repo_sync_with_policy(&self.repo, crate::rebuild::CachePolicy::Use)
            .with_context(|| format!("folding repo `{}`", self.repo.display()))
    }

    /// Test the same repo whose lock is held, before consuming an input. Cached
    /// rebuilds resume from HEAD; rejection does not inspect blocks/reviewer
    /// verdicts because an occupied slot is occupied even while blocked.
    pub(crate) fn empty_state(&self) -> anyhow::Result<crate::repo_state::RepoState> {
        let state = self.state()?;
        if let Some(active) = &state.fold.active_plan {
            anyhow::bail!(
                "plan `{}` is already active; finish or stash it before introducing another plan",
                active.key
            );
        }
        Ok(state)
    }
}
