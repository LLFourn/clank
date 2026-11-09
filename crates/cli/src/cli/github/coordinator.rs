//! The emission gate, and the watch loop it guards.
//!
//! Two halves reach an agent through one door, and they are not the
//! same kind of thing: the feed is a stream of id-bearing events that
//! arrive once, the checks are a state re-read on demand.
//! [`Coordinator`] is the single place that decides whether a wake is
//! news — one dedup horizon over action keys — so neither half can
//! emit behind the other's back.
//!
//! [`poll_loop`] is the watch. It polls both halves when it starts and
//! then every effective interval, retries a failed half with backoff,
//! and never stops while it is hosted. GitHub offers no subscription
//! API, so polling is the only way in that needs no public endpoint.

use clank_core::agent_config::GithubSource;

use super::ci::CiReader;
use super::feed::{
    Classified, EventFetcher, FeedHorizon, MAX_PAGES, PollDelta, SEEN_CAP, SeenSet,
    configured_interval, effective_interval, poll_once, row_in_scope,
};
use super::progress::{Half, Progress};
use super::selection::Selection;
use super::session::LoginResolver;

/// How close together the checks may be re-read, whatever asks: an
/// unscoped watch reads every open PR's checks, so a short
/// `poll_interval` must not turn into that many requests as often.
const CI_RECHECK_FLOOR: std::time::Duration = std::time::Duration::from_secs(15);

const RETRY_BASE: std::time::Duration = std::time::Duration::from_secs(5);
const RETRY_CAP: std::time::Duration = std::time::Duration::from_secs(300);

/// The feed half's collaborators: who to ask for events, and who
/// resolves the login its own-action filter needs.
///
/// Grouped because they ARE one half — the same move this plan made
/// for the state it keeps (ruthless de2b1f7). A loop that takes nine
/// loose arguments is one where the two halves can be crossed by
/// accident, which is the mistake this plan kept making.
pub(crate) struct FeedSide<'a, F: EventFetcher, R: LoginResolver> {
    pub(crate) fetcher: &'a F,
    pub(crate) login: &'a R,
}

/// The checks half's collaborators: who to ask for conclusions, and
/// where what it learns persists.
pub(crate) struct ChecksSide<'a, C: CiReader> {
    pub(crate) reader: &'a C,
    /// `None` for an unbound wait with no persistence dir.
    pub(crate) state: Option<std::path::PathBuf>,
}

/// The per-source EMISSION gate (the coordinator model, codex
/// 1ddc0f6/afb7a35): the first read of an action key emits; a later
/// copy drops at the gate. A KEYLESS wake (missing payload identity)
/// is never deduped — emit-always is the safe degradation. The feed
/// BASELINE records pre-watch history and emits nothing.
pub(crate) struct Coordinator {
    interrupt: crate::cli::github::log::Interrupt,
    keys: SeenSet,
    tx: crate::cli::event_source::EmissionSink,
    /// The write-ahead sink: every gate decision lands here BEFORE any
    /// emission (github-offline-catchup). Disabled for sources without
    /// a persistence dir.
    wal: crate::cli::github::log::SharedWal,
}

impl Coordinator {
    /// Test convenience: a coordinator with no persistence.
    #[cfg(test)]
    pub(crate) fn new(tx: crate::cli::event_source::EmissionSink) -> Self {
        Self::with_wal(tx, crate::cli::github::log::SharedWal::disabled())
    }

    pub(crate) fn with_wal(
        tx: crate::cli::event_source::EmissionSink,
        wal: crate::cli::github::log::SharedWal,
    ) -> Self {
        Self {
            interrupt: Default::default(),
            keys: SeenSet::new(SEEN_CAP),
            tx,
            wal,
        }
    }

    /// Restore the durable action-key horizon.
    pub(crate) fn remember_keys(&mut self, keys: impl IntoIterator<Item = String>) {
        self.keys.extend(keys);
    }

    /// Decide, write the event's single atomic WAL record (inbox when
    /// emitting, obs when the action was already emitted), THEN emit.
    /// Returns false when the wait side is gone (the caller should
    /// stop).
    pub(crate) fn offer(&mut self, mut c: Classified) -> bool {
        if let Some(k) = &c.key {
            if self.keys.contains(k) {
                self.wal.obs(c.feed_id.as_deref(), Some(k), false);
                return true;
            }
            self.keys.extend([k.clone()]);
        }
        // The WRITE's outcome rides along: a reader presenting from
        // the log cannot work out whether this one is in it, and the
        // existing behaviour on a failed append is to warn once and
        // keep delivering so the agent still learns about the event.
        let durability = self.wal.selected_inbox(
            crate::cli::github::log::Transport::Poll,
            c.feed_id.as_deref(),
            c.key.as_deref(),
            c.event_at,
            &mut c.item,
            self.interrupt.clone(),
        );
        self.tx.send((c.item, durability)).is_ok()
    }

    /// The BASELINE read's actions, recorded as history — born-handled
    /// baseline records keeping their payload so the timeline can show
    /// pre-watch history (github-watch-resilience) — emitting nothing.
    pub(crate) fn baseline(&mut self, baseline: Vec<Classified>) {
        let history: Vec<Classified> = baseline
            .into_iter()
            .filter(|c| c.key.as_deref().is_none_or(|k| !self.keys.contains(k)))
            .collect();
        self.seed(history);
    }

    /// Record actions as already-known without emitting them: their
    /// keys join the gate and their payloads land as born-handled
    /// baseline records. A CI baseline establishes only the checks'
    /// horizon (a-red-build-is-a-wake), so it is not the feed's
    /// [`baseline`](Self::baseline).
    pub(crate) fn seed(&mut self, actions: Vec<Classified>) {
        for c in actions {
            self.wal
                .baseline_inbox(c.feed_id.as_deref(), c.key.as_deref(), c.event_at, &c.item);
            self.keys.extend(c.key);
        }
    }
}

/// Work a watch owes GitHub: owed until an attempt succeeds, retried
/// with backoff meanwhile. A one-shot deadline that exists only while
/// the work is pending — not a timer.
#[derive(Default)]
struct Owed {
    owed: bool,
    failures: u32,
    retry_at: Option<tokio::time::Instant>,
    /// The wait the last failure chose, until a success clears it.
    backoff: std::time::Duration,
}

impl Owed {
    fn owe(&mut self) {
        self.owed = true;
    }

    /// When the work may run, if it is owed at all.
    fn due(&self, not_before: Option<tokio::time::Instant>) -> Option<tokio::time::Instant> {
        if !self.owed {
            return None;
        }
        let at = match (self.retry_at, not_before) {
            (Some(a), Some(b)) => a.max(b),
            (a, b) => a.or(b).unwrap_or_else(tokio::time::Instant::now),
        };
        Some(at)
    }

    fn ready(&self, not_before: Option<tokio::time::Instant>) -> bool {
        self.due(not_before)
            .is_some_and(|at| at <= tokio::time::Instant::now())
    }

    /// The attempt takes the debt with it, so a reason to owe that
    /// arrives while it runs owes one more attempt after it.
    fn begin(&mut self) {
        self.owed = false;
    }

    fn succeeded(&mut self) {
        self.failures = 0;
        self.retry_at = None;
        self.backoff = std::time::Duration::ZERO;
    }

    fn failed(&mut self, e: &anyhow::Error, at_least: Option<std::time::Duration>) {
        self.owed = true;
        self.failures += 1;
        let mut wait = RETRY_BASE
            .saturating_mul(1 << self.failures.saturating_sub(1).min(16))
            .min(RETRY_CAP)
            .max(at_least.unwrap_or_default());
        if let Some(limited) = e.downcast_ref::<crate::cli::github::session::RateLimited>() {
            wait = wait.max(limited.retry_in);
        }
        self.backoff = wait;
        self.retry_at = Some(tokio::time::Instant::now() + wait);
    }
}

/// The feed's state across reads.
struct FeedState {
    horizon: FeedHorizon,
    own_login: Option<String>,
}

/// One feed reconcile: resolve the own-action filter if it is needed
/// and missing, then one [`poll_once`] against the cursor. `Ok(false)`
/// means the wait is gone. A failed read keeps the committed cursor,
/// so its retry re-reads from there.
async fn read_feed<F: EventFetcher, R: LoginResolver>(
    src: &GithubSource,
    sel: &Selection,
    side: &FeedSide<'_, F, R>,
    state: &mut FeedState,
    coordinator: &mut Coordinator,
    wal: &crate::cli::github::log::SharedWal,
) -> anyhow::Result<bool> {
    // Never read unfiltered: filtering unavailable must NOT degrade to
    // emitting the agent's own actions (codex a989653).
    if sel.needs_own_login() && state.own_login.is_none() {
        let login = side.login.resolve().await.map_err(|e| {
            e.context(
                "own-action filter needs the gh login; not reading the feed until it resolves",
            )
        })?;
        state.own_login = Some(login);
    }
    let feed = &mut state.horizon;
    let outcome = poll_once(
        src,
        &feed.seen.as_btree(),
        feed.etag.as_deref(),
        state.own_login.as_deref(),
        side.fetcher,
    )
    .await;
    // The server floor binds unconditionally — including on a 304
    // and on a read whose deeper page FAILED (codex e9b8b66).
    feed.floor = outcome.poll_interval.or(feed.floor);
    match outcome.result? {
        PollDelta::NotModified => {
            wal.save_state(feed.etag.as_deref(), feed.floor, feed.established);
        }
        PollDelta::Fetched {
            items,
            new_ids,
            overrun,
        } => {
            // Pagination can shift events across pages mid-read, so
            // the SAME presentable event can appear in `items` twice.
            // One event = one gate decision = one primary record
            // (codex 7c9e7fe): keep each feed id's first occurrence.
            let items = {
                let mut seen_ids: std::collections::HashSet<String> =
                    std::collections::HashSet::new();
                let mut deduped = Vec::with_capacity(items.len());
                for c in items {
                    match &c.feed_id {
                        Some(id) if !seen_ids.insert(id.clone()) => {}
                        _ => deduped.push(c),
                    }
                }
                deduped
            };
            // Durable observations for newly observed ids with no
            // presentable item — exactly one atomic primary record
            // per event: presentable ids get their inbox record at
            // the emission gate instead (codex df3f1a7). Computed
            // against the PRE-read cursor; pages can repeat an id
            // within one read, so dedupe locally.
            {
                let presentable: std::collections::HashSet<&str> =
                    items.iter().filter_map(|c| c.feed_id.as_deref()).collect();
                let mut recorded: std::collections::HashSet<&str> =
                    std::collections::HashSet::new();
                for id in &new_ids {
                    if !feed.seen.contains(id)
                        && !presentable.contains(id.as_str())
                        && recorded.insert(id)
                    {
                        wal.obs(Some(id), None, !feed.established);
                    }
                }
            }
            feed.seen.extend(new_ids);
            // An ETag only from a COMPLETE read: conditioning the next
            // one with a failed read's would 304 straight past the
            // unread deeper pages' events.
            feed.etag = outcome.etag.or(feed.etag.take());
            // Overrun is meaningless at the baseline (which pages the
            // whole bounded feed into the cursor by design); only a
            // DELTA read that overran can have lost events.
            if overrun && feed.established {
                eprintln!(
                    "wait: github {} — more than {MAX_PAGES} pages of new events since the \
                     last read; older ones may have been missed",
                    src.repo
                );
            }
            // The FIRST successful read is baseline-only; a failed one
            // does NOT count, so real history is never replayed as
            // new.
            if feed.established {
                for c in items {
                    if !coordinator.offer(c) {
                        return Ok(false);
                    }
                }
            } else {
                coordinator.baseline(items);
            }
            feed.established = true;
            wal.save_state(feed.etag.as_deref(), feed.floor, feed.established);
            wal.compact();
        }
    }
    Ok(true)
}

/// The watch loop: present the warm backlog, then poll.
///
/// - It owes both halves a read at once — the first feed read ever is
///   the baseline — and again every effective interval:
///   `poll_interval` (default 60s) or GitHub's `X-Poll-Interval`,
///   whichever is longer.
/// - A failed half is retried with backoff, honouring the floor and a
///   rate limit's reset, until it succeeds; the timer keeps running.
/// - Each attempt is reported as [`Lifecycle::Polling`] on `key`.
///
/// The own-action filter is fail-closed: until the login resolves,
/// the feed is not read. Generic over the fetcher, login resolver and
/// CI reader, so tested end-to-end without `gh`.
///
/// [`Lifecycle::Polling`]: crate::cli::event_source::Lifecycle::Polling
pub(crate) async fn poll_loop<F: EventFetcher, R: LoginResolver, C: CiReader>(
    src: &GithubSource,
    feed_side: FeedSide<'_, F, R>,
    checks: ChecksSide<'_, C>,
    tx: &crate::cli::event_source::EmissionSink,
    key: &str,
    mut log: Option<crate::cli::github::log::EventLog>,
    suppress_backlog: std::collections::HashSet<(u64, String)>,
) {
    let sel = Selection::of(src);
    let wants_ci = sel.wants_ci();
    let every = configured_interval(src);
    let mut progress = Progress::start(wants_ci);
    let mut feed = FeedState {
        horizon: FeedHorizon {
            seen: SeenSet::new(SEEN_CAP),
            etag: None,
            floor: None,
            established: false,
        },
        own_login: None,
    };
    // CI's horizon is its own object with its own file: read from the
    // PRs rather than the feed, established on its own terms, and
    // remembered per PR. Sharing the feed's state is what made one
    // half's history establish the other and let eviction re-announce
    // unchanged builds (codex 7c132ec, ae48e92).
    let mut ci_horizon = crate::cli::github::ci::CiHorizon::load(checks.state.as_deref());

    // Warm start (github-offline-catchup): rebuild both horizons from
    // the WAL and re-present the unhandled backlog IMMEDIATELY — an
    // agent arming with unhandled events is woken for them before any
    // network I/O (and before the own-login gate: the backlog was
    // already filtered when it was logged). A warm cursor makes the
    // catch-up a RECONCILE — events that fired while no wait was
    // armed emit instead of being swallowed by a fresh baseline.
    let mut warm_keys: Vec<String> = Vec::new();
    if let Some(l) = log.as_mut() {
        let loaded = l.load();
        if loaded.log_quarantined {
            eprintln!(
                "wait: github {} — event log was corrupt; quarantined to .corrupt and \
                 re-baselining (unhandled events, if any, were lost)",
                src.repo
            );
            // Say it durably, not just on stdout. The sidecar
            // outlives the log it describes, and its "the feed
            // baselined" is a statement about a horizon that no
            // longer exists — left standing, the next arm restores a
            // stale ETag against a cursor nothing can reconstruct and
            // emits the first page as new wakes. Written now, because
            // a failed fetch writes nothing and another restart would
            // find the old claim again (codex 4a6b328).
            let _ = l.save_state(None, None, false);
        }
        if loaded.state_corrupt {
            eprintln!(
                "wait: github {} — event cache state was corrupt; resetting only the \
                 etag/poll-interval cache",
                src.repo
            );
        }
        if loaded.foreign > 0 {
            eprintln!(
                "wait: github {} — {} event-log record(s) from another clank version \
                 preserved but skipped",
                src.repo, loaded.foreign
            );
        }
        for entry in &loaded.unhandled {
            // A takeover's tail phase already presented these LOGICAL
            // events IN THIS PROCESS — re-presenting them in the same
            // wait would double-wake (codex 8bf682e/20cdd09). Keyed by
            // (seq, ident): distinct damaged events at one seq
            // suppress independently. Horizons and coordinator state
            // still load fully.
            if suppress_backlog.contains(&(entry.seq, entry.ident.clone())) {
                continue;
            }
            if !row_in_scope(src, &entry.item) {
                continue;
            }
            // Read back OUT of the log, so the log has it.
            let mut item = entry.item.clone();
            item.set_presentation(clank_core::wait::EventPresentation {
                source: l.source_key().to_owned(),
                seq: entry.seq,
                ident: entry.ident.clone(),
            });
            if tx
                .send((item, crate::cli::event_source::Durability::Logged))
                .is_err()
            {
                return; // wait gone
            }
        }
        feed.horizon = FeedHorizon::restore(&loaded);
        warm_keys = loaded.action_keys;
    }
    let wal = crate::cli::github::log::SharedWal::new(log);
    let mut coordinator = Coordinator::with_wal(tx.clone(), wal.clone());
    coordinator.interrupt =
        crate::cli::github::log::Interrupt::new(src.interrupt, src.interrupt_message.as_deref());
    coordinator.remember_keys(warm_keys);

    let mut feed_work = Schedule::owed_now(every, feed.horizon.floor);
    let mut checks_work = Schedule::owed_now(every, feed.horizon.floor);
    // PERMISSION TO LOOK AGAIN, advanced by every attempt whatever it
    // returns: advancing it only on success would let a broken checks
    // endpoint be hit on every tick of a short interval.
    let mut checks_not_before: Option<tokio::time::Instant> = None;
    let report = |progress: &Progress, feed_work: &Schedule, checks_work: &Schedule, floor| {
        tx.report(
            key,
            crate::cli::event_source::Lifecycle::Polling(progress.record(
                effective_interval(every, floor),
                feed_work.work.backoff.max(checks_work.work.backoff),
            )),
        );
    };
    report(&progress, &feed_work, &checks_work, feed.horizon.floor);

    loop {
        let now = tokio::time::Instant::now();
        feed_work.tick(now);
        checks_work.tick(now);
        // The checks come BEFORE the feed and keep their own horizon:
        // a conclusion is nobody's action, so the own-action filter
        // has nothing to say about it, and waiting on a login it does
        // not need would hold a red build behind an unrelated outage.
        if wants_ci && checks_work.work.ready(checks_not_before) {
            checks_work.work.begin();
            checks_not_before = Some(tokio::time::Instant::now() + CI_RECHECK_FLOOR);
            match checks.reader.read().await {
                Ok(results) => {
                    checks_work.work.succeeded();
                    progress.succeeded(Half::Checks);
                    let wakes = crate::cli::github::ci::wakes(&src.repo, &sel, &results);
                    let in_scope: std::collections::BTreeSet<u64> =
                        results.iter().map(|(pr, _)| pr.number).collect();
                    for c in ci_horizon.absorb(wakes, &in_scope) {
                        if !coordinator.offer(c) {
                            return; // wait gone
                        }
                    }
                    // A completed read is its OWN durability point:
                    // waiting for the feed to persist it would put
                    // this half's memory back under the other's
                    // availability.
                    ci_horizon.save(checks.state.as_deref());
                }
                Err(e) => {
                    checks_work.work.failed(&e, None);
                    let retry = checks_work.wake(checks_not_before);
                    progress.failed(Half::Checks, &e, retry);
                    eprintln!("wait: github {} ci read failed: {e:#}", src.repo);
                }
            }
            checks_work.attempted(every, feed.horizon.floor);
            report(&progress, &feed_work, &checks_work, feed.horizon.floor);
            continue;
        }
        if feed_work.work.ready(None) {
            feed_work.work.begin();
            match read_feed(src, &sel, &feed_side, &mut feed, &mut coordinator, &wal).await {
                Ok(true) => {
                    feed_work.work.succeeded();
                    progress.succeeded(Half::Feed);
                }
                Ok(false) => return, // wait gone
                Err(e) => {
                    // GitHub's X-Poll-Interval binds a retry too.
                    let floor = feed.horizon.floor.map(std::time::Duration::from_secs);
                    feed_work.work.failed(&e, floor);
                    progress.failed(Half::Feed, &e, feed_work.wake(None));
                    eprintln!("wait: github {} feed read failed: {e:#}", src.repo);
                }
            }
            // Under the floor this read just learned.
            feed_work.attempted(every, feed.horizon.floor);
            report(&progress, &feed_work, &checks_work, feed.horizon.floor);
            continue;
        }

        let deadline = match wants_ci {
            true => feed_work
                .wake(None)
                .min(checks_work.wake(checks_not_before)),
            false => feed_work.wake(None),
        };
        tokio::time::sleep_until(deadline).await;
    }
}

/// One half's reads: its periodic tick, and the debt a failure leaves
/// until a retry succeeds. Each half keeps its own, so a retry of one
/// never moves the other's cadence.
struct Schedule {
    work: Owed,
    tick: tokio::time::Instant,
}

impl Schedule {
    /// Owed at once, the start's catch-up.
    fn owed_now(every: std::time::Duration, floor: Option<u64>) -> Self {
        let mut work = Owed::default();
        work.owe();
        Self {
            work,
            tick: tokio::time::Instant::now() + effective_interval(every, floor),
        }
    }

    /// Owe a read once the periodic tick has come.
    fn tick(&mut self, now: tokio::time::Instant) {
        if !self.work.owed && self.tick <= now {
            self.work.owe();
        }
    }

    /// The next periodic read counts from this attempt, whatever it
    /// returned.
    fn attempted(&mut self, every: std::time::Duration, floor: Option<u64>) {
        self.tick = tokio::time::Instant::now() + effective_interval(every, floor);
    }

    /// When this half next wants to run: its debt's retry while owed,
    /// else its tick.
    fn wake(&self, not_before: Option<tokio::time::Instant>) -> tokio::time::Instant {
        self.work.due(not_before).unwrap_or(self.tick)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::github::ci;
    use clank_core::wait::WaitItem;
    // The session's fakes live with the session; these two tests
    // drive the FETCHER over it, so they stay here — and so does
    // their own fake, which speaks the session's response type.
    use std::time::Duration;

    use clank_core::agent_config::GithubEventKind;

    use crate::cli::github::ci::tests::{DeadCi, NoCi, ScriptCi, red};
    use crate::cli::github::feed::tests::{ScriptFetcher, classified, ev, gh_item, issue, ok, src};
    use crate::cli::github::feed::{GhFetcher, GhResponse};
    use crate::cli::github::log::tests::{open_log, wal_tempdir};
    use crate::cli::github::session::tests::{FixedLogin, ScriptLogin};
    use crate::cli::github::session::{GithubSession, HttpSend, MiniResponse};
    use std::sync::Mutex;

    #[test]
    fn interruption_applies_to_new_events_but_never_the_baseline() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let wal = crate::cli::github::log::SharedWal::new(Some(open_log(dir.path())));
        let mut coordinator = Coordinator::with_wal(tx.into(), wal);
        coordinator.interrupt =
            crate::cli::github::log::Interrupt::new(true, Some("New PR activity"));
        coordinator.baseline(vec![classified(1, Some("baseline"))]);
        assert!(coordinator.offer(classified(2, Some("fresh"))));
        assert!(rx.try_recv().is_ok());
        let rows = open_log(dir.path()).read_rows().unwrap().unwrap().rows;
        assert!(!rows[0].interrupt);
        assert!(rows[1].interrupt);
        assert_eq!(
            rows[0].interrupt_hint, None,
            "the baseline never interrupts"
        );
        assert_eq!(rows[1].interrupt_hint.as_deref(), Some("New PR activity"));
        coordinator.interrupt = false.into();
        assert!(coordinator.offer(classified(2, Some("fresh"))));
        let rows = open_log(dir.path()).read_rows().unwrap().unwrap().rows;
        assert_eq!(rows.len(), 2);
        assert!(rows[1].interrupt);
    }

    // ── the coordinator: one emission owner, baseline horizon ──

    #[tokio::test]
    async fn coordinator_gates_emission_by_action_key() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::cli::event_source::EmissionSink::from(tx);
        let mut c = Coordinator::new(tx);
        c.baseline(vec![classified(1, Some("issue_opened#1"))]);
        assert!(rx.try_recv().is_err(), "the baseline emits nothing");
        assert!(c.offer(classified(2, Some("issue_opened#2"))));
        assert!(c.offer(classified(2, Some("issue_opened#2"))));
        assert!(rx.try_recv().is_ok(), "the first read emitted");
        assert!(rx.try_recv().is_err(), "a later copy dropped");
        assert!(c.offer(classified(1, Some("issue_opened#1"))));
        assert!(rx.try_recv().is_err(), "history is not news");
        // KEYLESS wakes are never deduped.
        assert!(c.offer(classified(9, None)));
        assert!(c.offer(classified(9, None)));
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_ok(), "keyless = emit-always");
    }

    // ── bounded transport: deadlines at the ownership boundary ──

    /// Hangs on the FIRST send forever; later sends return 200 with
    /// one page of nothing. The call counter is shared with the test.
    struct HangFirstHttp {
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }
    impl HttpSend for HangFirstHttp {
        async fn send(
            &self,
            _url: &str,
            _bearer: &str,
            _etag: Option<&str>,
        ) -> anyhow::Result<MiniResponse> {
            if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                std::future::pending::<()>().await;
            }
            Ok(MiniResponse {
                status: 200,
                headers: Default::default(),
                body: "[]".to_string(),
            })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn hung_send_fails_the_tick_and_the_next_tick_proceeds() {
        // github-watch-resilience: the deadline is the SESSION's, so a
        // hung HttpSend (which bypasses any reqwest limit) still
        // becomes a bounded tick error, and the loop's next tick
        // recovers — no source-task ever awaits forever.
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let session = std::sync::Arc::new(
            GithubSession::for_tests(
                HangFirstHttp {
                    calls: calls.clone(),
                },
                || Box::pin(async { Ok("tok".to_string()) }),
            )
            .with_timeouts(
                std::time::Duration::from_secs(30),
                std::time::Duration::from_secs(15),
            ),
        );
        let fetcher = GhFetcher {
            repo: "o/r".to_string(),
            session: session.clone(),
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::cli::event_source::EmissionSink::from(tx);
        let s = src(&[GithubEventKind::IssueOpened], true);
        let run = tokio::spawn(async move {
            poll_loop(
                &s,
                FeedSide {
                    fetcher: &fetcher,
                    login: &FixedLogin("me"),
                },
                ChecksSide {
                    reader: &NoCi,
                    state: None,
                },
                &tx,
                "k",
                None,
                Default::default(),
            )
            .await;
        });
        // Tick 1 hangs → times out at 30s; the loop sleeps the
        // interval and tick 2's send succeeds (empty baseline). Give
        // the paused clock several minutes of virtual time.
        tokio::time::sleep(std::time::Duration::from_secs(300)).await;
        assert!(
            calls.load(std::sync::atomic::Ordering::SeqCst) >= 2,
            "the loop must reach a SECOND send after the hung first one"
        );
        assert!(rx.try_recv().is_err(), "empty feed emits nothing");
        run.abort();
        let _ = run.await;
    }

    #[tokio::test(start_paused = true)]
    async fn poll_loop_baselines_then_emits_new_events() {
        // The REAL poll_loop, paused-time so its inter-tick sleeps
        // resolve instantly. Tick 1 (baseline): a new event + a seen id
        // (fully-seen page 2 ends baseline) — emits NOTHING. Tick 2: a
        // genuinely new event → emitted. Then 304 forever; dropping the
        // receiver ends the loop.
        let f = ScriptFetcher::new(vec![
            // Tick 1 baseline: page 1 has id 1 (new at baseline) + a
            // marker seen id to end pagination.
            Box::new(|_p, _e| Ok(ok(vec![issue("1", 1)], Some("\"e1\""), None))),
            Box::new(|_p, _e| Ok(ok(vec![], None, None))), // baseline page 2 empty → done
            // Tick 2: id 2 new, then id 1 (now seen) ends pagination.
            Box::new(|_p, _e| Ok(ok(vec![issue("2", 2), issue("1", 1)], Some("\"e2\""), None))),
        ]);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::cli::event_source::EmissionSink::from(tx);
        let handle = tokio::spawn(async move {
            poll_loop(
                &src(&[GithubEventKind::IssueOpened], false),
                FeedSide {
                    fetcher: &f,
                    login: &FixedLogin("me"),
                },
                ChecksSide {
                    reader: &NoCi,
                    state: None,
                },
                &tx,
                "k",
                None,
                Default::default(),
            )
            .await;
        });
        // The first emitted item is id 2 (baseline id 1 was suppressed).
        let item = tokio::time::timeout(Duration::from_secs(300), rx.recv())
            .await
            .expect("an event is emitted within a few ticks")
            .expect("channel open");
        match item.0 {
            WaitItem::GithubEvent { number, .. } => assert_eq!(number, Some(2)),
            other => panic!("expected the new event, got {other:?}"),
        }
        // Finish deterministically: abort the source task and join it
        // (codex a989653 — don't leave the JoinHandle to time out).
        handle.abort();
        let _ = handle.await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_build_that_was_already_red_is_not_news_but_a_new_one_is() {
        // The first read is the state of the world (seeded, silent).
        // The same conclusion on the same head stays silent however
        // many ticks pass. A new head that fails is a wake.
        let ci = ScriptCi::new(vec![
            vec![red(7, "head1", "fmt")],
            vec![red(7, "head1", "fmt")],
            vec![red(7, "head2", "test")],
        ]);
        let f = ScriptFetcher::new(vec![Box::new(|_p, _e| Ok(ok(vec![], None, None)))]);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::cli::event_source::EmissionSink::from(tx);
        let handle = tokio::spawn(async move {
            poll_loop(
                &src(&[GithubEventKind::CiFailed], true),
                FeedSide {
                    fetcher: &f,
                    login: &FixedLogin("me"),
                },
                ChecksSide {
                    reader: &ci,
                    state: None,
                },
                &tx,
                "k",
                None,
                Default::default(),
            )
            .await;
        });
        let item = tokio::time::timeout(Duration::from_secs(300), rx.recv())
            .await
            .expect("the new head's failure wakes")
            .expect("channel open");
        let (
            WaitItem::GithubEvent {
                event,
                number,
                title,
                ..
            },
            _,
        ) = item
        else {
            panic!("a github event");
        };
        assert_eq!(event, "ci_failed");
        assert_eq!(number, Some(7));
        assert_eq!(
            title.as_deref(),
            Some("CI failed: test"),
            "the wake is the NEW head's failure, not the seeded one",
        );
        handle.abort();
        let _ = handle.await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_broken_checks_read_does_not_stop_the_feed() {
        // Separate horizons: the CI read failing every tick must not
        // cost the feed its baseline or its wakes.
        let f = ScriptFetcher::new(vec![
            Box::new(|_p, _e| Ok(ok(vec![issue("1", 1)], None, None))),
            Box::new(|_p, _e| Ok(ok(vec![], None, None))),
            Box::new(|_p, _e| Ok(ok(vec![issue("2", 2), issue("1", 1)], None, None))),
        ]);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::cli::event_source::EmissionSink::from(tx);
        let handle = tokio::spawn(async move {
            poll_loop(
                &src(
                    &[GithubEventKind::IssueOpened, GithubEventKind::CiFailed],
                    true,
                ),
                FeedSide {
                    fetcher: &f,
                    login: &FixedLogin("me"),
                },
                ChecksSide {
                    reader: &DeadCi,
                    state: None,
                },
                &tx,
                "k",
                None,
                Default::default(),
            )
            .await;
        });
        let item = tokio::time::timeout(Duration::from_secs(300), rx.recv())
            .await
            .expect("the feed still wakes")
            .expect("channel open");
        match item.0 {
            WaitItem::GithubEvent { number, .. } => assert_eq!(number, Some(2)),
            other => panic!("expected the feed's event, got {other:?}"),
        }
        handle.abort();
        let _ = handle.await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_broken_feed_does_not_stop_the_checks() {
        // The other half of the separate horizons: a feed that errors
        // every tick must not hold back a build's wake. The feed's
        // baseline never establishes here — the CI horizon is its own.
        struct DeadFeed;
        impl EventFetcher for DeadFeed {
            async fn fetch(&self, _page: u32, _etag: Option<String>) -> anyhow::Result<GhResponse> {
                anyhow::bail!("events unreachable")
            }
        }
        let ci = ScriptCi::new(vec![
            vec![red(7, "head1", "fmt")],
            vec![red(7, "head2", "test")],
        ]);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::cli::event_source::EmissionSink::from(tx);
        let handle = tokio::spawn(async move {
            poll_loop(
                &src(
                    &[GithubEventKind::IssueOpened, GithubEventKind::CiFailed],
                    true,
                ),
                FeedSide {
                    fetcher: &DeadFeed,
                    login: &FixedLogin("me"),
                },
                ChecksSide {
                    reader: &ci,
                    state: None,
                },
                &tx,
                "k",
                None,
                Default::default(),
            )
            .await;
        });
        let item = tokio::time::timeout(Duration::from_secs(300), rx.recv())
            .await
            .expect("the build's wake does not wait on the feed")
            .expect("channel open");
        let (WaitItem::GithubEvent { event, title, .. }, _) = item else {
            panic!("a github event");
        };
        assert_eq!(event, "ci_failed");
        assert_eq!(title.as_deref(), Some("CI failed: test"));
        handle.abort();
        let _ = handle.await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_build_wakes_even_while_the_own_action_filter_is_down() {
        // The gate exists so the agent's own actions cannot wake it.
        // A conclusion is nobody's action, so holding it behind a
        // login that will not resolve would keep a red build silent
        // for a reason that does not apply to it.
        struct NoLogin;
        impl LoginResolver for NoLogin {
            async fn resolve(&self) -> anyhow::Result<String> {
                anyhow::bail!("gh down")
            }
        }
        let ci = ScriptCi::new(vec![
            vec![red(7, "head1", "fmt")],
            vec![red(7, "head2", "test")],
        ]);
        let f = ScriptFetcher::new(vec![]);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::cli::event_source::EmissionSink::from(tx);
        let handle = tokio::spawn(async move {
            poll_loop(
                // include_own_actions = false: the gate is armed.
                &src(&[GithubEventKind::CiFailed], false),
                FeedSide {
                    fetcher: &f,
                    login: &NoLogin,
                },
                ChecksSide {
                    reader: &ci,
                    state: None,
                },
                &tx,
                "k",
                None,
                Default::default(),
            )
            .await;
        });
        let item = tokio::time::timeout(Duration::from_secs(300), rx.recv())
            .await
            .expect("a conclusion does not wait on the actor filter")
            .expect("channel open");
        let (WaitItem::GithubEvent { event, actor, .. }, _) = item else {
            panic!("a github event");
        };
        assert_eq!(event, "ci_failed");
        assert_eq!(actor, None, "nothing to filter in the first place");
        handle.abort();
        let _ = handle.await;
    }

    /// codex 7c132ec, probe 1: a log holding only CI rows must not
    /// mark the FEED established. It did, so the feed's first real
    /// fetch emitted its existing history instead of baselining it.
    #[tokio::test(start_paused = true)]
    async fn one_halfs_history_does_not_establish_the_other() {
        let dir = wal_tempdir();
        let s1 = src(
            &[GithubEventKind::IssueOpened, GithubEventKind::CiFailed],
            true,
        );
        // Arm 1: the events endpoint is down; the checks read works.
        struct DeadFeed;
        impl EventFetcher for DeadFeed {
            async fn fetch(&self, _page: u32, _etag: Option<String>) -> anyhow::Result<GhResponse> {
                anyhow::bail!("events unreachable")
            }
        }
        let ci = ScriptCi::new(vec![vec![red(7, "head1", "fmt")]]);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::cli::event_source::EmissionSink::from(tx);
        let d = dir.clone();
        let run1 = tokio::spawn(async move {
            poll_loop(
                &s1,
                FeedSide {
                    fetcher: &DeadFeed,
                    login: &FixedLogin("me"),
                },
                ChecksSide {
                    reader: &ci,
                    state: Some(d.join("t.ci.json")),
                },
                &tx,
                "k",
                Some(open_log(&d)),
                Default::default(),
            )
            .await;
        });
        tokio::time::sleep(Duration::from_secs(120)).await;
        assert!(rx.try_recv().is_err(), "the first checks read is seeded");
        run1.abort();
        let _ = run1.await;

        // Arm 2: the feed comes back and returns history it has never
        // shown us. That is a BASELINE, not a backlog of wakes.
        let f = ScriptFetcher::new(vec![Box::new(|_p, _e| {
            Ok(ok(vec![issue("1", 1), issue("2", 2)], None, None))
        })]);
        let ci2 = ScriptCi::new(vec![vec![red(7, "head1", "fmt")]]);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::cli::event_source::EmissionSink::from(tx);
        let s2 = src(
            &[GithubEventKind::IssueOpened, GithubEventKind::CiFailed],
            true,
        );
        let d = dir.clone();
        let run2 = tokio::spawn(async move {
            poll_loop(
                &s2,
                FeedSide {
                    fetcher: &f,
                    login: &FixedLogin("me"),
                },
                ChecksSide {
                    reader: &ci2,
                    state: Some(d.join("t.ci.json")),
                },
                &tx,
                "k",
                Some(open_log(&d)),
                Default::default(),
            )
            .await;
        });
        tokio::time::sleep(Duration::from_secs(120)).await;
        assert!(
            rx.try_recv().is_err(),
            "the feed's own history is baselined, not replayed as wakes",
        );
        run2.abort();
        let _ = run2.await;
    }

    /// The inverse: a feed-only log must not mark CI established,
    /// which would swallow the first conclusion it ever reads.
    #[tokio::test(start_paused = true)]
    async fn a_feed_only_log_still_seeds_the_first_checks_read() {
        let dir = wal_tempdir();
        let f = ScriptFetcher::new(vec![Box::new(|_p, _e| {
            Ok(ok(vec![issue("1", 1)], None, None))
        })]);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::cli::event_source::EmissionSink::from(tx);
        let s1 = src(&[GithubEventKind::IssueOpened], true);
        let d = dir.clone();
        let run1 = tokio::spawn(async move {
            poll_loop(
                &s1,
                FeedSide {
                    fetcher: &f,
                    login: &FixedLogin("me"),
                },
                ChecksSide {
                    reader: &NoCi,
                    state: Some(d.join("t.ci.json")),
                },
                &tx,
                "k",
                Some(open_log(&d)),
                Default::default(),
            )
            .await;
        });
        tokio::time::sleep(Duration::from_secs(120)).await;
        run1.abort();
        let _ = run1.await;

        // Arm 2 adds a CI kind: its first read is the state of the
        // world, so a build that was ALREADY red says nothing, and
        // the next one that changes does.
        let ci = ScriptCi::new(vec![
            vec![red(7, "head1", "fmt")],
            vec![red(7, "head2", "test")],
        ]);
        let f2 = ScriptFetcher::new(vec![]);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::cli::event_source::EmissionSink::from(tx);
        let s2 = src(
            &[GithubEventKind::IssueOpened, GithubEventKind::CiFailed],
            true,
        );
        let d = dir.clone();
        let run2 = tokio::spawn(async move {
            poll_loop(
                &s2,
                FeedSide {
                    fetcher: &f2,
                    login: &FixedLogin("me"),
                },
                ChecksSide {
                    reader: &ci,
                    state: Some(d.join("t.ci.json")),
                },
                &tx,
                "k",
                Some(open_log(&d)),
                Default::default(),
            )
            .await;
        });
        let item = tokio::time::timeout(Duration::from_secs(300), rx.recv())
            .await
            .expect("the CHANGED conclusion wakes")
            .expect("open");
        let (WaitItem::GithubEvent { title, .. }, _) = item else {
            panic!("a github event");
        };
        assert_eq!(
            title.as_deref(),
            Some("CI failed: test"),
            "head1's failure was the baseline, head2's is the news",
        );
        run2.abort();
        let _ = run2.await;
    }

    /// codex 7c132ec, probe 2: the unchanged conclusion's memory must
    /// outlive a restart. It used to live in the shared key horizon,
    /// a recency window over EVENTS — but a state read re-observes
    /// the same conclusion forever, so eviction woke the same build
    /// again.
    #[tokio::test(start_paused = true)]
    async fn an_unchanged_build_stays_quiet_across_a_restart() {
        let dir = wal_tempdir();
        for arm in 0..2 {
            let ci = ScriptCi::new(vec![vec![red(7, "head1", "fmt")]]);
            let f = ScriptFetcher::new(vec![]);
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let tx = crate::cli::event_source::EmissionSink::from(tx);
            let s = src(&[GithubEventKind::CiFailed], true);
            let d = dir.clone();
            let handle = tokio::spawn(async move {
                poll_loop(
                    &s,
                    FeedSide {
                        fetcher: &f,
                        login: &FixedLogin("me"),
                    },
                    ChecksSide {
                        reader: &ci,
                        state: None,
                    },
                    &tx,
                    "k",
                    Some(open_log(&d)),
                    Default::default(),
                )
                .await;
            });
            tokio::time::sleep(Duration::from_secs(600)).await;
            assert!(
                rx.try_recv().is_err(),
                "arm {arm}: the same red build on the same head is not news",
            );
            handle.abort();
            let _ = handle.await;
            // Compaction rewrites the log; the CI memory is not in it.
            open_log(&dir).maybe_compact().unwrap();
        }
    }

    /// codex 8043f14: the log outlives the meaning of the scope. This
    /// watch's config did not change — `[7]` then and now — so it
    /// keeps its key, and its log holds a comment on 8 that was
    /// legitimately accepted when `prs` scoped CI alone. The backlog
    /// replay never classifies anything, so only a rule applied THERE
    /// keeps that comment from waking it.
    #[tokio::test(start_paused = true)]
    async fn a_widened_meaning_is_applied_to_the_old_log() {
        let dir = wal_tempdir();
        // An old log: comments on 7 AND 8, neither acked.
        {
            let mut log = open_log(&dir);
            for number in [7u64, 8] {
                log.append_inbox(
                    crate::cli::github::log::Transport::Poll,
                    Some(&format!("feed{number}")),
                    Some(&format!("pr_issue_comment#{number}")),
                    None,
                    &gh_item("pr_comment", Some(number)),
                )
                .unwrap();
            }
        }

        let mut s = src(&[GithubEventKind::PrComment], true);
        s.prs = vec![7];
        let f = ScriptFetcher::new(vec![]);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::cli::event_source::EmissionSink::from(tx);
        let d = dir.clone();
        let handle = tokio::spawn(async move {
            poll_loop(
                &s,
                FeedSide {
                    fetcher: &f,
                    login: &FixedLogin("me"),
                },
                ChecksSide {
                    reader: &NoCi,
                    state: None,
                },
                &tx,
                "k",
                Some(open_log(&d)),
                Default::default(),
            )
            .await;
        });
        tokio::time::sleep(Duration::from_secs(5)).await;
        handle.abort();
        let _ = handle.await;

        let mut numbers = Vec::new();
        while let Ok((item, _)) = rx.try_recv() {
            if let WaitItem::GithubEvent { number, .. } = item {
                numbers.push(number);
            }
        }
        assert_eq!(
            numbers,
            vec![Some(7)],
            "the backlog is replayed through the scope, not around it",
        );
        // The row for 8 is still in the log, unacked: out of scope is
        // not the same as handled, and narrowing a watch must not
        // silently ack somebody's history.
        let rows = open_log(&dir).read_rows().unwrap().unwrap().rows;
        assert!(
            rows.iter().any(|r| !r.acked
                && matches!(
                    &r.item,
                    WaitItem::GithubEvent {
                        number: Some(8),
                        ..
                    }
                )),
            "8's row is untouched, just unpresented",
        );
    }

    /// codex ae48e92 [P1]: a state file written before any of this
    /// existed must still restore feed catch-up. It does not name the
    /// feed's horizon at all any more — the WAL's own feed ids are
    /// the evidence — so a legacy file is simply a file.
    #[tokio::test(start_paused = true)]
    async fn a_state_file_from_before_this_plan_keeps_catch_up() {
        let dir = wal_tempdir();
        let f = ScriptFetcher::new(vec![Box::new(|_p, _e| {
            Ok(ok(vec![issue("1", 1)], None, None))
        })]);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::cli::event_source::EmissionSink::from(tx);
        let s1 = src(&[GithubEventKind::IssueOpened], true);
        let d = dir.clone();
        let run1 = tokio::spawn(async move {
            poll_loop(
                &s1,
                FeedSide {
                    fetcher: &f,
                    login: &FixedLogin("me"),
                },
                ChecksSide {
                    reader: &NoCi,
                    state: Some(d.join("t.ci.json")),
                },
                &tx,
                "k",
                Some(open_log(&d)),
                Default::default(),
            )
            .await;
        });
        tokio::time::sleep(Duration::from_secs(120)).await;
        run1.abort();
        let _ = run1.await;

        // Rewrite the cache the way an older clank wrote it: etag,
        // floor, saved_at, and nothing else.
        std::fs::write(
            dir.join("t.state.json"),
            r#"{"poll_interval_floor":60,"saved_at":1}"#,
        )
        .unwrap();

        let f = ScriptFetcher::new(vec![Box::new(|_p, _e| {
            Ok(ok(vec![issue("2", 2), issue("1", 1)], None, None))
        })]);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::cli::event_source::EmissionSink::from(tx);
        let s2 = src(&[GithubEventKind::IssueOpened], true);
        let d = dir.clone();
        let run2 = tokio::spawn(async move {
            poll_loop(
                &s2,
                FeedSide {
                    fetcher: &f,
                    login: &FixedLogin("me"),
                },
                ChecksSide {
                    reader: &NoCi,
                    state: Some(d.join("t.ci.json")),
                },
                &tx,
                "k",
                Some(open_log(&d)),
                Default::default(),
            )
            .await;
        });
        let woke = tokio::time::timeout(Duration::from_secs(300), rx.recv())
            .await
            .expect("the event missed while away still wakes after an upgrade")
            .expect("open");
        let (WaitItem::GithubEvent { number, .. }, _) = woke else {
            panic!("a github event");
        };
        assert_eq!(number, Some(2));
        run2.abort();
        let _ = run2.await;
    }

    /// The purest form of codex 4b6035b: a feed that baselined on an
    /// empty feed and wrote NOTHING at all. No rows, so no evidence —
    /// the recorded fact is the only thing that can say it happened,
    /// and without it the next arm re-baselines and swallows the
    /// event that arrived in between.
    #[tokio::test(start_paused = true)]
    async fn an_empty_baseline_that_wrote_nothing_is_still_a_baseline() {
        let dir = wal_tempdir();
        let f = ScriptFetcher::new(vec![Box::new(|_p, _e| Ok(ok(vec![], None, None)))]);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::cli::event_source::EmissionSink::from(tx);
        let s1 = src(&[GithubEventKind::IssueOpened], true);
        let d = dir.clone();
        let run1 = tokio::spawn(async move {
            poll_loop(
                &s1,
                FeedSide {
                    fetcher: &f,
                    login: &FixedLogin("me"),
                },
                ChecksSide {
                    reader: &NoCi,
                    state: Some(d.join("t.ci.json")),
                },
                &tx,
                "k",
                Some(open_log(&d)),
                Default::default(),
            )
            .await;
        });
        tokio::time::sleep(Duration::from_secs(120)).await;
        run1.abort();
        let _ = run1.await;

        let f = ScriptFetcher::new(vec![Box::new(|_p, _e| {
            Ok(ok(vec![issue("9", 9)], None, None))
        })]);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::cli::event_source::EmissionSink::from(tx);
        let s2 = src(&[GithubEventKind::IssueOpened], true);
        let d = dir.clone();
        let run2 = tokio::spawn(async move {
            poll_loop(
                &s2,
                FeedSide {
                    fetcher: &f,
                    login: &FixedLogin("me"),
                },
                ChecksSide {
                    reader: &NoCi,
                    state: Some(d.join("t.ci.json")),
                },
                &tx,
                "k",
                Some(open_log(&d)),
                Default::default(),
            )
            .await;
        });
        let woke = tokio::time::timeout(Duration::from_secs(300), rx.recv())
            .await
            .expect("the event that arrived after an empty baseline is catch-up")
            .expect("open");
        let (WaitItem::GithubEvent { number, .. }, _) = woke else {
            panic!("a github event");
        };
        assert_eq!(number, Some(9));
        run2.abort();
        let _ = run2.await;
    }

    /// The same state, but written by a clank that had no such field:
    /// only the WAL can answer, and a relay row is the feed's row.
    #[tokio::test(start_paused = true)]
    async fn a_legacy_relay_only_log_is_an_established_feed() {
        let dir = wal_tempdir();
        // A relay-delivered issue, emitted and acked, is all the log
        // holds — no feed ids at all.
        {
            let mut log = open_log(&dir);
            let item = WaitItem::GithubEvent {
                presentation: None,
                repo: "o/r".into(),
                event: "issue_opened".into(),
                detail: None,
                number: Some(4),
                title: None,
                actor: None,
                url: None,
                instructions: None,
                content: None,
            };
            log.append_inbox(
                crate::cli::github::log::Transport::Relay,
                None,
                Some("issue_opened#4"),
                None,
                &item,
            )
            .unwrap();
            let seqs: Vec<u64> = log
                .read_rows()
                .unwrap()
                .map(|r| r.rows.iter().map(|row| row.seq).collect())
                .unwrap_or_default();
            for seq in seqs {
                log.append_ack(seq).unwrap();
            }
        }
        // …beside a cache file from before the field existed.
        std::fs::write(
            dir.join("t.state.json"),
            r#"{"etag":"W/"e1"","saved_at":1}"#,
        )
        .unwrap();

        let f = ScriptFetcher::new(vec![Box::new(|_p, _e| {
            Ok(ok(vec![issue("9", 9)], None, None))
        })]);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::cli::event_source::EmissionSink::from(tx);
        let s = src(&[GithubEventKind::IssueOpened], true);
        let d = dir.clone();
        let handle = tokio::spawn(async move {
            poll_loop(
                &s,
                FeedSide {
                    fetcher: &f,
                    login: &FixedLogin("me"),
                },
                ChecksSide {
                    reader: &NoCi,
                    state: Some(d.join("t.ci.json")),
                },
                &tx,
                "k",
                Some(open_log(&d)),
                Default::default(),
            )
            .await;
        });
        let woke = tokio::time::timeout(Duration::from_secs(300), rx.recv())
            .await
            .expect("an upgrade from a relay-only log keeps its catch-up")
            .expect("open");
        let (WaitItem::GithubEvent { number, .. }, _) = woke else {
            panic!("a github event");
        };
        assert_eq!(number, Some(9));
        handle.abort();
        let _ = handle.await;
    }

    /// codex 7c132ec probe 2, as it was run: more in-scope PRs than
    /// the shared key horizon holds. That horizon is a recency window
    /// over EVENTS; a state read re-observes the same conclusions
    /// forever, so the evicted ones came back as new wakes — 1001 of
    /// them at once. The per-PR memory is what makes this quiet, in
    /// one run and across a restart.
    ///
    /// The feed is DOWN throughout, which is the other half of the
    /// claim: every other save happens on a completed feed tick, so
    /// this half has to persist on its own or it remembers nothing
    /// across arms.
    #[tokio::test(start_paused = true)]
    async fn more_prs_than_the_key_horizon_holds_still_wake_once() {
        struct DeadFeed;
        impl EventFetcher for DeadFeed {
            async fn fetch(&self, _page: u32, _etag: Option<String>) -> anyhow::Result<GhResponse> {
                anyhow::bail!("events unreachable")
            }
        }
        let many: Vec<(ci::PrHead, ci::CiRollup)> = (1..=(SEEN_CAP as u64 + 1))
            .map(|n| red(n, &format!("head{n}"), "fmt"))
            .collect();
        let dir = wal_tempdir();
        for arm in 0..2 {
            let ci = ScriptCi::new(vec![many.clone()]);
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let tx = crate::cli::event_source::EmissionSink::from(tx);
            let s = src(
                &[GithubEventKind::IssueOpened, GithubEventKind::CiFailed],
                true,
            );
            let d = dir.clone();
            let handle = tokio::spawn(async move {
                poll_loop(
                    &s,
                    FeedSide {
                        fetcher: &DeadFeed,
                        login: &FixedLogin("me"),
                    },
                    ChecksSide {
                        reader: &ci,
                        state: None,
                    },
                    &tx,
                    "k",
                    Some(open_log(&d)),
                    Default::default(),
                )
                .await;
            });
            tokio::time::sleep(Duration::from_secs(600)).await;
            let woke = rx.try_recv();
            assert!(
                woke.is_err(),
                "arm {arm}: every one of these builds was already red — {woke:?}",
            );
            handle.abort();
            let _ = handle.await;
        }
    }

    /// Retention follows the PRs: one that leaves the scope takes its
    /// memory with it, so the saved state cannot grow forever on a
    /// repo whose PRs come and go.
    #[tokio::test(start_paused = true)]
    async fn a_pr_that_leaves_the_scope_takes_its_memory_with_it() {
        let dir = wal_tempdir();
        let ci = ScriptCi::new(vec![
            vec![red(7, "head7", "fmt"), red(9, "head9", "fmt")],
            vec![red(7, "head7", "fmt")],
        ]);
        let f = ScriptFetcher::new(vec![]);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::cli::event_source::EmissionSink::from(tx);
        let s = src(&[GithubEventKind::CiFailed], true);
        let d = dir.clone();
        let handle = tokio::spawn(async move {
            poll_loop(
                &s,
                FeedSide {
                    fetcher: &f,
                    login: &FixedLogin("me"),
                },
                ChecksSide {
                    reader: &ci,
                    state: Some(d.join("t.ci.json")),
                },
                &tx,
                "k",
                Some(open_log(&d)),
                Default::default(),
            )
            .await;
        });
        tokio::time::sleep(Duration::from_secs(600)).await;
        handle.abort();
        let _ = handle.await;

        let horizon: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("t.ci.json")).unwrap()).unwrap();
        let said = &horizon["said"];
        assert!(said.get("7").is_some(), "the PR still in scope: {horizon}");
        assert!(
            said.get("9").is_none(),
            "the PR that left the scope is forgotten: {horizon}",
        );
        assert!(
            !std::fs::exists(dir.join("t.state.json")).unwrap_or(false)
                || !std::fs::read_to_string(dir.join("t.state.json"))
                    .unwrap()
                    .contains("ci"),
            "and none of it is in the feed's cache",
        );
    }

    /// A read that found nothing to report still establishes the
    /// horizon — otherwise every arm re-baselines and the first real
    /// conclusion is swallowed as "the state of the world".
    #[tokio::test(start_paused = true)]
    async fn a_checks_read_with_nothing_to_report_still_establishes() {
        let dir = wal_tempdir();
        let ci = ScriptCi::new(vec![vec![]]);
        let f = ScriptFetcher::new(vec![]);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::cli::event_source::EmissionSink::from(tx);
        let s = src(&[GithubEventKind::CiFailed], true);
        let d = dir.clone();
        let run1 = tokio::spawn(async move {
            poll_loop(
                &s,
                FeedSide {
                    fetcher: &f,
                    login: &FixedLogin("me"),
                },
                ChecksSide {
                    reader: &ci,
                    state: Some(d.join("t.ci.json")),
                },
                &tx,
                "k",
                Some(open_log(&d)),
                Default::default(),
            )
            .await;
        });
        tokio::time::sleep(Duration::from_secs(120)).await;
        run1.abort();
        let _ = run1.await;

        let ci2 = ScriptCi::new(vec![vec![red(7, "head1", "fmt")]]);
        let f2 = ScriptFetcher::new(vec![]);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::cli::event_source::EmissionSink::from(tx);
        let s2 = src(&[GithubEventKind::CiFailed], true);
        let d = dir.clone();
        let run2 = tokio::spawn(async move {
            poll_loop(
                &s2,
                FeedSide {
                    fetcher: &f2,
                    login: &FixedLogin("me"),
                },
                ChecksSide {
                    reader: &ci2,
                    state: Some(d.join("t.ci.json")),
                },
                &tx,
                "k",
                Some(open_log(&d)),
                Default::default(),
            )
            .await;
        });
        let item = tokio::time::timeout(Duration::from_secs(300), rx.recv())
            .await
            .expect("the first conclusion after an empty read is news")
            .expect("open");
        assert!(matches!(item.0, WaitItem::GithubEvent { .. }));
        run2.abort();
        let _ = run2.await;
    }

    #[tokio::test(start_paused = true)]
    async fn an_unchanged_build_writes_nothing_tick_after_tick() {
        // The CI read re-observes the SAME conclusion every tick. If
        // each one reached the gate it would append an observation
        // record per PR per tick — a log that grows forever over a
        // build that has not changed, and compaction churn behind it.
        let dir = wal_tempdir();
        let ci = ScriptCi::new(vec![vec![red(7, "head1", "fmt")]]);
        let f = ScriptFetcher::new(vec![]);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::cli::event_source::EmissionSink::from(tx);
        let s = src(&[GithubEventKind::CiFailed], true);
        let d = dir.clone();
        let handle = tokio::spawn(async move {
            poll_loop(
                &s,
                FeedSide {
                    fetcher: &f,
                    login: &FixedLogin("me"),
                },
                ChecksSide {
                    reader: &ci,
                    state: Some(d.join("t.ci.json")),
                },
                &tx,
                "k",
                Some(open_log(&d)),
                Default::default(),
            )
            .await;
        });
        // Let many ticks pass (paused clock: instantly).
        tokio::time::sleep(Duration::from_secs(3600)).await;
        handle.abort();
        let _ = handle.await;

        let lines = std::fs::read_to_string(dir.join("t.jsonl"))
            .map(|t| t.lines().filter(|l| !l.trim().is_empty()).count())
            .unwrap_or(0);
        assert_eq!(
            lines, 0,
            "a build nobody has been told about writes nothing at all — what CI \
             remembers is its own file, not a record in the event log",
        );
    }

    #[tokio::test(start_paused = true)]
    async fn does_not_poll_until_the_own_action_filter_is_established() {
        // codex a989653: with include_own_actions=false, a login
        // failure must NOT let the source poll unfiltered. Login fails
        // once (no poll, no emit), then succeeds; only then does the
        // baseline+delta run, and the agent's OWN events stay
        // suppressed.
        let login = ScriptLogin(Mutex::new(
            vec![Err(anyhow::anyhow!("gh down")), Ok("me".to_string())].into(),
        ));
        // Own event (actor "me") + a foreign one; the foreign should be
        // the only thing that ever emits.
        let own = ev(
            "1",
            "IssuesEvent",
            serde_json::json!({"action":"opened","issue":{"number":1}}),
            "me",
        );
        let foreign_baseline = ev(
            "2",
            "IssuesEvent",
            serde_json::json!({"action":"opened","issue":{"number":2}}),
            "them",
        );
        let foreign_new = ev(
            "3",
            "IssuesEvent",
            serde_json::json!({"action":"opened","issue":{"number":3}}),
            "them",
        );
        let f = ScriptFetcher::new(vec![
            // First poll (after login succeeds) is the baseline: own +
            // foreign, then empty page to end pagination.
            Box::new(move |_p, _e| {
                Ok(ok(
                    vec![own.clone(), foreign_baseline.clone()],
                    Some("\"e\""),
                    None,
                ))
            }),
            Box::new(|_p, _e| Ok(ok(vec![], None, None))),
            // Delta: a new foreign event + a seen id to stop.
            Box::new(move |_p, _e| {
                Ok(ok(
                    vec![
                        foreign_new.clone(),
                        ev(
                            "2",
                            "IssuesEvent",
                            serde_json::json!({"action":"opened"}),
                            "them",
                        ),
                    ],
                    None,
                    None,
                ))
            }),
        ]);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::cli::event_source::EmissionSink::from(tx);
        let handle = tokio::spawn(async move {
            poll_loop(
                &src(&[GithubEventKind::IssueOpened], false),
                FeedSide {
                    fetcher: &f,
                    login: &login,
                },
                ChecksSide {
                    reader: &NoCi,
                    state: None,
                },
                &tx,
                "k",
                None,
                Default::default(),
            )
            .await;
        });
        let item = tokio::time::timeout(Duration::from_secs(300), rx.recv())
            .await
            .expect("a foreign event emits once the filter is established")
            .expect("channel open");
        match item.0 {
            // ONLY the foreign new event (id 3) — never the own id 1.
            WaitItem::GithubEvent { number, .. } => assert_eq!(number, Some(3)),
            other => panic!("expected the foreign event, got {other:?}"),
        }
        handle.abort();
        let _ = handle.await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_page_two_failure_still_applies_the_page_one_floor() {
        // codex e9b8b66: page 1 raises the floor to 90s, page 2 FAILS.
        // The degraded tick must still bind the server floor — the next
        // fetch happens no sooner than 90s later (not the 60s default).
        // The failed tick's ETag must NOT condition the retry (it would
        // 304 past the unread pages).
        let f = std::sync::Arc::new(ScriptFetcher::new(vec![
            Box::new(|_p, _e| Ok(ok(vec![issue("1", 1)], Some("\"partial\""), Some(90)))),
            Box::new(|_p, _e| Err(anyhow::anyhow!("gh transport died"))),
            // The retry tick (page 1 again).
            Box::new(|_p, etag| {
                assert!(
                    etag.is_none(),
                    "a failed tick's ETag must not condition the retry"
                );
                Ok(ok(vec![], None, None))
            }),
        ]));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::cli::event_source::EmissionSink::from(tx);
        let f2 = f.clone();
        let handle = tokio::spawn(async move {
            poll_loop(
                &src(&[GithubEventKind::IssueOpened], false),
                FeedSide {
                    fetcher: &*f2,
                    login: &FixedLogin("me"),
                },
                ChecksSide {
                    reader: &NoCi,
                    state: None,
                },
                &tx,
                "k",
                None,
                Default::default(),
            )
            .await;
        });
        // Wait (paused time auto-advances) until the retry fetch lands.
        while f.calls.lock().unwrap().len() < 3 {
            tokio::task::yield_now().await;
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        handle.abort();
        let _ = handle.await;
        let instants = f.instants.lock().unwrap();
        let gap = instants[2] - instants[1];
        assert!(
            gap >= Duration::from_secs(90),
            "the raised floor binds on the degraded tick: retry after {gap:?}, want ≥90s"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_304_floor_update_is_applied_to_the_next_sleep() {
        // codex e9b8b66: a 304 can RAISE X-Poll-Interval; the loop must
        // observe it — the tick after the 304 waits ≥120s.
        let f = std::sync::Arc::new(ScriptFetcher::new(vec![
            // Baseline: empty feed, no floor.
            Box::new(|_p, _e| Ok(ok(vec![], Some("\"e\""), None))),
            // 304 carrying a raised floor.
            Box::new(|_p, _e| {
                Ok(GhResponse::NotModified {
                    poll_interval: Some(120),
                })
            }),
            Box::new(|_p, _e| {
                Ok(GhResponse::NotModified {
                    poll_interval: None,
                })
            }),
        ]));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::cli::event_source::EmissionSink::from(tx);
        let f2 = f.clone();
        let handle = tokio::spawn(async move {
            poll_loop(
                &src(&[GithubEventKind::IssueOpened], false),
                FeedSide {
                    fetcher: &*f2,
                    login: &FixedLogin("me"),
                },
                ChecksSide {
                    reader: &NoCi,
                    state: None,
                },
                &tx,
                "k",
                None,
                Default::default(),
            )
            .await;
        });
        while f.calls.lock().unwrap().len() < 3 {
            tokio::task::yield_now().await;
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        handle.abort();
        let _ = handle.await;
        let instants = f.instants.lock().unwrap();
        let gap = instants[2] - instants[1];
        assert!(
            gap >= Duration::from_secs(120),
            "the 304's raised floor binds: next tick after {gap:?}, want ≥120s"
        );
    }

    #[tokio::test]
    async fn poll_loop_cancels_a_pending_fetch_on_abort() {
        // Supervision (extra-wait-events): aborting the source task
        // must cancel a fetch in flight — the join returns promptly
        // rather than hanging on the pending future.
        struct PendingFetcher;
        impl EventFetcher for PendingFetcher {
            async fn fetch(&self, _p: u32, _e: Option<String>) -> anyhow::Result<GhResponse> {
                std::future::pending::<()>().await; // never resolves
                unreachable!()
            }
        }
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::cli::event_source::EmissionSink::from(tx);
        let handle = tokio::spawn(async move {
            poll_loop(
                &src(&[GithubEventKind::IssueOpened], false),
                FeedSide {
                    fetcher: &PendingFetcher,
                    login: &FixedLogin("me"),
                },
                ChecksSide {
                    reader: &NoCi,
                    state: None,
                },
                &tx,
                "k",
                None,
                Default::default(),
            )
            .await;
        });
        tokio::task::yield_now().await;
        handle.abort();
        let joined = tokio::time::timeout(Duration::from_secs(5), handle).await;
        assert!(
            joined.is_ok(),
            "abort must cancel the pending fetch, not hang"
        );
    }

    /// Cold arm: the baseline writes observation records; a SECOND arm
    /// over the same dir is warm — the event that fired between the
    /// two runs EMITS on the second arm's first fetch instead of being
    /// swallowed by a fresh baseline. The core offline-catch-up
    /// acceptance.
    #[tokio::test(start_paused = true)]
    async fn event_between_runs_is_presented_by_the_second_run() {
        let dir = wal_tempdir();
        // Run 1: baseline sees issue #1, then the wait dies (abort).
        let f = ScriptFetcher::new(vec![Box::new(|_p, _e| {
            Ok(ok(vec![issue("1", 1)], Some("\"e1\""), None))
        })]);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::cli::event_source::EmissionSink::from(tx);
        let s1 = src(&[GithubEventKind::IssueOpened], true);
        let d = dir.clone();
        let run1 = tokio::spawn(async move {
            poll_loop(
                &s1,
                FeedSide {
                    fetcher: &f,
                    login: &FixedLogin("me"),
                },
                ChecksSide {
                    reader: &NoCi,
                    state: Some(d.join("t.ci.json")),
                },
                &tx,
                "k",
                Some(open_log(&d)),
                Default::default(),
            )
            .await;
        });
        tokio::task::yield_now().await;
        assert!(
            rx.try_recv().is_err(),
            "baseline presents nothing (cold arm)"
        );
        run1.abort();
        let _ = run1.await;

        // Between runs: issue #2 fires. Run 2 arms warm and reconciles.
        let f = ScriptFetcher::new(vec![Box::new(|_p, _e| {
            Ok(ok(vec![issue("2", 2), issue("1", 1)], None, None))
        })]);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::cli::event_source::EmissionSink::from(tx);
        let s2 = src(&[GithubEventKind::IssueOpened], true);
        let d = dir.clone();
        let run2 = tokio::spawn(async move {
            poll_loop(
                &s2,
                FeedSide {
                    fetcher: &f,
                    login: &FixedLogin("me"),
                },
                ChecksSide {
                    reader: &NoCi,
                    state: Some(d.join("t.ci.json")),
                },
                &tx,
                "k",
                Some(open_log(&d)),
                Default::default(),
            )
            .await;
        });
        let woke = tokio::time::timeout(std::time::Duration::from_secs(300), rx.recv())
            .await
            .expect("the missed event must wake the second run")
            .expect("open");
        let (WaitItem::GithubEvent { number, .. }, _) = &woke else {
            unreachable!()
        };
        assert_eq!(*number, Some(2), "only the NEW event emits, not history");
        // WAL-first: the emitted event is durably unhandled.
        let loaded = open_log(&dir).load();
        assert_eq!(loaded.unhandled.len(), 1);
        let mut expected = loaded.unhandled[0].item.clone();
        assert!(
            expected.presentation().is_none(),
            "selection is not WAL payload"
        );
        expected.set_presentation(woke.0.presentation().unwrap().clone());
        assert_eq!(
            expected, woke.0,
            "payload preserved with exact selected copy"
        );
        run2.abort();
        let _ = run2.await;
    }

    /// A killed wait between emission and reaction: the unhandled
    /// entry re-presents IMMEDIATELY at the next arm, before any
    /// fetch — and an ack stops the re-presenting.
    #[tokio::test(start_paused = true)]
    async fn unhandled_backlog_represents_at_arm_until_acked() {
        let dir = wal_tempdir();
        {
            let mut log = open_log(&dir);
            log.append_observation(Some("50"), None, true).unwrap();
            log.append_inbox(
                crate::cli::github::log::Transport::Poll,
                Some("51"),
                Some("issue#9"),
                None,
                &classified(9, Some("issue#9")).item,
            )
            .unwrap();
        }
        // The fetcher would block forever — the backlog must arrive
        // WITHOUT any fetch completing.
        let f = ScriptFetcher::new(vec![]);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::cli::event_source::EmissionSink::from(tx);
        let s = src(&[GithubEventKind::IssueOpened], true);
        let d = dir.clone();
        let run = tokio::spawn(async move {
            poll_loop(
                &s,
                FeedSide {
                    fetcher: &f,
                    login: &FixedLogin("me"),
                },
                ChecksSide {
                    reader: &NoCi,
                    state: Some(d.join("t.ci.json")),
                },
                &tx,
                "k",
                Some(open_log(&d)),
                Default::default(),
            )
            .await;
        });
        let woke = tokio::time::timeout(std::time::Duration::from_secs(300), rx.recv())
            .await
            .expect("backlog wakes the arm")
            .expect("open");
        let mut expected = classified(9, Some("issue#9")).item;
        expected.set_presentation(woke.0.presentation().unwrap().clone());
        assert_eq!(woke.0, expected);
        run.abort();
        let _ = run.await;

        // Ack it: the next arm has nothing to re-present.
        let mut log = open_log(&dir);
        let seq = log.load().unhandled[0].seq;
        log.append_ack(seq).unwrap();
        assert!(open_log(&dir).load().unhandled.is_empty());
    }

    /// codex 7c9e7fe: a quarantined WAL with a SURVIVING state.json
    /// must not condition the first fetch — a stale ETag could 304
    /// forever and the baseline would never establish. Cold start =
    /// unconditional fetch, real baseline.
    #[tokio::test(start_paused = true)]
    async fn cold_start_ignores_surviving_etag_and_establishes_baseline() {
        let dir = wal_tempdir();
        {
            // A valid cache from a previous life…
            let log = open_log(&dir);
            log.save_state(Some("\"stale\""), Some(1), true).unwrap();
            // …and a WAL that quarantines (interior corruption).
            std::fs::write(
                dir.join("t.jsonl"),
                "not json\n{\"t\":\"ack\",\"seq\":1,\"at\":1}\n",
            )
            .unwrap();
        }
        // Conditioned request → 304 (the trap); unconditional → Ok.
        // One closure per PAGE fetch: tick 1 = pages 1+2 (boundary),
        // tick 2 = new event page + fully-seen boundary page.
        let f = std::sync::Arc::new(ScriptFetcher::new(vec![
            Box::new(|_p, etag| {
                if etag.is_some() {
                    Ok(GhResponse::NotModified {
                        poll_interval: None,
                    })
                } else {
                    Ok(ok(vec![issue("1", 1)], Some("\"fresh\""), None))
                }
            }),
            Box::new(|_p, _e| Ok(ok(vec![], None, None))),
            // Tick 2 after the baseline: a genuinely new event emits.
            Box::new(|_p, _e| Ok(ok(vec![issue("2", 2)], None, None))),
            Box::new(|_p, _e| Ok(ok(vec![issue("1", 1)], None, None))),
        ]));
        let calls = f.clone();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::cli::event_source::EmissionSink::from(tx);
        let s = src(&[GithubEventKind::IssueOpened], true);
        let d = dir.clone();
        let run = tokio::spawn(async move {
            poll_loop(
                &s,
                FeedSide {
                    fetcher: &*f,
                    login: &FixedLogin("me"),
                },
                ChecksSide {
                    reader: &NoCi,
                    state: Some(d.join("t.ci.json")),
                },
                &tx,
                "k",
                Some(open_log(&d)),
                Default::default(),
            )
            .await;
        });
        let second = tokio::time::timeout(std::time::Duration::from_secs(300), rx.recv())
            .await
            .expect("the post-baseline delta emits")
            .expect("open");
        let (WaitItem::GithubEvent { number, .. }, _) = &second else {
            unreachable!()
        };
        assert_eq!(*number, Some(2));
        // The stated invariant, asserted rather than inferred: the
        // FIRST request after a quarantine carried no ETag at all. A
        // conditional one would 304 against a horizon that no longer
        // exists, and the script advancing past it is what let this
        // test pass while the request was wrong (codex 4a6b328).
        let first_request = calls.calls.lock().unwrap()[0].clone();
        assert_eq!(
            first_request,
            (1, None),
            "page 1, unconditional — the surviving etag belongs to the quarantined log",
        );
        run.abort();
        let _ = run.await;
    }

    /// codex 4a6b328: a quarantined WAL must override the sidecar's
    /// claim that the feed baselined. The claim was written alongside
    /// a horizon that no longer exists, so trusting it replays the
    /// first page as wakes — and the claim has to be invalidated
    /// DURABLY, or a failed fetch and another restart find it again.
    #[tokio::test(start_paused = true)]
    async fn a_quarantined_log_overrides_the_saved_baseline() {
        let dir = wal_tempdir();
        {
            let log = open_log(&dir);
            log.save_state(Some("\"stale\""), Some(1), true).unwrap();
            std::fs::write(
                dir.join("t.jsonl"),
                "not json\n{\"t\":\"ack\",\"seq\":1,\"at\":1}\n",
            )
            .unwrap();
        }
        // Every fetch FAILS, so no tick ever completes and nothing
        // is saved by the normal path. Then the process dies. This is
        // the window the quarantine's own write closes: a scripted
        // fetcher would fall through to NotModified, complete a tick
        // and save the flag as a side effect, hiding the gap.
        struct DeadFeed;
        impl EventFetcher for DeadFeed {
            async fn fetch(&self, _page: u32, _etag: Option<String>) -> anyhow::Result<GhResponse> {
                anyhow::bail!("network")
            }
        }
        let f = DeadFeed;
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::cli::event_source::EmissionSink::from(tx);
        let s1 = src(&[GithubEventKind::IssueOpened], true);
        let d = dir.clone();
        let run1 = tokio::spawn(async move {
            poll_loop(
                &s1,
                FeedSide {
                    fetcher: &f,
                    login: &FixedLogin("me"),
                },
                ChecksSide {
                    reader: &NoCi,
                    state: Some(d.join("t.ci.json")),
                },
                &tx,
                "k",
                Some(open_log(&d)),
                Default::default(),
            )
            .await;
        });
        tokio::time::sleep(Duration::from_secs(120)).await;
        run1.abort();
        let _ = run1.await;

        // Second arm: the log is gone (quarantined), so an old issue
        // on the first fetch is HISTORY.
        // Two closures: the page with the old issue, then an empty
        // one so pagination ENDS normally — a tick that never
        // completes would pass this test for the wrong reason.
        let f = std::sync::Arc::new(ScriptFetcher::new(vec![
            Box::new(|_p, _e| Ok(ok(vec![issue("1", 1)], None, None))),
            Box::new(|_p, _e| Ok(ok(vec![], None, None))),
        ]));
        let calls = f.clone();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::cli::event_source::EmissionSink::from(tx);
        let s2 = src(&[GithubEventKind::IssueOpened], true);
        let d = dir.clone();
        let run2 = tokio::spawn(async move {
            poll_loop(
                &s2,
                FeedSide {
                    fetcher: &*f,
                    login: &FixedLogin("me"),
                },
                ChecksSide {
                    reader: &NoCi,
                    state: Some(d.join("t.ci.json")),
                },
                &tx,
                "k",
                Some(open_log(&d)),
                Default::default(),
            )
            .await;
        });
        tokio::time::sleep(Duration::from_secs(600)).await;
        let leaked = rx.try_recv();
        assert!(
            leaked.is_err(),
            "the first page after a quarantine is the baseline — {leaked:?}",
        );
        assert_eq!(
            calls.calls.lock().unwrap()[0].1,
            None,
            "and it was asked for unconditionally",
        );
        run2.abort();
        let _ = run2.await;
    }

    /// codex 7c9e7fe: pagination shift repeating a PRESENTABLE event
    /// across pages within one tick must produce ONE emission and ONE
    /// primary WAL record.
    #[tokio::test(start_paused = true)]
    async fn repeated_presentable_id_across_pages_emits_and_records_once() {
        let dir = wal_tempdir();
        // One closure per PAGE fetch: baseline pages, then a delta
        // tick whose pagination SHIFTS issue #2 (id 20) onto page 2 as
        // well — the same presentable event observed twice in one tick.
        let f = ScriptFetcher::new(vec![
            Box::new(|_p, _e| Ok(ok(vec![issue("1", 1)], Some("\"e\""), None))),
            Box::new(|_p, _e| Ok(ok(vec![], None, None))),
            Box::new(|_p, _e| Ok(ok(vec![issue("20", 2)], None, None))),
            Box::new(|_p, _e| Ok(ok(vec![issue("20", 2), issue("1", 1)], None, None))),
            Box::new(|_p, _e| Ok(ok(vec![], None, None))),
        ]);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::cli::event_source::EmissionSink::from(tx);
        let s = src(&[GithubEventKind::IssueOpened], true);
        let d = dir.clone();
        let run = tokio::spawn(async move {
            poll_loop(
                &s,
                FeedSide {
                    fetcher: &f,
                    login: &FixedLogin("me"),
                },
                ChecksSide {
                    reader: &NoCi,
                    state: Some(d.join("t.ci.json")),
                },
                &tx,
                "k",
                Some(open_log(&d)),
                Default::default(),
            )
            .await;
        });
        let woke = tokio::time::timeout(std::time::Duration::from_secs(300), rx.recv())
            .await
            .expect("the new event emits once")
            .expect("open");
        let (WaitItem::GithubEvent { number, .. }, _) = &woke else {
            unreachable!()
        };
        assert_eq!(*number, Some(2));
        // No second emission from the page-2 copy.
        tokio::time::sleep(std::time::Duration::from_secs(120)).await;
        assert!(rx.try_recv().is_err(), "page-repeated copy double-emitted");
        run.abort();
        let _ = run.await;
        // Exactly one primary record for feed id 20: it's unhandled
        // (inbox) and appears once in the cursor.
        let loaded = open_log(&dir).load();
        assert_eq!(loaded.unhandled.len(), 1);
        assert_eq!(
            loaded.feed_ids.iter().filter(|id| *id == "20").count(),
            1,
            "one primary record, one cursor entry"
        );
    }

    // ── the polling cadence ──

    type Rollups = Vec<(ci::PrHead, ci::CiRollup)>;

    /// A CI reader that answers from a script, then the last answer,
    /// and counts its reads.
    #[derive(Default)]
    struct TestCi {
        answers: Mutex<std::collections::VecDeque<Result<Rollups, &'static str>>>,
        reads: Mutex<Vec<tokio::time::Instant>>,
    }

    impl TestCi {
        fn answering(answers: Vec<Result<Rollups, &'static str>>) -> std::sync::Arc<Self> {
            std::sync::Arc::new(Self {
                answers: Mutex::new(answers.into()),
                ..Default::default()
            })
        }

        fn reads(&self) -> usize {
            self.reads.lock().unwrap().len()
        }
    }

    impl CiReader for TestCi {
        async fn read(&self) -> anyhow::Result<Rollups> {
            self.reads.lock().unwrap().push(tokio::time::Instant::now());
            let mut answers = self.answers.lock().unwrap();
            let next = match answers.len() {
                0 => Ok(Vec::new()),
                1 => answers.front().cloned().unwrap(),
                _ => answers.pop_front().unwrap(),
            };
            next.map_err(|e| anyhow::anyhow!(e))
        }
    }

    /// How many feed READS happened: each starts with page 1.
    fn feed_reads(f: &ScriptFetcher) -> usize {
        f.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(p, _)| *p == 1)
            .count()
    }

    fn feed_issue(id: &str, n: u64) -> serde_json::Value {
        serde_json::json!({"id":id,"type":"IssuesEvent","actor":{"login":"them"},
            "payload":{"action":"opened","issue":{"number":n}}})
    }

    fn woke_numbers(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<crate::cli::event_source::Emission>,
    ) -> Vec<u64> {
        let mut out = Vec::new();
        while let Ok((WaitItem::GithubEvent { number, .. }, _)) = rx.try_recv() {
            out.extend(number);
        }
        out
    }

    fn spawn_watch<
        L: LoginResolver + Send + Sync + 'static,
        C: CiReader + Send + Sync + 'static,
    >(
        s: GithubSource,
        fetcher: std::sync::Arc<ScriptFetcher>,
        login: std::sync::Arc<L>,
        reader: std::sync::Arc<C>,
        log: Option<crate::cli::github::log::EventLog>,
        tx: crate::cli::event_source::EmissionSink,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            poll_loop(
                &s,
                FeedSide {
                    fetcher: &*fetcher,
                    login: &*login,
                },
                ChecksSide {
                    reader: &*reader,
                    state: None,
                },
                &tx,
                "k",
                log,
                Default::default(),
            )
            .await;
        })
    }

    fn sink() -> (
        crate::cli::event_source::EmissionSink,
        tokio::sync::mpsc::UnboundedReceiver<crate::cli::event_source::Emission>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (tx.into(), rx)
    }

    type Heard = tokio::sync::mpsc::UnboundedReceiver<crate::cli::event_source::LifecycleReport>;
    type Emitted = tokio::sync::mpsc::UnboundedReceiver<(
        clank_core::AgentLabel,
        crate::cli::event_source::Emission,
    )>;

    /// A sink that also hears the watch's lifecycle reports.
    fn reporting_sink() -> (crate::cli::event_source::EmissionSink, Emitted, Heard) {
        let (tx, emitted) = tokio::sync::mpsc::unbounded_channel();
        let (reports, heard) = tokio::sync::mpsc::unbounded_channel();
        let author = clank_core::AgentLabel::parse("claude").unwrap();
        (
            crate::cli::event_source::EmissionSink::for_author(tx, author.clone())
                .reporting(reports, author, 1),
            emitted,
            heard,
        )
    }

    fn records(heard: &mut Heard) -> Vec<crate::cli::github::progress::PollRecord> {
        let mut out = Vec::new();
        while let Ok((_, key, _, state)) = heard.try_recv() {
            assert_eq!(key, "k");
            match state {
                crate::cli::event_source::Lifecycle::Polling(r) => out.push(r),
                other => panic!("a github watch reports its polling, not {other:?}"),
            }
        }
        out
    }

    async fn settle() {
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
    }

    const DAY: Duration = Duration::from_secs(24 * 3600);

    /// A watch with no `poll_interval` polls both halves at once and
    /// then every 60s: nothing else exists to tell it to read.
    #[tokio::test(start_paused = true)]
    async fn a_hosted_watch_polls_both_halves_every_interval() {
        let f = std::sync::Arc::new(ScriptFetcher::new(vec![]));
        let ci = TestCi::answering(vec![]);
        let (tx, _rx) = sink();
        let s = src(
            &[GithubEventKind::IssueOpened, GithubEventKind::CiFailed],
            false,
        );
        assert_eq!(s.poll_interval, None);
        let run = spawn_watch(
            s,
            f.clone(),
            std::sync::Arc::new(FixedLogin("me")),
            ci.clone(),
            None,
            tx,
        );
        settle().await;
        assert_eq!((feed_reads(&f), ci.reads()), (1, 1), "both, at once");
        tokio::time::sleep(Duration::from_secs(59)).await;
        assert_eq!((feed_reads(&f), ci.reads()), (1, 1), "not before 60s");
        tokio::time::sleep(Duration::from_secs(3 * 60 - 59) + Duration::from_millis(1)).await;
        assert_eq!((feed_reads(&f), ci.reads()), (4, 4));
        run.abort();
    }

    /// `poll_interval` sets the cadence; GitHub's `X-Poll-Interval`
    /// lengthens it.
    #[tokio::test(start_paused = true)]
    async fn the_interval_is_the_longer_of_the_config_and_the_server() {
        let f = std::sync::Arc::new(ScriptFetcher::new(vec![Box::new(|_p, _e| {
            Ok(ok(vec![], Some("\"e1\""), Some(300)))
        })]));
        let (tx, _rx) = sink();
        let mut s = src(&[GithubEventKind::IssueOpened], false);
        s.poll_interval = Some("2m".into());
        let run = spawn_watch(
            s,
            f.clone(),
            std::sync::Arc::new(FixedLogin("me")),
            std::sync::Arc::new(NoCi),
            None,
            tx,
        );
        settle().await;
        tokio::time::sleep(Duration::from_secs(299)).await;
        assert_eq!(feed_reads(&f), 1, "the server's floor binds");
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(feed_reads(&f), 2);
        run.abort();
    }

    /// A 304 is a poll that found nothing: the ETag that earned it
    /// conditions every read after it.
    #[tokio::test(start_paused = true)]
    async fn a_304_keeps_the_etag() {
        let f = std::sync::Arc::new(ScriptFetcher::new(vec![Box::new(|_p, _e| {
            Ok(ok(vec![], Some("\"e1\""), None))
        })]));
        let (tx, _emitted, mut heard) = reporting_sink();
        let run = spawn_watch(
            src(&[GithubEventKind::IssueOpened], false),
            f.clone(),
            std::sync::Arc::new(FixedLogin("me")),
            std::sync::Arc::new(NoCi),
            None,
            tx,
        );
        tokio::time::sleep(Duration::from_secs(3 * 60) + Duration::from_millis(1)).await;
        let etags: Vec<Option<String>> = f
            .calls
            .lock()
            .unwrap()
            .iter()
            .map(|(_, e)| e.clone())
            .collect();
        assert_eq!(
            etags,
            [
                None,
                Some("\"e1\"".into()),
                Some("\"e1\"".into()),
                Some("\"e1\"".into())
            ]
        );
        let last = records(&mut heard).pop().expect("reported");
        assert!(last.errors.is_empty(), "a 304 is a completed poll");
        assert!(last.checked.is_some());
        run.abort();
    }

    /// A failed read is reported with its reason and backs off, the
    /// timer keeps running, and the next success clears the report.
    #[tokio::test(start_paused = true)]
    async fn an_error_backs_off_is_reported_and_polling_resumes() {
        let f = std::sync::Arc::new(ScriptFetcher::new(vec![
            Box::new(|_p, _e| Err(anyhow::anyhow!("github /events: 502"))),
            Box::new(|_p, _e| Err(anyhow::anyhow!("github /events: 502"))),
            Box::new(|_p, _e| Ok(ok(vec![], None, None))),
        ]));
        let (tx, _emitted, mut heard) = reporting_sink();
        let run = spawn_watch(
            src(&[GithubEventKind::IssueOpened], false),
            f.clone(),
            std::sync::Arc::new(FixedLogin("me")),
            std::sync::Arc::new(NoCi),
            None,
            tx,
        );
        settle().await;
        let first = records(&mut heard);
        let failed = first.last().expect("reported");
        let error = failed.errors.first().expect("the failure is reported");
        assert!(error.why.contains("502"), "{}", error.why);
        assert_eq!(failed.backoff, 5);
        assert_eq!(
            error.retry_at,
            failed.since + 5,
            "the retry is when it says"
        );
        tokio::time::sleep(Duration::from_secs(5)).await;
        settle().await;
        let second = records(&mut heard);
        let again = second.last().expect("reported");
        assert_eq!(again.backoff, 10, "backing off");
        assert_eq!(
            again.errors.first().map(|e| e.since),
            Some(error.since),
            "still failing since the first"
        );
        tokio::time::sleep(Duration::from_secs(10)).await;
        settle().await;
        let healed = records(&mut heard).pop().expect("reported");
        assert_eq!((healed.errors, healed.backoff), (vec![], 0));
        assert_eq!(feed_reads(&f), 3);
        tokio::time::sleep(Duration::from_secs(60) + Duration::from_millis(1)).await;
        assert_eq!(feed_reads(&f), 4, "and polling resumes at its interval");
        run.abort();
    }

    /// No answer stops a hosted watch: a feed that fails forever is
    /// still read, at most a backoff cap apart.
    #[tokio::test(start_paused = true)]
    async fn polling_never_stops_on_error() {
        struct Broken(std::sync::Arc<std::sync::atomic::AtomicUsize>);
        impl EventFetcher for Broken {
            async fn fetch(&self, _p: u32, _e: Option<String>) -> anyhow::Result<GhResponse> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                anyhow::bail!("unreachable")
            }
        }
        let fetches: std::sync::Arc<std::sync::atomic::AtomicUsize> = Default::default();
        let f = Broken(fetches.clone());
        let ci = TestCi::answering(vec![Err("checks unreachable")]);
        let (tx, _rx) = sink();
        let s = src(
            &[GithubEventKind::IssueOpened, GithubEventKind::CiFailed],
            false,
        );
        let reader = ci.clone();
        let run = tokio::spawn(async move {
            poll_loop(
                &s,
                FeedSide {
                    fetcher: &f,
                    login: &FixedLogin("me"),
                },
                ChecksSide {
                    reader: &*reader,
                    state: None,
                },
                &tx,
                "k",
                None,
                Default::default(),
            )
            .await;
        });
        tokio::time::sleep(DAY).await;
        let (feed, checks) = (
            fetches.load(std::sync::atomic::Ordering::SeqCst),
            ci.reads(),
        );
        run.abort();
        let per_day = (DAY.as_secs() / RETRY_CAP.as_secs()) as usize;
        assert!(feed >= per_day, "the feed read {feed} times in a day");
        assert!(checks >= per_day, "the checks read {checks} times in a day");
    }

    /// A config from when watches chose a delivery loads and polls.
    #[tokio::test(start_paused = true)]
    async fn an_old_realtime_config_polls() {
        let clank_core::agent_config::WaitEventSource::Github(s) = serde_json::from_str(
            r#"{"kind":"github","repo":"o/r","events":["issue_opened"],"delivery":"realtime"}"#,
        )
        .expect("an old config loads") else {
            panic!("github");
        };
        let f = std::sync::Arc::new(ScriptFetcher::new(vec![]));
        let (tx, _rx) = sink();
        let run = spawn_watch(
            s,
            f.clone(),
            std::sync::Arc::new(FixedLogin("me")),
            std::sync::Arc::new(NoCi),
            None,
            tx,
        );
        tokio::time::sleep(Duration::from_secs(60) + Duration::from_millis(1)).await;
        assert_eq!(feed_reads(&f), 2);
        run.abort();
    }

    /// Warm start: what the log does not hold is pending.
    #[tokio::test(start_paused = true)]
    async fn a_warm_catch_up_presents_what_the_log_lacks() {
        let dir = wal_tempdir();
        {
            let mut log = open_log(&dir);
            log.append_observation(Some("1"), None, true).unwrap();
            log.save_state(None, None, true).unwrap();
        }
        let f = std::sync::Arc::new(ScriptFetcher::new(vec![Box::new(|_p, _e| {
            Ok(ok(vec![feed_issue("2", 2), feed_issue("1", 1)], None, None))
        })]));
        let (tx, mut rx) = sink();
        let run = spawn_watch(
            src(&[GithubEventKind::IssueOpened], false),
            f,
            std::sync::Arc::new(FixedLogin("me")),
            std::sync::Arc::new(NoCi),
            Some(open_log(&dir)),
            tx,
        );
        settle().await;
        assert_eq!(woke_numbers(&mut rx), vec![2]);
        run.abort();
    }

    /// One read is one logical reconcile: a failed attempt — the
    /// login, the network, or half a page — is retried on its backoff,
    /// re-reads from the committed cursor, and once it completes the
    /// next read waits for the interval.
    #[tokio::test(start_paused = true)]
    async fn a_failed_read_is_retried_until_it_completes() {
        let dir = wal_tempdir();
        {
            let mut log = open_log(&dir);
            log.append_observation(Some("1"), None, true).unwrap();
            log.save_state(None, None, true).unwrap();
        }
        let f = std::sync::Arc::new(ScriptFetcher::new(vec![
            Box::new(|_p, _e| Err(anyhow::anyhow!("network unreachable"))),
            Box::new(|_p, _e| Ok(ok(vec![feed_issue("3", 3)], Some("\"half\""), None))),
            Box::new(|_p, _e| Err(anyhow::anyhow!("page 2 failed"))),
            Box::new(|_p, etag| {
                assert!(
                    etag.is_none(),
                    "a half read's etag must not condition the retry"
                );
                Ok(ok(vec![feed_issue("3", 3), feed_issue("2", 2)], None, None))
            }),
            Box::new(|_p, _e| Ok(ok(vec![feed_issue("1", 1)], None, None))),
        ]));
        let login = std::sync::Arc::new(ScriptLogin(Mutex::new(
            vec![Err(anyhow::anyhow!("gh down")), Ok("me".to_string())].into(),
        )));
        let (tx, mut rx) = sink();
        let run = spawn_watch(
            src(&[GithubEventKind::IssueOpened], false),
            f.clone(),
            login,
            std::sync::Arc::new(NoCi),
            Some(open_log(&dir)),
            tx,
        );
        // Backoffs of 5, 10 and 20s: all three retries land inside the
        // first interval.
        tokio::time::sleep(Duration::from_secs(40)).await;
        let mut woke = woke_numbers(&mut rx);
        woke.sort();
        assert_eq!(woke, vec![2, 3], "each once, through the committed cursor");
        let reads = feed_reads(&f);
        assert_eq!(
            reads, 3,
            "login, network and the half page each cost one retry"
        );
        let done = *f.instants.lock().unwrap().last().unwrap();
        tokio::time::sleep(Duration::from_secs(60)).await;
        let instants = f.instants.lock().unwrap().clone();
        let after: Vec<_> = instants.iter().filter(|i| **i > done).collect();
        assert_eq!(after.len(), 1, "success ends the retrying");
        assert_eq!(
            *after[0] - done,
            Duration::from_secs(60),
            "the next read is the interval's"
        );
        run.abort();
    }

    /// A rate limit's reset is honoured: the retry waits it out rather
    /// than backing off on its own, shorter, schedule, and the source
    /// says it is rate limited meanwhile.
    #[tokio::test(start_paused = true)]
    async fn a_rate_limited_read_waits_for_the_reset() {
        let f = std::sync::Arc::new(ScriptFetcher::new(vec![Box::new(|_p, _e| {
            Err(crate::cli::github::session::RateLimited {
                url: "https://api.github.com/repos/o/r/events".into(),
                retry_in: Duration::from_secs(600),
            }
            .into())
        })]));
        let (tx, _emitted, mut heard) = reporting_sink();
        let run = spawn_watch(
            src(&[GithubEventKind::IssueOpened], false),
            f.clone(),
            std::sync::Arc::new(FixedLogin("me")),
            std::sync::Arc::new(NoCi),
            None,
            tx,
        );
        settle().await;
        let limited = records(&mut heard).pop().expect("reported");
        let [e] = &limited.errors[..] else {
            panic!("one failing half: {:?}", limited.errors)
        };
        assert!(e.rate_limited);
        assert_eq!(limited.backoff, 600);
        assert_eq!(
            e.retry_at,
            limited.since + 600,
            "the published retry is the reset, not the 60s interval"
        );
        tokio::time::sleep(Duration::from_secs(61)).await;
        settle().await;
        assert_eq!(feed_reads(&f), 1, "the interval passing is not a retry");
        assert!(
            records(&mut heard).is_empty(),
            "and nothing republished a shorter retry"
        );
        tokio::time::sleep(Duration::from_secs(538)).await;
        assert_eq!(feed_reads(&f), 1, "not before the reset");
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(feed_reads(&f), 2);
        run.abort();
    }

    /// A failing feed's retries are its own: the checks keep their
    /// cadence from their own last read, and while the feed waits out a
    /// rate limit the healthy checks poll many times over, the record
    /// still naming the feed's retry, not the checks' next read.
    #[tokio::test(start_paused = true)]
    async fn a_failing_half_never_moves_the_other_halfs_cadence() {
        let f = std::sync::Arc::new(ScriptFetcher::new(vec![
            Box::new(|_p, _e| Err(anyhow::anyhow!("502"))),
            Box::new(|_p, _e| Err(anyhow::anyhow!("502"))),
            Box::new(|_p, _e| Err(anyhow::anyhow!("502"))),
            Box::new(|_p, _e| Ok(ok(vec![], None, None))),
            Box::new(|_p, _e| {
                Err(crate::cli::github::session::RateLimited {
                    url: "https://api.github.com/repos/o/r/events".into(),
                    retry_in: Duration::from_secs(600),
                }
                .into())
            }),
        ]));
        let ci = TestCi::answering(vec![]);
        let (tx, _emitted, mut heard) = reporting_sink();
        let start = tokio::time::Instant::now();
        let run = spawn_watch(
            src(
                &[GithubEventKind::IssueOpened, GithubEventKind::CiFailed],
                false,
            ),
            f.clone(),
            std::sync::Arc::new(FixedLogin("me")),
            ci.clone(),
            None,
            tx,
        );
        // Feed: fails at 0, 5, 15; succeeds at 35; rate limited at 95.
        tokio::time::sleep(Duration::from_secs(4 * 60) + Duration::from_millis(1)).await;
        let feed_at: Vec<u64> = f
            .instants
            .lock()
            .unwrap()
            .iter()
            .map(|i| (*i - start).as_secs())
            .collect();
        assert_eq!(feed_at, [0, 5, 15, 35, 95]);
        let checks_at: Vec<u64> = ci
            .reads
            .lock()
            .unwrap()
            .iter()
            .map(|i| (*i - start).as_secs())
            .collect();
        assert_eq!(
            checks_at,
            [0, 60, 120, 180, 240],
            "the checks' own cadence, through the feed's retries and its rate limit"
        );
        let last = records(&mut heard).pop().expect("reported");
        let [e] = &last.errors[..] else {
            panic!("the feed alone is failing: {:?}", last.errors)
        };
        assert!(e.why.starts_with("feed read failed") && e.rate_limited);
        assert_eq!(e.retry_at, last.since + 95 + 600, "the feed's own retry");
        run.abort();
    }
}
