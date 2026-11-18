use std::time::Duration;

#[cfg(test)]
use super::custody::Custody;
#[cfg(test)]
use super::hosted::Sources;
#[cfg(test)]
use super::published::{describe, publish};
use super::watchdog;

/// The pause before a failed fold is tried again, doubling up to the cap
/// while it goes on failing; the watchdog stands the monitor down if it
/// fails for longer than [`watchdog::BUDGET`].
const FOLD_FIRST_RETRY: Duration = Duration::from_millis(500);
const FOLD_RETRY_CAP: Duration = Duration::from_secs(8);

/// The repo's changes, produced independently of client traffic.
///
/// `Beat::changed` is a receive and then a 200ms debounce. Inside the
/// owner's select it could consume a filesystem wake and then be
/// cancelled during the debounce, so the channel item was gone and the
/// owner never saw the change (codex on 24ea491). A dropped repo change
/// is a silent agent, which is the failure this plan exists to prevent.
///
/// A dedicated task owns the `Beat` instead. Nothing cancels it, so its
/// state cannot be discarded, and the owner only ever awaits a channel
/// receive — which holds no state to lose. Capacity 1 coalesces changes
/// for a busy owner rather than queueing them, and the watcher's own
/// channel buffers the events meanwhile. It closes when the watcher
/// does.
pub(super) fn changes(fs_rx: std::sync::mpsc::Receiver<()>) -> tokio::sync::mpsc::Receiver<()> {
    let (tx, rx) = tokio::sync::mpsc::channel::<()>(1);
    tokio::spawn(async move {
        let mut beat = crate::cli::wait::Beat::bridge(fs_rx);
        while beat.changed().await.is_ok() {
            if tx.send(()).await.is_err() {
                break;
            }
        }
    });
    rx
}

/// Fold, and count it ONLY if that succeeded: a failure arms the
/// watchdog until a fold succeeds.
///
/// Counting it first and then failing the rebuild would advertise
/// progress that never happened, and a loop that kept failing would
/// keep looking fresh to the watchdog — the invariant this plan set for
/// itself, inverted (codex on 13e9fa7).
pub(super) async fn advance(
    repo: &std::path::Path,
    fold: u64,
    busy: &watchdog::Busy,
) -> Option<(u64, crate::repo_state::RepoState)> {
    let outcome = folded(repo).await;
    advance_with(fold, busy, outcome)
}

/// The ORDERING, with the fold's outcome supplied.
///
/// Split out because the rebuild is fail-soft: probed here, it returns
/// `Ok` for a repo with no git, for a broken `.git` file and for an
/// empty `.git` directory, so a failing fold cannot be provoked
/// through it and the invariant would go untested. What matters is the
/// order — nothing is published unless a fold completed — and that is
/// decided here.
///
/// The watchdog is told of the outcome here, and ONLY here, so the
/// caller has no way to report a fold that did not complete.
fn advance_with(
    fold: u64,
    busy: &watchdog::Busy,
    outcome: anyhow::Result<crate::repo_state::RepoState>,
) -> Option<(u64, crate::repo_state::RepoState)> {
    let state = match outcome {
        Ok(state) => state,
        Err(e) => {
            eprintln!("monitor: {e}");
            busy.failing(true);
            return None;
        }
    };
    busy.failing(false);
    Some((fold + 1, state))
}

/// One completed fold of the repo.
/// One fold's publication, as a monitor in this process would make it:
/// what status reads of each agent's turns.
#[cfg(test)]
pub(crate) async fn publish_one_fold(repo: &std::path::Path) {
    publish(
        repo,
        &describe(
            repo,
            std::process::id() as i32,
            std::path::Path::new("socket"),
        ),
    )
    .unwrap();
    let state = folded(repo).await.unwrap();
    let mut custody = Custody::of(
        crate::cli::turn_custody::Turns::new(repo.to_path_buf()),
        crate::cli::recovery::Driver::new(repo.to_path_buf(), "/nonexistent".into()),
    );
    let facts = custody.facts(repo, &state, &mut Sources::hosting(Default::default()));
    custody
        .recovery_mut()
        .reconcile(facts, crate::cli::recovery::unix_now());
}

pub(super) async fn folded(repo: &std::path::Path) -> anyhow::Result<crate::repo_state::RepoState> {
    crate::rebuild::rebuild_repo_with_policy(repo, crate::rebuild::CachePolicy::Use)
        .await
        .map_err(|e| anyhow::anyhow!("failed to fold repo `{}`: {e}", repo.display()))
}

/// The repo's fold: THE watcher of the repo, what it says changed, the
/// last fold completed, and the retry of one that failed.
///
/// One watcher, here, instead of one per parked wait and two per `clank
/// tui` — the whole point of the plan: ~170 of them were attached on the
/// machine this was measured on, each refolding the repo on every
/// change.
pub(super) struct Folding {
    /// The last completed fold, so a source signal can present against
    /// the same state a tick would.
    state: Option<crate::repo_state::RepoState>,
    /// Retried after a failure, a little later each time.
    retry: Option<(tokio::time::Instant, Duration)>,
    pub(super) changes: tokio::sync::mpsc::Receiver<()>,
    watcher: std::sync::Arc<std::sync::Mutex<Option<crate::repo_watch::RepoStateWatcher>>>,
    event_dirty: std::sync::Arc<std::sync::atomic::AtomicBool>,
    timer_dirty: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// The monitor's own fold counter. Private on purpose: it was
    /// once published and put on every frame, and no reader ever
    /// compared two of them. What needs it is the watchdog, in this
    /// process, which stands a stalled monitor down, and a client sees
    /// that as its socket closing.
    fold: u64,
}

/// What a change invalidated besides the fold: the events owed, or the
/// timers' inventory.
#[derive(Debug, Clone, Copy)]
pub(super) struct Dirty {
    pub(super) events: bool,
    pub(super) timers: bool,
}

impl Folding {
    /// Watch `repo`, its first fold due at once.
    pub(super) fn start(repo: &std::sync::Arc<std::path::PathBuf>) -> Self {
        let (fs_tx, fs_rx) = std::sync::mpsc::channel::<()>();
        let timer_dirty = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let event_dirty = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        // The first fold now, and the watcher attached off the loop: its
        // native registration can take seconds, and clients attaching
        // meanwhile must be greeted. Attached, it wakes the loop once for
        // the gap it took as its baseline; failing to attach closes the
        // channel, which ends the monitor saying so.
        let _ = fs_tx.send(());
        let watcher: std::sync::Arc<std::sync::Mutex<Option<crate::repo_watch::RepoStateWatcher>>> =
            Default::default();
        {
            let (repo, watcher, timer_dirty, event_dirty) = (
                repo.clone(),
                watcher.clone(),
                timer_dirty.clone(),
                event_dirty.clone(),
            );
            std::thread::spawn(move || {
                match crate::repo_watch::RepoStateWatcher::attach_with_invalidations(
                    &repo,
                    fs_tx.clone(),
                    Some(timer_dirty),
                    Some(event_dirty),
                ) {
                    Ok(attached) => {
                        *watcher
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(attached);
                        let _ = fs_tx.send(());
                    }
                    Err(e) => eprintln!("monitor: cannot watch {}: {e:#}", repo.display()),
                }
            });
        }
        let changes = changes(fs_rx);
        Folding {
            state: None,
            retry: None,
            changes,
            watcher,
            event_dirty,
            timer_dirty,
            fold: 0,
        }
    }

    pub(super) fn state(&self) -> Option<&crate::repo_state::RepoState> {
        self.state.as_ref()
    }

    pub(super) fn retry_at(&self) -> Option<tokio::time::Instant> {
        self.retry.map(|(at, _)| at)
    }

    /// What a change woke the loop for, taken from the watcher; `open` is
    /// `false` once the watcher is gone, which ends the monitor saying why.
    pub(super) fn dirtied(&self, open: bool) -> anyhow::Result<Dirty> {
        if !open {
            let why = self
                .watcher
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
                .and_then(|w| w.fault())
                .unwrap_or_else(|| "it could not attach".to_string());
            anyhow::bail!("the repo watcher stopped: {why}");
        }
        Ok(Dirty {
            events: self
                .event_dirty
                .swap(false, std::sync::atomic::Ordering::AcqRel),
            timers: self
                .timer_dirty
                .swap(false, std::sync::atomic::Ordering::AcqRel),
        })
    }

    /// Fold `repo`, counting the fold and clearing any retry only if it
    /// completed.
    pub(super) async fn refold(
        &mut self,
        repo: &std::path::Path,
        busy: &watchdog::Busy,
    ) -> Option<crate::repo_state::RepoState> {
        let (next, state) = advance(repo, self.fold, busy).await?;
        self.fold = next;
        self.retry = None;
        Some(state)
    }

    pub(super) fn folded(&mut self, state: crate::repo_state::RepoState) {
        self.state = Some(state);
    }

    /// Keep what a failed fold could not apply, and try again a little
    /// later than last time.
    pub(super) fn failed(&mut self, dirty: Dirty) {
        // Keep an observed ack/config invalidation until a fold
        // can apply it; clearing dirty must not strand Pending.
        if dirty.events {
            self.event_dirty
                .store(true, std::sync::atomic::Ordering::Release);
        }
        if dirty.timers {
            self.timer_dirty
                .store(true, std::sync::atomic::Ordering::Release);
        }
        let pause = self.retry.map_or(FOLD_FIRST_RETRY, |(_, pause)| {
            (pause * 2).min(FOLD_RETRY_CAP)
        });
        self.retry = Some((tokio::time::Instant::now() + pause, pause));
        eprintln!(
            "monitor: fold failed, held at {}; trying again in {pause:?}",
            self.fold
        );
    }
}

#[cfg(test)]
#[path = "fold_tests.rs"]
mod tests;
