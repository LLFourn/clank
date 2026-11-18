use std::time::Duration;

use super::clients::Clients;
use super::custody::Custody;
use super::hosted::Sources;

/// What the loop waits on for operations that found an event log's
/// sidecar or an ingest lease held (I9): a thread per held lock, blocked
/// in a flock of it as the loop's take wanted it, which lets it go at once
/// and says so. The kernel wakes that flock when the holder lets go, so
/// nothing wakes while the lock stays held, and a release between the
/// loop's take and the thread's flock is a flock that returns at once.
///
/// The threads are detached: a flock cannot be cancelled, and one still
/// blocked when the loop ends sends to nobody and exits on the release.
pub(super) struct Releases {
    /// Where this repo's locks are: another repo's, held in this process
    /// by a test's monitor, are not this loop's to wait on.
    under: std::path::PathBuf,
    pub(super) watching: std::collections::BTreeSet<std::path::PathBuf>,
    tx: tokio::sync::mpsc::UnboundedSender<std::path::PathBuf>,
    pub(super) rx: tokio::sync::mpsc::UnboundedReceiver<std::path::PathBuf>,
    spawn: SpawnWaiter,
    /// Held locks no waiter could be started for, by failed starts.
    unwaited: std::collections::BTreeMap<std::path::PathBuf, u32>,
    /// When to try those again: nothing else would wake the loop for them.
    retry_at: Option<tokio::time::Instant>,
}

/// Starts a waiter off the loop; a seam so a test can refuse.
type SpawnWaiter = fn(Box<dyn FnOnce() + Send>) -> std::io::Result<()>;

pub(super) fn spawn_waiter(waiter: Box<dyn FnOnce() + Send>) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("lock-release".into())
        .spawn(waiter)
        .map(drop)
}

/// How many times a waiter for one held lock may fail to start, a pause
/// apart, before the loop stands down rather than park on a lock nobody
/// waits for.
pub(super) const WAITER_STARTS: u32 = 5;
const WAITER_RETRY: Duration = Duration::from_secs(1);

impl Releases {
    pub(super) fn of(repo: &std::path::Path) -> Self {
        Self::spawning(repo, spawn_waiter)
    }

    pub(super) fn spawning(repo: &std::path::Path, spawn: SpawnWaiter) -> Self {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            under: repo.join(".clank"),
            watching: Default::default(),
            tx,
            rx,
            spawn,
            unwaited: Default::default(),
            retry_at: None,
        }
    }

    /// Wait for the release of every lock found held and not yet waited
    /// for. Called before the loop sleeps, after every take of its pass.
    /// An error is a lock that no waiter could be started for within
    /// [`WAITER_STARTS`], which ends the loop.
    pub(super) fn arm(&mut self) -> anyhow::Result<()> {
        self.retry_at = None;
        for (path, exclusive) in crate::cli::github::log::found_held() {
            if !path.starts_with(&self.under) || self.watching.contains(&path) {
                continue;
            }
            let (tx, waited) = (self.tx.clone(), path.clone());
            let started = (self.spawn)(Box::new(move || {
                if let Err(e) = crate::cli::github::log::when_released(&waited, exclusive) {
                    eprintln!("monitor: waiting on {} failed: {e}", waited.display());
                }
                let _ = tx.send(waited);
            }));
            match started {
                Ok(()) => {
                    self.unwaited.remove(&path);
                    self.watching.insert(path);
                }
                Err(e) => {
                    let failed = self.unwaited.entry(path.clone()).or_default();
                    *failed += 1;
                    anyhow::ensure!(
                        *failed < WAITER_STARTS,
                        "cannot wait for {}'s release ({failed} tries): {e}",
                        path.display()
                    );
                    eprintln!(
                        "monitor: cannot wait for {}'s release yet: {e}",
                        path.display()
                    );
                    self.retry_at = Some(tokio::time::Instant::now() + WAITER_RETRY);
                }
            }
        }
        Ok(())
    }

    /// Whether a released lock is waited for.
    pub(super) fn waiting(&self) -> bool {
        !self.watching.is_empty()
    }

    /// When a waiter that failed to start is tried again.
    pub(super) fn retry_at(&self) -> Option<tokio::time::Instant> {
        self.retry_at
    }

    /// The next released lock, and every other already released.
    pub(super) async fn released(&mut self) -> Option<Vec<std::path::PathBuf>> {
        let mut released = vec![self.rx.recv().await?];
        while let Ok(more) = self.rx.try_recv() {
            released.push(more);
        }
        for path in &released {
            self.watching.remove(path);
        }
        Some(released)
    }
}

/// Whether some loop operation met a log another process held and has
/// yet to run again unheld: a hosted timer's step, a client's last serve,
/// or recovery's last fold.
pub(super) fn unavailable_somewhere(
    sources: &Sources,
    clients: &Clients,
    custody: &Custody,
) -> bool {
    sources.unavailable() || clients.unavailable() || custody.awaits_a_complete_fold()
}
