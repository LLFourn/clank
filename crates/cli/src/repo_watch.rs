//! The shared repo-STATE watcher: the single wake producer for the
//! gate projection, used by both `clank wait` and `clank status`.
//!
//! It watches exactly the inputs the gate is derived from — the
//! resolved gitdir (HEAD / refs / commits) and the workflow dirs under
//! `.clank/` ([`CLANK_WAKE_DIRS`]) — and NOTHING else. It never watches
//! the working tree, so it is storm-safe by construction (no
//! `build/`/`target/` churn can reach it).
//!
//! `status --tui` additionally runs a SEPARATE diff watcher (in
//! `cli::status`) over the working tree to refresh dirty/diff lines;
//! that one is NOT a source of gate state. This module is the part the
//! two commands share so "who is the waiting-upon agent" is derived
//! from one watcher.

use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

use notify::{RecommendedWatcher, RecursiveMode, Watcher};

/// The first component under `.clank/` that carries a GATE signal. An
/// ALLOWLIST, not "anything under `.clank`": the derived `cache`/`html`,
/// the `zellij` layout, queue `drafts`, and nested worktrees under
/// `.clank/worktrees/<name>/` (whole separate repos with their own
/// `target/` + `.clank/cache`) must NOT wake the loop
/// (status-tui-watch-cpu). The single source of truth for which
/// `.clank` paths matter — `status`'s reuse fingerprint keys on the
/// same set.
pub const CLANK_WAKE_DIRS: &[&str] = &[
    "plans",
    "queue",
    "blocks",
    "agents", // agents/<label>/feedback — the gate signal
    "finished",
    "config.json",
    "pr-reviews", // pr-reviews/<pr>/ — PrReviewer/PrMaster gate state
];

/// Resolve `<git-common-dir>` (the worktree's gitdir for a linked
/// worktree, `<repo>/.git` for the main one), canonicalized so prefix
/// checks match the canonical paths the OS watcher delivers (FSEvents
/// hands back `/private/var/…`).
pub fn git_state_dir(repo: &Path) -> anyhow::Result<PathBuf> {
    let dir = crate::git_io::git_dir(repo)?;
    Ok(dunce::canonicalize(&dir).unwrap_or(dir))
}

/// True iff a path carries a GATE-state signal: a change under the
/// resolved gitdir (HEAD/refs — commits move the fold) OR a write under
/// `.clank/<dir>` where `<dir>` is in [`CLANK_WAKE_DIRS`]. This is the
/// ONE core wake rule both `clank wait` and `clank status` use — never
/// the working tree.
pub fn is_core_wake(path: &Path, git_dir: &Path, clank_root: &Path) -> bool {
    if path.starts_with(git_dir) {
        return true;
    }
    if let Ok(rel) = path.strip_prefix(clank_root) {
        return rel
            .components()
            .next()
            .and_then(|c| c.as_os_str().to_str())
            .is_some_and(|head| CLANK_WAKE_DIRS.contains(&head));
    }
    false
}

/// The shared core watcher. Sends `()` on `tx` for each [`is_core_wake`]
/// event under `.clank/` or the gitdir. Watcher ERRORS also wake
/// (notify signals queue overflow as an error; one cheap refold beats
/// silent staleness). `poll_mode` skips the gitdir watch entirely — the
/// caller's periodic refold is the git-change signal then (empirically
/// required under the Codex tool sandbox, where native gitdir events
/// never reach notify). Holds the `notify` watcher alive; drop to stop.
pub struct RepoStateWatcher {
    _watcher: RecommendedWatcher,
}

impl RepoStateWatcher {
    pub fn attach(repo: &Path, poll_mode: bool, tx: Sender<()>) -> anyhow::Result<Self> {
        // `<repo>/.clank` may not exist yet on a brand-new repo; notify
        // refuses to watch a missing path, so create it first (clank
        // owns the dir anyway).
        let clank_root = repo.join(".clank");
        if let Err(e) = std::fs::create_dir_all(&clank_root) {
            anyhow::bail!("ensure `{}` exists: {e}", clank_root.display());
        }
        let clank_root = dunce::canonicalize(&clank_root).unwrap_or(clank_root);
        let git_dir = git_state_dir(repo)?;

        let cr = clank_root.clone();
        let gd = git_dir.clone();
        let mut watcher =
            notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                match res {
                    Ok(event) => {
                        // Path-less events (rescan notices) wake conservatively.
                        if event.paths.is_empty()
                            || event.paths.iter().any(|p| is_core_wake(p, &gd, &cr))
                        {
                            let _ = tx.send(());
                        }
                    }
                    Err(_) => {
                        let _ = tx.send(());
                    }
                }
            })?;

        watcher
            .watch(&clank_root, RecursiveMode::Recursive)
            .map_err(|e| anyhow::anyhow!("watch `{}` failed: {e}", clank_root.display()))?;
        // Gitdir watch is native-mode only (see `poll_mode` above). It is
        // watched as its own root — `.clank/` is a sibling of `.git/`, so
        // there is no overlap to dedupe.
        if !poll_mode {
            watcher
                .watch(&git_dir, RecursiveMode::Recursive)
                .map_err(|e| anyhow::anyhow!("watch `{}` failed: {e}", git_dir.display()))?;
        }
        Ok(Self { _watcher: watcher })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(rel: &str) -> PathBuf {
        PathBuf::from("/r").join(rel)
    }

    #[test]
    fn core_wakes_on_gate_signal_dirs_and_gitdir() {
        let git_dir = p(".git");
        let clank = p(".clank");
        // .clank gate-signal dirs wake.
        assert!(is_core_wake(&p(".clank/plans/foo.md"), &git_dir, &clank));
        assert!(is_core_wake(
            &p(".clank/agents/codex/feedback/abc.md"),
            &git_dir,
            &clank
        ));
        assert!(is_core_wake(
            &p(".clank/queue/500-foo.md"),
            &git_dir,
            &clank
        ));
        assert!(is_core_wake(&p(".clank/blocks/q.md"), &git_dir, &clank));
        assert!(is_core_wake(&p(".clank/finished/foo.md"), &git_dir, &clank));
        assert!(is_core_wake(&p(".clank/config.json"), &git_dir, &clank));
        // PR-review state (PrReviewer/PrMaster gate) lives here too.
        assert!(is_core_wake(
            &p(".clank/pr-reviews/123/pr.json"),
            &git_dir,
            &clank
        ));
        // gitdir (commits / refs) wakes.
        assert!(is_core_wake(&p(".git/HEAD"), &git_dir, &clank));
        assert!(is_core_wake(&p(".git/refs/heads/main"), &git_dir, &clank));
    }

    #[test]
    fn core_ignores_derived_clank_and_working_tree() {
        let git_dir = p(".git");
        let clank = p(".clank");
        // Derived / foreign .clank subtrees never wake the core loop.
        assert!(!is_core_wake(
            &p(".clank/cache/repo-state/x.v10.bin"),
            &git_dir,
            &clank
        ));
        assert!(!is_core_wake(
            &p(".clank/html/index.html"),
            &git_dir,
            &clank
        ));
        assert!(!is_core_wake(
            &p(".clank/zellij/layout.kdl"),
            &git_dir,
            &clank
        ));
        // Nested worktrees under .clank are whole separate repos.
        assert!(!is_core_wake(
            &p(".clank/worktrees/wt1/.clank/plans/bar.md"),
            &git_dir,
            &clank
        ));
        // The working tree is the diff watcher's concern, never the core.
        assert!(!is_core_wake(&p("src/lib.rs"), &git_dir, &clank));
        assert!(!is_core_wake(&p("target/debug/junk.o"), &git_dir, &clank));
    }
}
