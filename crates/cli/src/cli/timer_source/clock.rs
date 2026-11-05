//! The clock native timers keep: wall seconds for slots, due times and
//! ack coverage, and the monotonic instant a deadline sleeps until.
//!
//! Production reads the system clocks. A `test-clock` build reads the
//! wall time from the file `CLANK_TEST_TIMER_CLOCK` names, when it is set, so a
//! test can hold every clank process's timers on one logical clock and
//! move it: a timer then fires through its own deadline, never because
//! the wall clock happened to cross a slot mid-test.

use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::Instant;

/// Time since the Unix epoch.
pub(crate) fn wall() -> Duration {
    #[cfg(feature = "test-clock")]
    if let Some(at) = logical::wall() {
        return at;
    }
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
}

/// The monotonic instant deadlines are measured against.
pub(crate) fn now() -> Instant {
    #[cfg(feature = "test-clock")]
    if let Some(at) = logical::now() {
        return at;
    }
    Instant::now()
}

/// Wait until `at` on this clock, or forever for `None`.
pub(crate) async fn sleep_until(at: Option<Instant>) {
    #[cfg(feature = "test-clock")]
    if logical::active() {
        return logical::sleep_until(at).await;
    }
    super::deadline(at).await
}

#[cfg(feature = "test-clock")]
mod logical {
    use std::path::{Path, PathBuf};
    use std::sync::OnceLock;
    use std::time::Duration;
    use tokio::time::Instant;

    /// Names a file holding the logical wall time, in whole seconds since
    /// the Unix epoch. Replace it atomically: a reader that meets a torn
    /// write panics rather than guess.
    const VAR: &str = "CLANK_TEST_TIMER_CLOCK";

    /// How often a sleeper rereads the file. A file has no change
    /// notification cheaper than a watcher, and only tests run this.
    const POLL: Duration = Duration::from_millis(10);

    fn file() -> Option<&'static Path> {
        static FILE: OnceLock<Option<PathBuf>> = OnceLock::new();
        FILE.get_or_init(|| std::env::var_os(VAR).map(PathBuf::from))
            .as_deref()
    }

    pub(super) fn active() -> bool {
        file().is_some()
    }

    pub(super) fn wall() -> Option<Duration> {
        let path = file()?;
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("{VAR} names {}: {e}", path.display()));
        let secs = text
            .trim()
            .parse()
            .unwrap_or_else(|e| panic!("{VAR} file holds {text:?}: {e}"));
        Some(Duration::from_secs(secs))
    }

    /// The process's first reading pins logical time to a real instant;
    /// after that, the instant moves only as far as the file does.
    pub(super) fn now() -> Option<Instant> {
        static ORIGIN: OnceLock<(Instant, Duration)> = OnceLock::new();
        let wall = wall()?;
        let (instant, at) = *ORIGIN.get_or_init(|| (Instant::now(), wall));
        Some(instant + wall.saturating_sub(at))
    }

    pub(super) async fn sleep_until(at: Option<Instant>) {
        let Some(at) = at else {
            return std::future::pending().await;
        };
        while now().expect("the logical clock is active") < at {
            tokio::time::sleep(POLL).await;
        }
    }
}
