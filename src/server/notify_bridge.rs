//! Notify-debouncer-full wiring. Watches `<repo>/.trinity/` recursively
//! and the resolved gitdir's `HEAD` + `logs/HEAD`. Translates events into
//! `FilesystemSignal` via `fs_watcher::path_to_signal` and forwards them
//! to the runtime's `handle_signal`.
//!
//! Per the plan's implementation notes, linked-worktree gitdirs (where
//! `<worktree>/.git` is a file containing `gitdir: <path>`) are resolved
//! before being watched.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use notify::RecursiveMode;
use notify_debouncer_full::{DebounceEventResult, new_debouncer};
use tokio::sync::mpsc;

use crate::fs_watcher::{FsEventKind, path_to_signal};
use crate::runtime::Runtime;

/// Start watching a repo. Returns a join handle for the background task
/// that forwards events to `runtime.handle_signal`. Dropping the handle
/// (or aborting) stops the watch.
pub async fn start(
    runtime: Arc<Runtime>,
    repo_root: PathBuf,
) -> anyhow::Result<tokio::task::JoinHandle<()>> {
    let trinity_dir = repo_root.join(".trinity");
    std::fs::create_dir_all(&trinity_dir)?;
    let git_dir = resolve_gitdir(&repo_root)?;

    let (tx, mut rx) = mpsc::unbounded_channel::<DebounceEventResult>();
    let mut debouncer = new_debouncer(
        Duration::from_millis(150),
        None,
        move |result: DebounceEventResult| {
            let _ = tx.send(result);
        },
    )?;

    debouncer
        .watch(&trinity_dir, RecursiveMode::Recursive)?;
    // Watch the gitdir's HEAD-family files. If the gitdir is shared with
    // the worktree (regular repo), one watcher per file is enough; for
    // linked worktrees, `git_dir` is the resolved per-worktree gitdir.
    debouncer
        .watch(&git_dir, RecursiveMode::NonRecursive)?;

    let repo_root_for_task = repo_root.clone();
    let runtime_for_task = Arc::clone(&runtime);
    let handle = tokio::spawn(async move {
        // Keep the debouncer alive for the lifetime of the task.
        let _debouncer = debouncer;
        while let Some(result) = rx.recv().await {
            let events = match result {
                Ok(events) => events,
                Err(errs) => {
                    for e in errs {
                        tracing::warn!(error = ?e, "notify error");
                    }
                    continue;
                }
            };
            for event in events {
                for path in &event.event.paths {
                    let kind = if event.event.kind.is_remove() {
                        FsEventKind::Removed
                    } else {
                        FsEventKind::CreatedOrModified
                    };
                    // The path could be under `<repo>/.trinity/` or under the gitdir.
                    // Try both root strippings.
                    let signal = path_to_signal(path, &repo_root_for_task, kind)
                        .or_else(|| path_to_signal(path, &git_dir, kind));
                    let Some(signal) = signal else {
                        continue;
                    };
                    // Dedupe consecutive identical HeadChanged signals?
                    // For now, just forward every signal — the runtime is
                    // idempotent on these.
                    let now = unix_now();
                    if let Err(err) = runtime_for_task
                        .handle_signal(&repo_root_for_task, signal, now)
                        .await
                    {
                        tracing::warn!(error = ?err, "handle_signal failed");
                    }
                }
            }
        }
    });

    Ok(handle)
}

/// Resolve the gitdir for a repo root. For regular repos this is just
/// `<repo>/.git`; for linked worktrees `<worktree>/.git` is a *file*
/// containing `gitdir: <path>`, and we follow it.
fn resolve_gitdir(repo_root: &Path) -> anyhow::Result<PathBuf> {
    let dotgit = repo_root.join(".git");
    let meta = std::fs::metadata(&dotgit)
        .map_err(|e| anyhow::anyhow!("stat {}: {e}", dotgit.display()))?;
    if meta.is_dir() {
        return Ok(dotgit);
    }
    // It's a file. Parse the `gitdir: <path>` line.
    let body = std::fs::read_to_string(&dotgit)?;
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("gitdir: ") {
            let p = PathBuf::from(rest.trim());
            if p.is_absolute() {
                return Ok(p);
            }
            return Ok(repo_root.join(p));
        }
    }
    Err(anyhow::anyhow!(
        ".git file present but no `gitdir:` line: {}",
        dotgit.display()
    ))
}

fn unix_now() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn run_git(cwd: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn resolve_gitdir_regular_repo() {
        let dir = tempfile::tempdir().unwrap();
        run_git(dir.path(), &["init", "--quiet", "--initial-branch=main"]);
        let gitdir = resolve_gitdir(dir.path()).unwrap();
        assert!(gitdir.ends_with(".git"));
        assert!(gitdir.is_dir());
    }

    #[test]
    fn resolve_gitdir_linked_worktree() {
        let main = tempfile::tempdir().unwrap();
        run_git(main.path(), &["init", "--quiet", "--initial-branch=main"]);
        run_git(main.path(), &["config", "user.email", "t@t"]);
        run_git(main.path(), &["config", "user.name", "t"]);
        run_git(main.path(), &["config", "commit.gpgsign", "false"]);
        run_git(main.path(), &["commit", "--allow-empty", "-m", "init"]);
        let wt_dir = tempfile::tempdir().unwrap();
        let wt_path = wt_dir.path().join("wt1");
        run_git(
            main.path(),
            &[
                "worktree",
                "add",
                wt_path.to_str().unwrap(),
                "-b",
                "feat",
            ],
        );
        let gitdir = resolve_gitdir(&wt_path).unwrap();
        // Should resolve to <main>/.git/worktrees/wt1 (or similar)
        assert!(gitdir.is_dir(), "resolved gitdir should be a directory: {:?}", gitdir);
        // The gitdir's HEAD file should exist.
        assert!(gitdir.join("HEAD").exists());
    }
}
