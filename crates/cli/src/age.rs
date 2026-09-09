//! How long ago something happened, said in one unit.
//!
//! Every duration clank shows is the age of a RECORDED event — a
//! commit's author time, a feedback file's write time — never a clock
//! the process started for itself. This module is where such a time
//! becomes text, and where a record's write time is read.

use std::path::Path;

/// A duration at one significant unit — long enough to judge staleness
/// by eye, short enough for a status line that must not wrap.
pub(crate) fn short(secs: i64) -> String {
    match secs.max(0) {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

/// The resolution of [`coarse`] in seconds. The surfaces that use it
/// repaint on the status loop's idle cadence, and a unit finer than
/// that cadence would show a value the next paint is too late to
/// correct.
pub(crate) const COARSE_RESOLUTION_SECS: i64 = 60;

/// [`short`] at minute resolution: anything under a minute reads
/// `now` rather than a second count that would sit frozen on screen
/// until the next paint.
pub(crate) fn coarse(secs: i64) -> String {
    if secs.max(0) < COARSE_RESOLUTION_SECS {
        return "now".to_string();
    }
    short(secs)
}

/// Seconds between `at` (epoch seconds) and now, floored at zero — a
/// timestamp from the future (clock skew, a commit authored ahead of
/// this machine) is zero seconds old, never a wrapped number.
pub(crate) fn elapsed(at: i64) -> i64 {
    (now() - at).max(0)
}

/// Now, in epoch seconds.
pub(crate) fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// When a record was written, in epoch seconds.
///
/// Reviews carry no time of their own: a feedback file is machine-
/// local (gitignored) and written once, so its mtime IS when the
/// review was made. `None` when it cannot be read — a missing clock
/// shows no age rather than a wrong one.
pub(crate) fn written_at(path: &Path) -> Option<i64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    match modified.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => Some(d.as_secs() as i64),
        // Before the epoch: preserved through a rewrite from a machine
        // whose clock disagreed. Still a real time.
        Err(e) => Some(-(e.duration().as_secs() as i64)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_unit_at_every_boundary() {
        assert_eq!(short(0), "0s");
        assert_eq!(short(59), "59s");
        assert_eq!(short(60), "1m");
        assert_eq!(short(3599), "59m");
        assert_eq!(short(3600), "1h");
        assert_eq!(short(86_399), "23h");
        assert_eq!(short(86_400), "1d");
        assert_eq!(short(2 * 86_400 + 3 * 3600), "2d");
    }

    /// A commit authored ahead of this machine's clock is zero
    /// seconds old — never a wrapped or negative duration.
    #[test]
    fn a_time_from_the_future_is_no_age_at_all() {
        assert_eq!(short(-5), "0s");
        assert_eq!(coarse(-5), "now");
        assert_eq!(elapsed(now() + 3600), 0);
    }

    /// The log's rows repaint on the idle cadence, so their unit must
    /// not be finer than it: a `12s` frozen on screen for a minute is
    /// the lie this resolution exists to avoid.
    #[test]
    fn coarse_never_says_seconds() {
        for s in [0, 1, 30, 59] {
            assert_eq!(coarse(s), "now", "{s}s");
        }
        assert_eq!(coarse(60), "1m");
        assert_eq!(coarse(119), "1m");
        assert_eq!(coarse(3600), "1h");
        assert!(
            (0..=COARSE_RESOLUTION_SECS * 200)
                .all(|s| coarse(s) == "now" || !coarse(s).ends_with('s')),
            "no seconds string at any input"
        );
    }

    #[test]
    fn a_records_write_time_is_read_from_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("feedback.md");
        std::fs::write(&path, "CONTINUE ok\n").unwrap();
        let at = written_at(&path).expect("a written file has a write time");
        assert!(
            (at - now()).abs() < 5,
            "a file written just now: {at} vs {}",
            now()
        );
        assert_eq!(
            written_at(&dir.path().join("absent.md")),
            None,
            "no file, no time — never a fabricated one"
        );
    }
}
