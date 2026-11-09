//! What a watch's polling has done, in the account the monitor
//! publishes, and the judgement of it: a polled source is healthy while
//! it keeps completing polls, whatever those polls find.

use crate::cli::source_status::Health;

/// How many effective intervals may pass without a completed poll
/// before the source is failing. One missed poll is a slow network;
/// three is a watch that has stopped.
pub(crate) const STALE_INTERVALS: u64 = 3;

/// A github watch's polling, as of its last attempt. Times are Unix
/// seconds.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct PollRecord {
    /// When this host began polling: a source that has never completed
    /// a poll is judged from here, so a fresh one is not failing
    /// before its first deadline.
    pub(crate) since: u64,
    /// The effective interval in seconds: `poll_interval`, or GitHub's
    /// `X-Poll-Interval` when that is longer.
    pub(crate) every: u64,
    /// Backoff in force on top of `every`, in seconds.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub(crate) backoff: u64,
    /// When the feed and, for a CI watch, the checks had last both
    /// completed a read (a 304 counts).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) checked: Option<u64>,
    /// Each half whose last read failed, until it next succeeds.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) errors: Vec<PollError>,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// A half of the watch that is failing, since when, and when that half
/// may next be read.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct PollError {
    pub(crate) why: String,
    pub(crate) since: u64,
    pub(crate) retry_at: u64,
    /// GitHub said to stop until a reset.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) rate_limited: bool,
}

impl PollRecord {
    /// Failing on an explicit error or rate limit at once; otherwise
    /// once [`STALE_INTERVALS`] effective intervals, backoff included,
    /// pass with no completed poll.
    pub(crate) fn health(&self, now: u64) -> Health {
        if !self.errors.is_empty() {
            return Health::Failing {
                why: self
                    .errors
                    .iter()
                    .map(|e| e.why.as_str())
                    .collect::<Vec<_>>()
                    .join("; "),
                since: self.errors.iter().map(|e| e.since).min(),
            };
        }
        let from = self.checked.unwrap_or(self.since);
        let deadline = from + STALE_INTERVALS * (self.every + self.backoff);
        if now > deadline {
            let why = match self.checked {
                Some(_) => format!(
                    "no completed poll for {} (expected every {})",
                    crate::age::short(now.saturating_sub(from) as i64),
                    crate::age::short(self.every as i64),
                ),
                None => format!(
                    "no poll has completed in {} of polling",
                    crate::age::short(now.saturating_sub(from) as i64),
                ),
            };
            return Health::Failing {
                why,
                since: Some(deadline),
            };
        }
        Health::Healthy
    }
}

/// Which half of a watch a read was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Half {
    Feed,
    Checks,
}

#[derive(Default)]
struct HalfProgress {
    ok: Option<u64>,
    error: Option<PollError>,
}

/// The poll loop's running account, on the tokio clock so a paused-time
/// test sees the times its own schedule makes.
pub(super) struct Progress {
    epoch: u64,
    start: tokio::time::Instant,
    wants_ci: bool,
    feed: HalfProgress,
    checks: HalfProgress,
}

impl Progress {
    pub(super) fn start(wants_ci: bool) -> Self {
        Self {
            epoch: crate::cli::github::log::now_epoch(),
            start: tokio::time::Instant::now(),
            wants_ci,
            feed: HalfProgress::default(),
            checks: HalfProgress::default(),
        }
    }

    fn at(&self, instant: tokio::time::Instant) -> u64 {
        self.epoch + instant.saturating_duration_since(self.start).as_secs()
    }

    fn half(&mut self, half: Half) -> &mut HalfProgress {
        match half {
            Half::Feed => &mut self.feed,
            Half::Checks => &mut self.checks,
        }
    }

    pub(super) fn succeeded(&mut self, half: Half) {
        let now = self.at(tokio::time::Instant::now());
        let h = self.half(half);
        h.ok = Some(now);
        h.error = None;
    }

    /// `half` failed with `e`, and may next be read at `retry_at`.
    pub(super) fn failed(&mut self, half: Half, e: &anyhow::Error, retry_at: tokio::time::Instant) {
        let now = self.at(tokio::time::Instant::now());
        let retry_at = self.at(retry_at);
        let what = match half {
            Half::Feed => "feed",
            Half::Checks => "checks",
        };
        let why = format!("{what} read failed: {e:#}");
        let rate_limited = e
            .downcast_ref::<crate::cli::github::session::RateLimited>()
            .is_some();
        let h = self.half(half);
        let since = h.error.as_ref().map_or(now, |prev| prev.since);
        h.error = Some(PollError {
            why,
            since,
            retry_at,
            rate_limited,
        });
    }

    /// The record to publish: the effective interval and the backoff
    /// in force.
    pub(super) fn record(
        &self,
        every: std::time::Duration,
        backoff: std::time::Duration,
    ) -> PollRecord {
        let checked = match self.wants_ci {
            true => self.feed.ok.zip(self.checks.ok).map(|(f, c)| f.min(c)),
            false => self.feed.ok,
        };
        PollRecord {
            since: self.epoch,
            every: every.as_secs(),
            backoff: backoff.as_secs(),
            checked,
            errors: [&self.feed.error, &self.checks.error]
                .into_iter()
                .flatten()
                .cloned()
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(every: u64, backoff: u64, checked: Option<u64>) -> PollRecord {
        PollRecord {
            since: 1_000,
            every,
            backoff,
            checked,
            errors: Vec::new(),
        }
    }

    fn failing(r: &PollRecord, now: u64) -> bool {
        matches!(r.health(now), Health::Failing { .. })
    }

    /// A source that keeps completing polls is healthy however little
    /// they find: 304s and empty pages are checks made.
    #[test]
    fn a_source_completing_polls_is_healthy() {
        let r = record(60, 0, Some(2_000));
        assert!(!failing(&r, 2_000 + 180));
        assert!(failing(&r, 2_000 + 181));
    }

    /// The deadline is three EFFECTIVE intervals: a server floor
    /// longer than the configured interval, and backoff in force, both
    /// widen it.
    #[test]
    fn the_deadline_is_three_effective_intervals() {
        let floored = record(300, 0, Some(2_000));
        assert!(!failing(&floored, 2_000 + 900));
        assert!(failing(&floored, 2_000 + 901));
        let backing_off = record(60, 40, Some(2_000));
        assert!(!failing(&backing_off, 2_000 + 300));
        assert!(failing(&backing_off, 2_000 + 301));
        let Health::Failing { since, why } = backing_off.health(2_000 + 301) else {
            unreachable!()
        };
        assert_eq!(since, Some(2_000 + 300));
        assert!(why.contains("no completed poll"), "{why}");
    }

    /// The clock of a source that has never completed a poll starts
    /// when it was hosted.
    #[test]
    fn a_fresh_source_is_judged_from_when_it_was_hosted() {
        let fresh = record(60, 0, None);
        assert!(!failing(&fresh, 1_000 + 180));
        assert!(failing(&fresh, 1_000 + 181));
    }

    /// An explicit error fails the source at once, before any deadline,
    /// and is dated from when it began.
    #[test]
    fn an_explicit_error_fails_at_once() {
        let mut r = record(60, 5, Some(2_000));
        r.errors = vec![PollError {
            why: "feed read failed: 502".into(),
            since: 2_010,
            retry_at: 2_015,
            rate_limited: false,
        }];
        assert_eq!(
            r.health(2_011),
            Health::Failing {
                why: "feed read failed: 502".into(),
                since: Some(2_010)
            }
        );
    }

    /// Each failing half carries its own retry: the other half's
    /// schedule is not when this one is read again.
    #[tokio::test(start_paused = true)]
    async fn an_error_lasts_until_its_half_succeeds_and_keeps_its_start() {
        let every = std::time::Duration::from_secs(60);
        let in_secs = |s| tokio::time::Instant::now() + std::time::Duration::from_secs(s);
        let mut p = Progress::start(true);
        p.succeeded(Half::Feed);
        assert_eq!(
            p.record(every, Default::default()).checked,
            None,
            "a CI watch has completed a poll only once both halves have"
        );
        p.succeeded(Half::Checks);
        assert_eq!(p.record(every, Default::default()).checked, Some(p.epoch));
        tokio::time::advance(std::time::Duration::from_secs(10)).await;
        p.failed(Half::Checks, &anyhow::anyhow!("boom"), in_secs(5));
        tokio::time::advance(std::time::Duration::from_secs(10)).await;
        p.failed(
            Half::Checks,
            &anyhow::Error::new(crate::cli::github::session::RateLimited {
                url: "u".into(),
                retry_in: std::time::Duration::from_secs(600),
            }),
            in_secs(600),
        );
        p.failed(Half::Feed, &anyhow::anyhow!("502"), in_secs(5));
        let failing = p.record(every, Default::default());
        let [feed, checks] = &failing.errors[..] else {
            panic!("both halves: {:?}", failing.errors)
        };
        assert_eq!((feed.since, feed.retry_at), (p.epoch + 20, p.epoch + 25));
        assert_eq!(checks.since, p.epoch + 10, "dated from the first failure");
        assert_eq!(
            checks.retry_at,
            p.epoch + 620,
            "its own retry, not the feed's"
        );
        assert!(checks.rate_limited && checks.why.starts_with("checks read failed"));
        p.succeeded(Half::Feed);
        assert_eq!(
            p.record(every, Default::default()).errors.len(),
            1,
            "the other half succeeding is not this half's recovery"
        );
        p.succeeded(Half::Checks);
        let healed = p.record(every, Default::default());
        assert!(healed.errors.is_empty());
        assert_eq!(healed.checked, Some(p.epoch + 20));
    }
}
