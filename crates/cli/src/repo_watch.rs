//! The shared repo-STATE watcher: the single wake producer for the
//! gate projection, used by both `clank wait` and `clank status`.
//!
//! It watches exactly the inputs the gate is derived from — the
//! resolved gitdir (HEAD / refs / commits) and the workflow dirs under
//! `.clank/` ([`CLANK_WAKE_DIRS`]) — and NOTHING else. It never watches
//! the working tree, so it is storm-safe by construction (no
//! `build/`/`target/` churn can reach it).
//!
//! `clank tui` and `status --watch` additionally run a SEPARATE Git
//! watcher (in `cli::status`) over the working tree and git dirs to keep
//! their cached Git status fresh; that one is NOT a source of gate state. This module is the part the
//! two commands share so "who is the waiting-upon agent" is derived
//! from one watcher.

use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

use notify::{RecursiveMode, Watcher};

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
    "unblocks",
    "block-migration.json",
    "agents", // agents/<label>/feedback — the gate signal
    "finished",
    "config.json",
    "watch-events",
];

/// Under `.clank/`, what the status panel shows but no gate reads: a
/// change refreshes the panel and wakes no `clank wait` or monitor.
pub const CLANK_STATUS_ONLY_DIRS: &[&str] = &["drafts"];

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
    for dir in [git_dir, common_dir] {
        if let Ok(rel) = path.strip_prefix(dir) {
            return !rel.starts_with("objects") && !rel.starts_with("logs");
        }
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
/// The allowlist lives HERE rather than only in the filter: a recursive
/// `.clank` root would deliver every write to `cache/`, `html/`, and the
/// whole separate repos under `.clank/worktrees/<name>/` with their own
/// `target/` to the callback, which is what this module's allowlist
/// exists to prevent (status-tui-watch-cpu).
pub fn watch_roots(clank_root: &Path, git_dir: &Path, common_dir: &Path) -> Vec<WatchRoot> {
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
    // A git dir's own entries and its `refs/` tree, and nothing under
    // `objects/` or `logs/`, which carry no gate signal and would cost a
    // registration per directory. Git replaces HEAD, the index,
    // `packed-refs` and every loose ref by renaming a lock file over it,
    // so each change is an entry change in one of these directories. The
    // shared dir holds the branch refs a commit moves and `packed-refs`.
    for dir in [common_dir, git_dir] {
        if roots.iter().any(|root| root.path == dir) {
            continue;
        }
        roots.push(WatchRoot {
            path: dir.to_path_buf(),
            mode: RecursiveMode::NonRecursive,
        });
        roots.push(WatchRoot {
            path: dir.join("refs"),
            mode: RecursiveMode::Recursive,
        });
    }
    roots
}

/// Whether a write to `path`, beyond its directory's entries, can carry
/// a gate signal: a file under a `.clank` wake dir that is written in
/// place, such as a plan being edited or an event log appended to.
/// Feedback and finished plans arrive whole, by rename, and a lock's
/// content means nothing; leaving them out keeps the registrations to
/// the files that change. Git's files are all replaced by rename.
pub fn watches_content(path: &Path, clank_root: &Path) -> bool {
    let Ok(rel) = path.strip_prefix(clank_root) else {
        return false;
    };
    let Some(head) = rel.components().next().map(|c| c.as_os_str()) else {
        return false;
    };
    head.to_str()
        .is_some_and(|head| CLANK_WAKE_DIRS.contains(&head) && head != "finished")
        && !rel.components().any(|c| c.as_os_str() == "feedback")
        && path.extension().is_none_or(|ext| ext != "lock")
}

/// The shared core watcher. Sends `()` on `tx` for each [`is_core_wake`]
/// event under `.clank/` or the gitdir. Watcher ERRORS also wake (notify
/// signals queue overflow as an error; one cheap refold beats silent
/// staleness). Kernel events only, so nothing runs until something
/// changes; a watcher that cannot attach is an error, never a poll.
/// Holds the watcher alive; drop to stop.
pub struct RepoStateWatcher {
    _watcher: crate::proc_identity::tree_watch::KernelWatcher,
    fault: crate::fs_notice::Fault,
}

impl RepoStateWatcher {
    pub fn attach(repo: &Path, tx: Sender<()>) -> anyhow::Result<Self> {
        Self::attach_with_timer_invalidation(repo, tx, None)
    }

    /// Reuse this watcher for disposable timer-inventory invalidation.
    pub(crate) fn attach_with_timer_invalidation(
        repo: &Path,
        tx: Sender<()>,
        timer_dirty: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    ) -> anyhow::Result<Self> {
        Self::attach_with_invalidations(repo, tx, timer_dirty, None)
    }

    pub(crate) fn attach_with_invalidations(
        repo: &Path,
        tx: Sender<()>,
        timer_dirty: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
        event_dirty: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    ) -> anyhow::Result<Self> {
        Self::attach_reporting(
            repo,
            tx.into(),
            timer_dirty,
            event_dirty,
            Default::default(),
        )
    }

    /// [`Self::attach`], recording a failure in `fault`, for an owner
    /// that reads it without holding the watcher.
    pub(crate) fn attach_reporting_to<M: Clone + Send + Sync + 'static>(
        repo: &Path,
        tx: impl Into<crate::fs_notice::Wake<M>>,
        fault: crate::fs_notice::Fault,
    ) -> anyhow::Result<Self> {
        Self::attach_reporting(repo, tx.into(), None, None, fault)
    }

    fn attach_reporting<M: Clone + Send + Sync + 'static>(
        repo: &Path,
        tx: crate::fs_notice::Wake<M>,
        timer_dirty: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
        event_dirty: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
        fault: crate::fs_notice::Fault,
    ) -> anyhow::Result<Self> {
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
        let roots = watch_roots(&clank_root, &git_dir, &common_dir);
        for root in roots
            .iter()
            .filter(|root| root.path.starts_with(&clank_root))
        {
            // A dir we cannot create is a wake signal we cannot watch.
            // Failing here beats returning a watcher that silently
            // covers less than it claims.
            std::fs::create_dir_all(&root.path)
                .map_err(|e| anyhow::anyhow!("ensure `{}` exists: {e}", root.path.display()))?;
        }
        let content_root = clank_root.clone();
        let mut watcher = crate::proc_identity::tree_watch::KernelWatcher::new(
            move |path| watches_content(path, &content_root),
            handler(
                tx,
                clank_root.clone(),
                git_dir.clone(),
                common_dir.clone(),
                timer_dirty,
                event_dirty,
                fault.clone(),
            ),
        )
        .map_err(|e| anyhow::anyhow!("cannot watch {}: {e}", repo.display()))?;
        // A git dir's `refs/` that does not exist yet is watched for.
        for root in &roots {
            watcher
                .watch(&root.path, root.mode)
                .map_err(|e| anyhow::anyhow!("watch `{}` failed: {e}", root.path.display()))?;
        }
        Ok(Self {
            _watcher: watcher,
            fault,
        })
    }

    /// Why the watcher failed, once it has. It closes its channel then,
    /// so an owner that sees the channel close reads the reason here.
    pub fn fault(&self) -> Option<String> {
        crate::fs_notice::read(&self.fault)
    }
}

/// The watcher's callback, apart from the backend so a test can drive it.
fn handler<M: Clone + Send + Sync + 'static>(
    tx: crate::fs_notice::Wake<M>,
    cr: PathBuf,
    gd: PathBuf,
    cd: PathBuf,
    timer_dirty: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    event_dirty: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    fault: crate::fs_notice::Fault,
) -> impl FnMut(notify::Result<notify::Event>) + Send + 'static {
    use crate::fs_notice::{Notice, notice};
    let mut tx = Some(tx);
    let dirty = move |flag: &Option<std::sync::Arc<std::sync::atomic::AtomicBool>>| {
        if let Some(flag) = flag {
            flag.store(true, std::sync::atomic::Ordering::Release);
        }
    };
    move |res: notify::Result<notify::Event>| {
        let Some(wake) = &tx else {
            return;
        };
        match notice(&res) {
            Notice::Read => {}
            Notice::Rescan => {
                dirty(&event_dirty);
                dirty(&timer_dirty);
                wake.ring();
            }
            Notice::Changed(paths) => {
                if paths.iter().any(|path| is_event_invalidation(path, &cr)) {
                    dirty(&event_dirty);
                }
                if paths.iter().any(|path| is_timer_inventory(path, &cr)) {
                    dirty(&timer_dirty);
                }
                if paths.iter().any(|p| is_core_wake(p, &gd, &cd, &cr)) {
                    wake.ring();
                }
            }
            // The owner is woken once more and the channel is closed:
            // an owner sharing it reads the fault on that wake, and one
            // alone on it sees it close. Either way the fault says why.
            Notice::Failed(why) => {
                crate::fs_notice::record(&fault, why);
                wake.ring();
                tx = None;
            }
        }
    }
}

/// A timer's log under an agent's directory, or an ancestor of one.
fn is_timer_inventory(path: &Path, clank: &Path) -> bool {
    (path.starts_with(clank.join("agents")) || path.starts_with(clank.join("watch-events")))
        && (path.extension().is_none()
            || (path.extension().is_some_and(|ext| ext == "jsonl")
                && path
                    .file_stem()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("timer-"))))
}

/// Event custody or inventory changed; ordinary git/config/feedback changes do
/// not initiate interruption. Ancestor-directory changes conservatively rescan.
fn is_event_invalidation(path: &Path, clank: &Path) -> bool {
    if path.starts_with(clank.join("watch-events")) {
        return path.extension().is_none() || path.extension().is_some_and(|ext| ext == "jsonl");
    }
    let Ok(relative) = path.strip_prefix(clank.join("agents")) else {
        return false;
    };
    let components: Vec<_> = relative.components().collect();
    components.len() < 2
        || (components[1].as_os_str() == "events"
            && (path.extension().is_none()
                || path
                    .extension()
                    .is_some_and(|extension| extension == "jsonl")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(rel: &str) -> PathBuf {
        PathBuf::from("/r").join(rel)
    }

    #[test]
    fn event_invalidations_exclude_ordinary_repo_state_and_sidecars() {
        let root = p(".clank");
        for path in [
            ".clank/agents/grok/events/messages.jsonl",
            ".clank/agents/grok/events/timer-config-a.jsonl",
            ".clank/agents/grok/events",
            ".clank/agents/grok",
        ] {
            assert!(is_event_invalidation(&p(path), &root), "{path}");
        }
        for path in [
            ".git/HEAD",
            ".clank/config.json",
            ".clank/agents/grok/config.json",
            ".clank/agents/grok/feedback/a.md",
            ".clank/agents/grok/events/messages.lock",
            ".clank/agents/grok/events/messages.state.json",
        ] {
            assert!(!is_event_invalidation(&p(path), &root), "{path}");
        }
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
        // gitdir (commits / refs) wakes.
        assert!(is_core_wake(&p(".git/HEAD"), &git_dir, &git_dir, &clank));
        assert!(is_core_wake(
            &p(".git/refs/heads/main"),
            &git_dir,
            &git_dir,
            &clank
        ));
    }

    fn git(repo: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    /// The watcher is native: attaching it costs no scan, an idle repo
    /// wakes it never, and a commit and a gate write each wake it.
    #[test]
    fn the_watcher_wakes_on_changes_and_never_on_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        git(repo, &["init", "-q", "-b", "main"]);
        git(repo, &["commit", "-q", "--allow-empty", "-m", "one"]);
        let (tx, rx) = std::sync::mpsc::channel();
        let _watcher = RepoStateWatcher::attach(repo, tx).unwrap();
        while rx
            .recv_timeout(std::time::Duration::from_millis(500))
            .is_ok()
        {}
        assert!(
            rx.recv_timeout(std::time::Duration::from_secs(2)).is_err(),
            "an idle repo woke the watcher"
        );
        git(repo, &["commit", "-q", "--allow-empty", "-m", "two"]);
        assert!(
            rx.recv_timeout(std::time::Duration::from_secs(5)).is_ok(),
            "a commit did not wake it"
        );
        while rx
            .recv_timeout(std::time::Duration::from_millis(300))
            .is_ok()
        {}
        std::fs::write(repo.join(".clank/plans/new.md"), "# new\n").unwrap();
        assert!(
            rx.recv_timeout(std::time::Duration::from_secs(5)).is_ok(),
            "a plan did not wake it"
        );
    }

    fn settle(rx: &std::sync::mpsc::Receiver<()>) {
        while rx
            .recv_timeout(std::time::Duration::from_millis(300))
            .is_ok()
        {}
    }

    fn woken(rx: &std::sync::mpsc::Receiver<()>) -> bool {
        rx.recv_timeout(std::time::Duration::from_secs(5)).is_ok()
    }

    /// Git replaces HEAD, the index and refs by renaming over them, and
    /// makes ref directories as it needs them; an event log is appended
    /// to. Each wakes the watcher; nothing under `objects/` does.
    #[test]
    fn renames_over_git_state_new_ref_dirs_and_log_appends_wake_it() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        git(repo, &["init", "-q", "-b", "main"]);
        git(repo, &["commit", "-q", "--allow-empty", "-m", "one"]);
        let events = repo.join(".clank/agents/a/events");
        std::fs::create_dir_all(&events).unwrap();
        let log = events.join("messages.jsonl");
        std::fs::write(&log, "{}\n").unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let _watcher = RepoStateWatcher::attach(repo, tx).unwrap();
        settle(&rx);
        let gitdir = repo.join(".git");
        let replace = |path: &Path| {
            let body = std::fs::read(path).unwrap();
            let lock = path.with_extension("lock");
            std::fs::write(&lock, body).unwrap();
            std::fs::rename(&lock, path).unwrap();
        };

        std::fs::write(gitdir.join("objects/stray"), "").unwrap();
        assert!(
            rx.recv_timeout(std::time::Duration::from_secs(1)).is_err(),
            "the object store woke it"
        );
        for file in ["HEAD", "index"] {
            replace(&gitdir.join(file));
            assert!(woken(&rx), "a rename over {file} did not wake it");
            settle(&rx);
        }
        git(repo, &["branch", "topic/deeper/branch"]);
        assert!(woken(&rx), "a ref in a new refs directory did not wake it");
        settle(&rx);
        replace(&gitdir.join("refs/heads/topic/deeper/branch"));
        assert!(woken(&rx), "the new refs directory is not watched");
        settle(&rx);
        {
            use std::io::Write as _;
            let mut file = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
            file.write_all(b"{}\n").unwrap();
        }
        assert!(woken(&rx), "an append to an event log did not wake it");
    }

    #[test]
    fn only_files_written_in_place_have_their_content_watched() {
        let clank = p(".clank");
        for path in [
            ".clank/config.json",
            ".clank/plans/a.md",
            ".clank/queue/500-a.md",
            ".clank/agents/a/events/messages.jsonl",
            ".clank/agents/a/state.json",
        ] {
            assert!(watches_content(&p(path), &clank), "{path}");
        }
        for path in [
            ".clank/agents/a/feedback/abc.md",
            ".clank/finished/a.md",
            ".clank/agents/a/binding.lock",
            ".clank/cache/x.bin",
            ".git/HEAD",
        ] {
            assert!(!watches_content(&p(path), &clank), "{path}");
        }
    }

    /// Reading the files it watches is not a change: the owner's own
    /// fold opens HEAD and the index, and on Linux each open is a
    /// notification. A rescan still wakes, and a failure is recorded,
    /// woken for once, and closes the channel.
    #[test]
    fn reads_wake_nothing_and_a_failure_ends_the_stream() {
        use crate::fs_notice::tests::{event, opened};
        let (git_dir, clank) = (p(".git"), p(".clank"));
        let (tx, rx) = std::sync::mpsc::channel();
        let timer = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let events = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let fault = crate::fs_notice::Fault::default();
        let mut handle = handler(
            tx.into(),
            clank.clone(),
            git_dir.clone(),
            git_dir.clone(),
            Some(timer.clone()),
            Some(events.clone()),
            fault.clone(),
        );
        let dirty = || {
            timer.load(std::sync::atomic::Ordering::Acquire)
                || events.load(std::sync::atomic::Ordering::Acquire)
        };
        for read in ["HEAD", "index", "refs/heads/main"] {
            handle(opened(&git_dir.join(read)));
        }
        handle(opened(&clank.join("agents/codex/events/messages.jsonl")));
        assert!(rx.try_recv().is_err(), "a read woke the owner");
        assert!(!dirty(), "a read invalidated something");

        handle(event(
            notify::EventKind::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Content,
            )),
            &git_dir.join("HEAD"),
        ));
        assert!(rx.try_recv().is_ok(), "a write wakes it");

        handle(Ok(notify::Event::new(notify::EventKind::Any)
            .add_path(git_dir.clone())
            .set_flag(notify::event::Flag::Rescan)));
        assert!(rx.try_recv().is_ok() && dirty(), "a rescan is conservative");

        handle(Err(notify::Error::generic("the stream died")));
        assert!(rx.try_recv().is_ok(), "woken once to read the fault");
        assert_eq!(
            rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Disconnected),
            "and the channel is closed"
        );
        assert_eq!(
            crate::fs_notice::read(&fault).as_deref(),
            Some("the stream died")
        );
        handle(event(notify::EventKind::Any, &git_dir.join("HEAD")));
        assert_eq!(
            crate::fs_notice::read(&fault).as_deref(),
            Some("the stream died")
        );
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
        let roots = watch_roots(&clank, &git_dir, &common);
        for (path, why) in [
            (
                common.join("packed-refs"),
                "the shared dir carries packed-refs",
            ),
            (
                common.join("refs/heads/feature"),
                "the branch ref a commit MOVES lives here",
            ),
            (common.join("refs/heads/new/later"), "and refs made later"),
            (git_dir.join("HEAD"), "per-worktree HEAD still counts"),
        ] {
            assert!(covered(&roots, &path), "{why}: {path:?}");
        }
        let apart = p("elsewhere/feature");
        assert!(
            covered(&watch_roots(&clank, &apart, &common), &apart.join("HEAD")),
            "a gitdir outside the shared one is its own root"
        );
    }

    /// Whether `roots` deliver events for `path`.
    fn covered(roots: &[WatchRoot], path: &Path) -> bool {
        roots.iter().any(|root| match root.mode {
            RecursiveMode::Recursive => path.starts_with(&root.path),
            RecursiveMode::NonRecursive => path.parent() == Some(root.path.as_path()),
        })
    }

    /// The main worktree must not register the same root twice.
    #[test]
    fn the_main_worktree_registers_its_gitdir_once() {
        let git_dir = p(".git");
        let clank = p(".clank");
        let got = paths(&watch_roots(&clank, &git_dir, &git_dir));
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
        let got = paths(&watch_roots(&clank, &git_dir, &git_dir));
        for excluded in ["cache", "html", "zellij", "drafts", "worktrees"] {
            assert!(
                !got.contains(&clank.join(excluded)),
                "`{excluded}` must never be a poll root: {got:?}"
            );
        }
        // And `.clank` itself is NON-recursive, or the exclusions above
        // would be reached by descent anyway.
        let root = watch_roots(&clank, &git_dir, &git_dir)
            .into_iter()
            .find(|r| r.path == clank)
            .expect("the .clank root");
        assert!(matches!(root.mode, RecursiveMode::NonRecursive));
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
        // The working tree is the Git watcher's concern, never the core.
        assert!(!is_core_wake(&p("src/lib.rs"), &git_dir, &git_dir, &clank));
        assert!(!is_core_wake(
            &p("target/debug/junk.o"),
            &git_dir,
            &git_dir,
            &clank
        ));
    }
}
