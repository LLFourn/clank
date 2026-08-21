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

use notify::{PollWatcher, RecursiveMode, Watcher};

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
pub fn git_common_state_dir(repo: &Path) -> anyhow::Result<PathBuf> {
    let dir = crate::git_io::common_dir(repo)?;
    Ok(dunce::canonicalize(&dir).unwrap_or(dir))
}

pub fn git_state_dir(repo: &Path) -> anyhow::Result<PathBuf> {
    let dir = crate::git_io::git_dir(repo)?;
    Ok(dunce::canonicalize(&dir).unwrap_or(dir))
}

/// True iff a path carries a GATE-state signal: a change under either
/// git directory (HEAD/refs — commits move the fold) OR a write under
/// `.clank/<dir>` where `<dir>` is in [`CLANK_WAKE_DIRS`]. This is the
/// ONE core wake rule both `clank wait` and `clank status` use — never
/// the working tree.
///
/// BOTH git dirs, because in a linked worktree they differ and the
/// signals are split across them: `HEAD` is per-worktree, while the
/// branch refs and `packed-refs` that a commit actually moves live in
/// the SHARED common dir. Watching only the per-worktree gitdir misses
/// every commit made in a linked worktree.
pub fn is_core_wake(path: &Path, git_dir: &Path, common_dir: &Path, clank_root: &Path) -> bool {
    if path.starts_with(git_dir) || path.starts_with(common_dir) {
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

/// One registered watch root: WHAT to watch and how deeply.
pub struct WatchRoot {
    pub path: PathBuf,
    pub mode: RecursiveMode,
}

/// The roots to register — the SELECTION, separated from the
/// registering so it can be asserted directly.
///
/// That separation is not cosmetic. The linked-worktree bug lived
/// here, in which roots got registered, and a test that only exercised
/// [`is_core_wake`] passed straight through it: the filter accepted
/// shared-dir paths that no root was delivering.
///
/// The allowlist lives HERE rather than only in the filter. Under a
/// native backend the kernel does the filtering and a recursive root
/// costs nothing. A POLLER restats every descendant of whatever it is
/// given — so a recursive `.clank` root would restat `cache/`,
/// `html/`, and the whole separate repos under
/// `.clank/worktrees/<name>/` with their own `target/`, every
/// interval, before the filter ever runs. That is exactly what this
/// module's allowlist exists to prevent (status-tui-watch-cpu).
pub fn watch_roots(
    clank_root: &Path,
    git_dir: &Path,
    common_dir: &Path,
    poll_mode: bool,
) -> Vec<WatchRoot> {
    let mut roots = vec![
        // Non-recursive so `config.json` — a file, not a dir — is
        // covered without descending into the derived dirs.
        WatchRoot {
            path: clank_root.to_path_buf(),
            mode: RecursiveMode::NonRecursive,
        },
    ];
    roots.extend(
        CLANK_WAKE_DIRS
            .iter()
            // `config.json` is covered by the root above.
            .filter(|d| !d.contains('.'))
            .map(|d| WatchRoot {
                path: clank_root.join(d),
                mode: RecursiveMode::Recursive,
            }),
    );
    // Gitdir watching is native-mode only: under `poll_mode` the
    // caller's periodic refold is the git-change signal instead.
    if !poll_mode {
        // The gate signals are HEAD and the refs. `objects/` is the
        // bulk of a gitdir and cannot carry one, so polling it would
        // be the most expensive thing here and buy nothing.
        roots.push(WatchRoot {
            path: git_dir.to_path_buf(),
            mode: RecursiveMode::NonRecursive,
        });
        roots.push(WatchRoot {
            path: git_dir.join("refs"),
            mode: RecursiveMode::Recursive,
        });
        // A LINKED worktree splits the signals: its own gitdir carries
        // `HEAD`, while the branch ref a commit MOVES and `packed-refs`
        // live in the shared dir. Without these, a commit made in a
        // linked worktree wakes nothing at all.
        if common_dir != git_dir {
            roots.push(WatchRoot {
                path: common_dir.to_path_buf(),
                mode: RecursiveMode::NonRecursive,
            });
            roots.push(WatchRoot {
                path: common_dir.join("refs"),
                mode: RecursiveMode::Recursive,
            });
        }
    }
    roots
}

/// The shared core watcher. Sends `()` on `tx` for each [`is_core_wake`]
/// event under `.clank/` or the gitdir. Watcher ERRORS also wake
/// (notify signals queue overflow as an error; one cheap refold beats
/// silent staleness). `poll_mode` skips the gitdir watch entirely — the
/// caller's periodic refold is the git-change signal then (empirically
/// required under the Codex tool sandbox, where native gitdir events
/// never reach notify). Holds the `notify` watcher alive; drop to stop.
pub struct RepoStateWatcher {
    _watcher: PollWatcher,
}

/// How often the watcher restats its roots.
///
/// Matches the shortest refold cadence a caller uses, so the watcher
/// is never the slower of the two signals.
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

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
        // The SHARED gitdir. Equal to `git_dir` in the main worktree;
        // in a linked one it holds the branch refs a commit moves.
        let common_dir = git_common_state_dir(repo)?;

        let cr = clank_root.clone();
        let gd = git_dir.clone();
        let cd = common_dir.clone();
        // POLLING, not the platform-native backend. `notify`'s FSEvents
        // backend costs ~10s PER `.watch()` call on macOS — measured at
        // 9.4s for one root and 18.9s for two, against 65ms to poll a
        // real 2112-entry gitdir. That is not a test-harness problem:
        // it sat in front of every `clank wait`, so an armed wait spent
        // ~20s not yet watching anything.
        //
        // The roots are small and bounded by construction — an
        // allowlist under `.clank/` plus the gitdir — which is what
        // makes restatting them affordable where polling a working
        // tree would not be.
        let mut watcher = PollWatcher::new(
            move |res: notify::Result<notify::Event>| {
                match res {
                    Ok(event) => {
                        // Path-less events (rescan notices) wake conservatively.
                        if event.paths.is_empty()
                            || event.paths.iter().any(|p| is_core_wake(p, &gd, &cd, &cr))
                        {
                            let _ = tx.send(());
                        }
                    }
                    Err(_) => {
                        let _ = tx.send(());
                    }
                }
            },
            notify::Config::default().with_poll_interval(POLL_INTERVAL),
        )?;

        for root in watch_roots(&clank_root, &git_dir, &common_dir, poll_mode) {
            if root.path.starts_with(&clank_root) && root.path != clank_root {
                // A dir we cannot create is a wake signal we cannot
                // watch. Failing here beats returning a watcher that
                // silently covers less than it claims.
                std::fs::create_dir_all(&root.path)
                    .map_err(|e| anyhow::anyhow!("ensure `{}` exists: {e}", root.path.display()))?;
            } else if !root.path.exists() {
                // A git root that does not exist yet (`refs/` on a
                // fresh repo) is not ours to create; git makes it, and
                // its creation shows up under the parent we already
                // watch non-recursively.
                continue;
            }
            watcher
                .watch(&root.path, root.mode)
                .map_err(|e| anyhow::anyhow!("watch `{}` failed: {e}", root.path.display()))?;
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
        assert!(is_core_wake(
            &p(".clank/plans/foo.md"),
            &git_dir,
            &git_dir,
            &clank
        ));
        assert!(is_core_wake(
            &p(".clank/agents/codex/feedback/abc.md"),
            &git_dir,
            &git_dir,
            &clank
        ));
        assert!(is_core_wake(
            &p(".clank/queue/500-foo.md"),
            &git_dir,
            &git_dir,
            &clank
        ));
        assert!(is_core_wake(
            &p(".clank/blocks/q.md"),
            &git_dir,
            &git_dir,
            &clank
        ));
        assert!(is_core_wake(
            &p(".clank/finished/foo.md"),
            &git_dir,
            &git_dir,
            &clank
        ));
        assert!(is_core_wake(
            &p(".clank/config.json"),
            &git_dir,
            &git_dir,
            &clank
        ));
        // PR-review state (PrReviewer/PrMaster gate) lives here too.
        assert!(is_core_wake(
            &p(".clank/pr-reviews/123/pr.json"),
            &git_dir,
            &git_dir,
            &clank
        ));
        // gitdir (commits / refs) wakes.
        assert!(is_core_wake(&p(".git/HEAD"), &git_dir, &git_dir, &clank));
        assert!(is_core_wake(
            &p(".git/refs/heads/main"),
            &git_dir,
            &git_dir,
            &clank
        ));
    }

    fn paths(roots: &[WatchRoot]) -> Vec<PathBuf> {
        roots.iter().map(|r| r.path.clone()).collect()
    }

    /// The REGISTERED ROOTS, not the filter. The linked-worktree bug
    /// was here — the filter already accepted shared-dir paths that
    /// no root was delivering, so a filter-only test passed straight
    /// through it.
    #[test]
    fn a_linked_worktree_registers_the_shared_refs_as_roots() {
        let git_dir = p(".git/worktrees/feature");
        let common = p(".git");
        let clank = p(".clank");
        let got = paths(&watch_roots(&clank, &git_dir, &common, false));

        assert!(
            got.contains(&common),
            "the shared dir carries packed-refs: {got:?}"
        );
        assert!(
            got.contains(&common.join("refs")),
            "the branch ref a commit MOVES lives here: {got:?}"
        );
        assert!(
            got.contains(&git_dir),
            "per-worktree HEAD still counts: {got:?}"
        );
    }

    /// The main worktree must not register the same root twice.
    #[test]
    fn the_main_worktree_registers_its_gitdir_once() {
        let git_dir = p(".git");
        let clank = p(".clank");
        let got = paths(&watch_roots(&clank, &git_dir, &git_dir, false));
        assert_eq!(
            got.iter().filter(|x| *x == &git_dir).count(),
            1,
            "git_dir == common_dir here: {got:?}"
        );
    }

    /// The storm-safety property, asserted on the roots themselves:
    /// the derived dirs and the nested repos under `worktrees/` are
    /// never registered, so a poller never restats them.
    #[test]
    fn derived_and_nested_repo_dirs_are_never_registered() {
        let clank = p(".clank");
        let git_dir = p(".git");
        let got = paths(&watch_roots(&clank, &git_dir, &git_dir, false));
        for excluded in ["cache", "html", "zellij", "drafts", "worktrees"] {
            assert!(
                !got.contains(&clank.join(excluded)),
                "`{excluded}` must never be a poll root: {got:?}"
            );
        }
        // And `.clank` itself is NON-recursive, or the exclusions above
        // would be reached by descent anyway.
        let root = watch_roots(&clank, &git_dir, &git_dir, false)
            .into_iter()
            .find(|r| r.path == clank)
            .expect("the .clank root");
        assert!(matches!(root.mode, RecursiveMode::NonRecursive));
    }

    /// Poll mode skips the gitdir entirely — the caller's periodic
    /// refold is the git-change signal there.
    #[test]
    fn poll_mode_registers_no_git_roots() {
        let clank = p(".clank");
        let git_dir = p(".git");
        let got = paths(&watch_roots(&clank, &git_dir, &git_dir, true));
        assert!(!got.iter().any(|x| x.starts_with(&git_dir)), "{got:?}");
    }

    /// A LINKED worktree splits the signals: `HEAD` sits in the
    /// per-worktree gitdir, but the branch ref a commit MOVES lives in
    /// the shared common dir. Watching only the per-worktree dir means
    /// no commit made in a linked worktree ever wakes anything.
    #[test]
    fn a_linked_worktree_wakes_on_the_shared_branch_ref() {
        let git_dir = p(".git/worktrees/feature");
        let common = p(".git");
        let clank = p(".clank");

        // The commit signal: a branch ref under the SHARED dir.
        assert!(is_core_wake(
            &p(".git/refs/heads/feature"),
            &git_dir,
            &common,
            &clank
        ));
        // And its packed form, which a gc rewrites.
        assert!(is_core_wake(
            &p(".git/packed-refs"),
            &git_dir,
            &common,
            &clank
        ));
        // The per-worktree half still counts.
        assert!(is_core_wake(
            &p(".git/worktrees/feature/HEAD"),
            &git_dir,
            &common,
            &clank
        ));
        // The working tree still does not.
        assert!(!is_core_wake(&p("src/lib.rs"), &git_dir, &common, &clank));
    }

    #[test]
    fn core_ignores_derived_clank_and_working_tree() {
        let git_dir = p(".git");
        let clank = p(".clank");
        // Derived / foreign .clank subtrees never wake the core loop.
        assert!(!is_core_wake(
            &p(".clank/cache/repo-state/x.v10.bin"),
            &git_dir,
            &git_dir,
            &clank
        ));
        assert!(!is_core_wake(
            &p(".clank/html/index.html"),
            &git_dir,
            &git_dir,
            &clank
        ));
        assert!(!is_core_wake(
            &p(".clank/zellij/layout.kdl"),
            &git_dir,
            &git_dir,
            &clank
        ));
        // Nested worktrees under .clank are whole separate repos.
        assert!(!is_core_wake(
            &p(".clank/worktrees/wt1/.clank/plans/bar.md"),
            &git_dir,
            &git_dir,
            &clank
        ));
        // The working tree is the diff watcher's concern, never the core.
        assert!(!is_core_wake(&p("src/lib.rs"), &git_dir, &git_dir, &clank));
        assert!(!is_core_wake(
            &p("target/debug/junk.o"),
            &git_dir,
            &git_dir,
            &clank
        ));
    }
}
