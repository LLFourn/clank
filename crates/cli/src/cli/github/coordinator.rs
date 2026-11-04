//! The emission gate, and the watch loop it guards.
//!
//! Two halves reach an agent through one door, and they are not the
//! same kind of thing: the feed is a stream of id-bearing events that
//! arrive once, the checks are a state re-read on demand.
//! [`Coordinator`] is the single place that decides whether a wake is
//! news — one dedup horizon over action keys, one gate the relay
//! waits behind — so neither half can emit behind the other's back.
//!
//! [`poll_loop`] is the watch. It is event driven: it reads GitHub
//! when its transport starts listening (one catch-up), when a webhook
//! cue says the checks may have moved, when unfinished work is due for
//! a retry, and — only if `poll_interval` is set — on that timer. A
//! healthy idle watch with no timer reads nothing.

use clank_core::agent_config::GithubSource;

use super::ci::CiReader;
use super::feed::{
    Classified, EventFetcher, FeedHorizon, MAX_PAGES, PollDelta, SEEN_CAP, SeenSet,
    classify_webhook, effective_interval, poll_once, row_in_scope,
};
use super::relay::{Heard, RELAY_BUFFER_CAP, RelayInbox};
use super::selection::Selection;
use super::session::LoginResolver;

/// Webhook events that mean "this PR's checks may have moved".
///
/// They are a CUE, never a conclusion: one App's suite is not the
/// PR's result, so what arrives is used to decide WHEN to re-read,
/// and the read is the same one the catch-up does. That is what keeps
/// a cue-driven read and a timer's read from ever disagreeing.
pub(super) const CHECK_CUES: &[&str] = &["check_suite", "status"];

/// The webhook that carries a PR's head moving — a build nobody has
/// read yet. It is also an event in its own right for the PR kinds.
pub(super) const HEAD_CHANGE: &str = "pull_request";

pub(super) fn is_head_change(event_name: &str, payload: &serde_json::Value) -> bool {
    event_name == HEAD_CHANGE
        && matches!(
            payload.get("action").and_then(|a| a.as_str()),
            Some("opened" | "reopened" | "synchronize")
        )
}

/// How close together cues may make the checks be re-read.
///
/// Without it a busy PR — several apps, several pushes — turns every
/// suite completion into a round of requests, and an unscoped watch
/// pays that for every open PR at once. A cue inside the window is
/// never dropped: with no timer behind it, a dropped cue is a lost
/// conclusion. It schedules ONE trailing read at the window's end,
/// which sees the state after the last cue.
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

/// The per-source EMISSION gate — one owner for what reaches the wait
/// loop across both delivery paths (the coordinator model, codex
/// 1ddc0f6/afb7a35). First arrival with a given action key emits; the
/// other path's copy drops at the gate. A KEYLESS wake (missing
/// payload identity) is never deduped — emit-always is the safe
/// degradation. The feed BASELINE owns the horizon: it records
/// pre-watch history and emits nothing, and relay deliveries BUFFER
/// (bounded) until a baseline has succeeded, then drain through the
/// gate.
///
/// A relay delivery is always LIVE — the hook it came through was
/// created after the watch began listening — so history never
/// suppresses it: an action the baseline recorded that the webhook
/// also delivers wakes once (codex ac94279).
pub(crate) struct Coordinator {
    interrupt: crate::cli::github::log::Interrupt,
    keys: SeenSet,
    /// Keys this run's baseline recorded as history that no live path
    /// has claimed yet.
    history: std::collections::HashSet<String>,
    baseline_done: bool,
    buffered: std::collections::VecDeque<Classified>,
    /// Deliveries the pre-baseline buffer evicted, not yet owed back.
    evicted: u64,
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
            history: Default::default(),
            baseline_done: false,
            buffered: std::collections::VecDeque::new(),
            evicted: 0,
            tx,
            wal,
        }
    }

    /// Restore the durable action-key horizon. Dedup only — this says
    /// nothing about whether any source has baselined.
    pub(crate) fn remember_keys(&mut self, keys: impl IntoIterator<Item = String>) {
        self.keys.extend(keys);
    }

    /// The FEED has a horizon, so relay deliveries need no baseline
    /// buffering: a replay of a pre-restart action drops at the gate
    /// instead of double-emitting (github-offline-catchup).
    ///
    /// Separate from [`remember_keys`](Self::remember_keys) because
    /// this gate is the feed's alone. Opening it because SOME record
    /// existed let a checks-only log release a queued webhook before
    /// any feed baseline had run (codex ae48e92).
    pub(crate) fn feed_established(&mut self) {
        self.baseline_done = true;
    }

    /// The one emission gate for BOTH paths: decides, writes the
    /// event's single atomic WAL record (inbox when emitting, obs when
    /// the other path already emitted the action), THEN emits. Returns
    /// false when the wait side is gone (the caller should stop).
    fn gate(&mut self, mut c: Classified, transport: crate::cli::github::log::Transport) -> bool {
        // Keyless wakes skip the dedup entirely: never dedup.
        if let Some(k) = &c.key {
            let live_over_history =
                transport == crate::cli::github::log::Transport::Relay && self.history.remove(k);
            if self.keys.contains(k) && !live_over_history {
                // The other path emitted this action: this copy's
                // durable decision is an observation.
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
            transport,
            c.feed_id.as_deref(),
            c.key.as_deref(),
            c.event_at,
            &mut c.item,
            self.interrupt.clone(),
        );
        self.tx.send((c.item, durability)).is_ok()
    }

    /// A classified wake from the POLL path.
    pub(crate) fn offer(&mut self, c: Classified) -> bool {
        self.gate(c, crate::cli::github::log::Transport::Poll)
    }

    /// The BASELINE read's actions. The relay backlog drains through
    /// the gate FIRST, so a live delivery is never pre-seeded as
    /// handled; then the rest are recorded as history — born-handled
    /// baseline records keeping their payload so the timeline can show
    /// pre-watch history (github-watch-resilience) — and emit nothing.
    pub(crate) fn baseline(&mut self, baseline: Vec<Classified>) -> bool {
        self.baseline_done = true;
        while let Some(c) = self.buffered.pop_front() {
            if !self.gate(c, crate::cli::github::log::Transport::Relay) {
                return false;
            }
        }
        let history: Vec<Classified> = baseline
            .into_iter()
            .filter(|c| c.key.as_deref().is_none_or(|k| !self.keys.contains(k)))
            .collect();
        self.history
            .extend(history.iter().filter_map(|c| c.key.clone()));
        self.seed(history);
        true
    }

    /// Record actions as already-known without emitting them: their
    /// keys join the gate and their payloads land as born-handled
    /// baseline records.
    ///
    /// Separate from [`baseline`](Self::baseline) because a source
    /// has more than one horizon to establish. The feed's baseline
    /// also opens the relay gate; a CI baseline establishes only
    /// itself, since nothing buffers on it (a-red-build-is-a-wake).
    pub(crate) fn seed(&mut self, actions: Vec<Classified>) {
        for c in actions {
            self.wal
                .baseline_inbox(c.feed_id.as_deref(), c.key.as_deref(), c.event_at, &c.item);
            self.keys.extend(c.key);
        }
    }

    /// A relay delivery: buffered until the baseline succeeds, gated
    /// afterward. The buffer is bounded; what it evicts is owed back
    /// as a reconcile through [`take_evicted`](Self::take_evicted).
    pub(crate) fn deliver(&mut self, c: Classified) -> bool {
        if !self.baseline_done {
            if self.buffered.len() == RELAY_BUFFER_CAP {
                self.buffered.pop_front();
                self.evicted += 1;
            }
            self.buffered.push_back(c);
            return true;
        }
        self.gate(c, crate::cli::github::log::Transport::Relay)
    }

    pub(crate) fn take_evicted(&mut self) -> u64 {
        std::mem::take(&mut self.evicted)
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
            } else if !coordinator.baseline(items) {
                return Ok(false);
            }
            feed.established = true;
            wal.save_state(feed.etag.as_deref(), feed.floor, feed.established);
            wal.compact();
        }
    }
    Ok(true)
}

/// The watch loop: present the warm backlog, then act on what the
/// transport says and on what is owed.
///
/// - Each [`Heard::CatchUp`] is a new start of listening and owes one
///   catch-up: a feed reconcile (the first one ever is the baseline)
///   and, for a CI watch, one checks read of every PR in scope. What
///   the feed still lags on at that moment is not in it, and the new
///   hook will not deliver it either — the stated gap only
///   `poll_interval` closes.
/// - A delivery classifies and emits through the coordinator gate; a
///   check cue, or a PR head change, owes a checks read instead
///   (coalesced inside [`CI_RECHECK_FLOOR`]).
/// - Intake overflow owes a reconcile.
/// - Owed work is retried with backoff until it succeeds.
/// - With `poll_interval` set, that timer owes both halves too.
///
/// The own-action filter is fail-closed: until the login resolves,
/// the feed is not read and deliveries wait at ingress. Generic over
/// the fetcher, login resolver and CI reader, so tested end-to-end
/// without `gh`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn poll_loop<F: EventFetcher, R: LoginResolver, C: CiReader>(
    src: &GithubSource,
    feed_side: FeedSide<'_, F, R>,
    checks: ChecksSide<'_, C>,
    tx: &crate::cli::event_source::EmissionSink,
    relay: &RelayInbox,
    key: &str,
    mut log: Option<crate::cli::github::log::EventLog>,
    suppress_backlog: std::collections::HashSet<(u64, String)>,
) {
    let sel = Selection::of(src);
    let wants_ci = sel.wants_ci();
    let poll_every = src
        .poll_interval
        .as_deref()
        .and_then(|s| crate::cli::wait::parse_duration_str(s).ok().flatten());
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
    // The key horizon covers BOTH halves — it is what stops a relay
    // delivery and a poll of the same action emitting twice — so it
    // is restored from any valid log. Opening the relay's gate is a
    // statement about the FEED alone, and a log holding only checks
    // rows is no evidence for it (codex ae48e92).
    coordinator.remember_keys(warm_keys);
    if feed.horizon.established {
        coordinator.feed_established();
    }

    let mut feed_work = Owed::default();
    let mut checks_work = Owed::default();
    // PERMISSION TO LOOK AGAIN, advanced by every attempt whatever it
    // returns: advancing it only on success would let a broken checks
    // endpoint be hit on every cue, which is when a storm is least
    // affordable.
    let mut checks_not_before: Option<tokio::time::Instant> = None;
    let next_tick = |floor: Option<u64>| {
        poll_every.map(|every| tokio::time::Instant::now() + effective_interval(every, floor))
    };
    let mut timer_due = next_tick(feed.horizon.floor);

    // Whatever the lost deliveries were, the reads that would have
    // followed them are owed: the feed's, and — since a lost delivery
    // may have moved a PR — the checks'.
    let overflowed = |dropped: u64, feed_work: &mut Owed, checks_work: &mut Owed| {
        eprintln!(
            "wait: github {} — {dropped} webhook deliveries overflowed intake; \
             reconciling for them",
            src.repo
        );
        wal.relay_overflow(dropped);
        feed_work.owe();
        if wants_ci {
            checks_work.owe();
        }
    };

    loop {
        // The checks come BEFORE the feed and keep their own horizon:
        // a conclusion is nobody's action, so the own-action filter
        // has nothing to say about it, and waiting on a login it does
        // not need would hold a red build behind an unrelated outage.
        if wants_ci && checks_work.ready(checks_not_before) {
            checks_work.begin();
            checks_not_before = Some(tokio::time::Instant::now() + CI_RECHECK_FLOOR);
            match checks.reader.read().await {
                Ok(results) => {
                    checks_work.succeeded();
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
                    checks_work.failed(&e, None);
                    eprintln!("wait: github {} ci read failed: {e:#}", src.repo);
                }
            }
            continue;
        }
        if feed_work.ready(None) {
            feed_work.begin();
            match read_feed(src, &sel, &feed_side, &mut feed, &mut coordinator, &wal).await {
                Ok(true) => feed_work.succeeded(),
                Ok(false) => return, // wait gone
                Err(e) => {
                    // GitHub's X-Poll-Interval binds a retry too.
                    let floor = feed.horizon.floor.map(std::time::Duration::from_secs);
                    feed_work.failed(&e, floor);
                    eprintln!("wait: github {} feed read failed: {e:#}", src.repo);
                }
            }
            // The timer counts from the last read, under the floor
            // that read just learned.
            timer_due = next_tick(feed.horizon.floor);
            let evicted = coordinator.take_evicted();
            if evicted > 0 {
                overflowed(evicted, &mut feed_work, &mut checks_work);
            }
            continue;
        }

        let deadline = [
            wants_ci
                .then(|| checks_work.due(checks_not_before))
                .flatten(),
            feed_work.due(None),
            timer_due,
        ]
        .into_iter()
        .flatten()
        .min();
        let deliveries = !sel.needs_own_login() || feed.own_login.is_some();
        tokio::select! {
            () = tokio::time::sleep_until(deadline.unwrap_or_else(tokio::time::Instant::now)),
                if deadline.is_some() =>
            {
                if timer_due.is_some_and(|t| t <= tokio::time::Instant::now()) {
                    feed_work.owe();
                    if wants_ci {
                        checks_work.owe();
                    }
                    timer_due = next_tick(feed.horizon.floor);
                }
            }
            heard = relay.next(deliveries) => match heard {
                Heard::Listening => {
                    tx.report(key, crate::cli::event_source::Lifecycle::Listening);
                }
                Heard::CatchUp => {
                    feed_work.owe();
                    if wants_ci {
                        checks_work.owe();
                    }
                }
                Heard::ChecksCue => {
                    if wants_ci {
                        checks_work.owe();
                    }
                }
                Heard::Down(down) => {
                    let why = down.cause.describe(&src.repo);
                    let meanwhile = match poll_every {
                        Some(every) => format!("polling every {}s meanwhile", every.as_secs()),
                        None => "no poll_interval, so nothing is read until it is back".to_string(),
                    };
                    eprintln!(
                        "wait: github {} — webhook not listening: {why}; {meanwhile}; \
                         retrying in {}s",
                        src.repo,
                        down.retry_in.as_secs()
                    );
                    tx.report(
                        key,
                        crate::cli::event_source::Lifecycle::Degraded {
                            why,
                            polling: poll_every.is_some(),
                        },
                    );
                }
                Heard::Overflowed(dropped) => {
                    overflowed(dropped, &mut feed_work, &mut checks_work)
                }
                Heard::Delivery((name, payload)) => {
                    if let Some(c) = classify_webhook(
                        &src.repo,
                        &name,
                        &payload,
                        &sel,
                        feed.own_login.as_deref(),
                    ) && !coordinator.deliver(c)
                    {
                        return; // wait gone
                    }
                    let evicted = coordinator.take_evicted();
                    if evicted > 0 {
                        overflowed(evicted, &mut feed_work, &mut checks_work);
                    }
                }
            },
        }
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
    use crate::cli::github::relay::tests::listening;
    use crate::cli::github::relay::{RelayDown, RelayFailure};
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
        assert!(coordinator.baseline(vec![classified(1, Some("baseline"))]));
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

    /// The watch under test with the optional timer on: these tests
    /// are about what successive reads do, which without a timer only
    /// webhooks would cause.
    fn timed_src(events: &[GithubEventKind], include_own: bool) -> GithubSource {
        let mut s = src(events, include_own);
        s.poll_interval = Some("60s".into());
        s
    }

    // ── the coordinator: one emission owner, baseline horizon ──

    #[tokio::test]
    async fn coordinator_gates_emission_by_action_key() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::cli::event_source::EmissionSink::from(tx);
        let mut c = Coordinator::new(tx);
        assert!(c.baseline(vec![classified(1, Some("issue_opened#1"))]));
        // Baseline emitted nothing.
        assert!(rx.try_recv().is_err());
        // webhook-then-poll of the SAME action → exactly one emission.
        assert!(c.deliver(classified(2, Some("issue_opened#2"))));
        assert!(c.offer(classified(2, Some("issue_opened#2"))));
        assert!(rx.try_recv().is_ok(), "the first arrival emitted");
        assert!(rx.try_recv().is_err(), "the second path's copy dropped");
        // A poll-only action (the relay missed it) still emits.
        assert!(c.offer(classified(3, Some("issue_opened#3"))));
        assert!(rx.try_recv().is_ok());
        // A webhook is live even for an action the baseline recorded
        // as history: it wakes once, and only once.
        assert!(c.deliver(classified(1, Some("issue_opened#1"))));
        assert!(
            rx.try_recv().is_ok(),
            "history never suppresses a live delivery"
        );
        assert!(c.deliver(classified(1, Some("issue_opened#1"))));
        assert!(c.offer(classified(1, Some("issue_opened#1"))));
        assert!(
            rx.try_recv().is_err(),
            "after which it is an ordinary duplicate"
        );
        // KEYLESS wakes are never deduped.
        assert!(c.offer(classified(9, None)));
        assert!(c.offer(classified(9, None)));
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_ok(), "keyless = emit-always");
    }

    #[tokio::test]
    async fn coordinator_buffers_relay_deliveries_until_the_baseline() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::cli::event_source::EmissionSink::from(tx);
        let mut c = Coordinator::new(tx);
        // Deliveries BEFORE the baseline buffer silently…
        assert!(c.deliver(classified(1, Some("issue_opened#1"))));
        assert!(c.deliver(classified(5, Some("issue_opened#5"))));
        assert!(rx.try_recv().is_err(), "nothing emits pre-baseline");
        // …then drain BEFORE the baseline records its history, so
        // action 1 — live, and also on the baseline page — is pending,
        // while action 2, history only, is not.
        assert!(c.baseline(vec![
            classified(1, Some("issue_opened#1")),
            classified(2, Some("issue_opened#2")),
        ]));
        let mut woke = Vec::new();
        while let Ok((WaitItem::GithubEvent { number, .. }, _)) = rx.try_recv() {
            woke.push(number.unwrap());
        }
        assert_eq!(woke, vec![1, 5], "each live action once; history never");
    }

    #[tokio::test]
    async fn coordinator_buffer_is_bounded_drop_oldest() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::cli::event_source::EmissionSink::from(tx);
        let mut c = Coordinator::new(tx);
        for i in 0..(RELAY_BUFFER_CAP as u64 + 3) {
            assert!(c.deliver(classified(i, Some(&format!("issue_opened#{i}")))));
        }
        assert!(c.baseline(vec![]));
        let mut got = Vec::new();
        while let Ok((item, _)) = rx.try_recv() {
            let WaitItem::GithubEvent { number, .. } = item else {
                unreachable!()
            };
            got.push(number.unwrap());
        }
        assert_eq!(got.len(), RELAY_BUFFER_CAP, "bounded backlog");
        assert_eq!(*got.first().unwrap(), 3, "oldest dropped first");
        assert_eq!(c.take_evicted(), 3, "and what went is owed back");
        assert_eq!(c.take_evicted(), 0);
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
                &listening().inbox,
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
                &timed_src(&[GithubEventKind::IssueOpened], false),
                FeedSide {
                    fetcher: &f,
                    login: &FixedLogin("me"),
                },
                ChecksSide {
                    reader: &NoCi,
                    state: None,
                },
                &tx,
                &listening().inbox,
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
                &timed_src(&[GithubEventKind::CiFailed], true),
                FeedSide {
                    fetcher: &f,
                    login: &FixedLogin("me"),
                },
                ChecksSide {
                    reader: &ci,
                    state: None,
                },
                &tx,
                &listening().inbox,
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
                &timed_src(
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
                &listening().inbox,
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
                &timed_src(
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
                &listening().inbox,
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
                &timed_src(&[GithubEventKind::CiFailed], false),
                FeedSide {
                    fetcher: &f,
                    login: &NoLogin,
                },
                ChecksSide {
                    reader: &ci,
                    state: None,
                },
                &tx,
                &listening().inbox,
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
                &listening().inbox,
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
                &listening().inbox,
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
        let s1 = timed_src(&[GithubEventKind::IssueOpened], true);
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
                &listening().inbox,
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
        let s2 = timed_src(
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
                &listening().inbox,
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
                    &listening().inbox,
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

    #[tokio::test(start_paused = true)]
    async fn the_relay_is_filtered_by_the_scope() {
        let mut s = src(&[GithubEventKind::PrComment], true);
        s.prs = vec![7];
        let inbox = std::sync::Arc::new(RelayInbox::new());
        inbox.started();
        for number in [8u64, 7] {
            inbox.push((
                "issue_comment".to_string(),
                serde_json::json!({
                    "action": "created",
                    "issue": {"number": number, "pull_request": {}},
                    "comment": {"id": number * 100},
                    "sender": {"login": "them"}
                }),
            ));
        }
        let f = ScriptFetcher::new(vec![Box::new(|_p, _e| Ok(ok(vec![], None, None)))]);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::cli::event_source::EmissionSink::from(tx);
        let loop_inbox = inbox.clone();
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
                &loop_inbox,
                "k",
                None,
                Default::default(),
            )
            .await;
        });
        let item = tokio::time::timeout(Duration::from_secs(300), rx.recv())
            .await
            .expect("the watched PR's comment arrives")
            .expect("open");
        handle.abort();
        let _ = handle.await;
        let (WaitItem::GithubEvent { number, .. }, _) = item else {
            panic!("a github event");
        };
        assert_eq!(
            number,
            Some(7),
            "8's delivery was dropped, so the FIRST thing out is 7",
        );
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
                &listening().inbox,
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
                &listening().inbox,
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
                &listening().inbox,
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

    #[tokio::test(start_paused = true)]
    async fn a_finished_suite_makes_the_checks_be_read_now() {
        // The cue carries no conclusion — one App's suite is not the
        // PR's result — so it ends the window and the loop re-reads.
        // Same read as the timer's, which is what stops a cue's read
        // and a timer's read from disagreeing.
        let ci = ScriptCi::new(vec![
            vec![red(7, "head1", "fmt")],
            vec![red(7, "head2", "test")],
        ]);
        // One tick's worth of feed, then nothing: if the wake waited
        // for the next scheduled read it would take the poll cadence.
        let f = std::sync::Arc::new(ScriptFetcher::new(vec![]));
        let fetches = f.clone();
        let inbox = std::sync::Arc::new(RelayInbox::new());
        inbox.started();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::cli::event_source::EmissionSink::from(tx);
        let s = src(&[GithubEventKind::CiFailed], true);
        let loop_inbox = inbox.clone();
        let handle = tokio::spawn(async move {
            poll_loop(
                &s,
                FeedSide {
                    fetcher: &*f,
                    login: &FixedLogin("me"),
                },
                ChecksSide {
                    reader: &ci,
                    state: None,
                },
                &tx,
                &loop_inbox,
                "k",
                None,
                Default::default(),
            )
            .await;
        });
        // Let the first read establish, then a suite finishes
        // mid-window. The poll cadence is 60s, so a wake that only
        // arrived on the next scheduled tick would land at 60s — the
        // clock is what distinguishes "the cue did it" from "the
        // poll would have anyway".
        tokio::time::sleep(Duration::from_secs(20)).await;
        let cue_at = tokio::time::Instant::now();
        inbox.push((
            "check_suite".to_string(),
            serde_json::json!({ "action": "completed" }),
        ));
        let item = tokio::time::timeout(Duration::from_secs(300), rx.recv())
            .await
            .expect("the cue makes the conclusion be read")
            .expect("open");
        assert!(
            cue_at.elapsed() < Duration::from_secs(30),
            "the wake followed the cue, not the next scheduled read: {:?}",
            cue_at.elapsed(),
        );
        // And it cost the feed nothing: the checks moved, the feed
        // did not, and its cadence is the server's to set.
        assert_eq!(
            fetches.calls.lock().unwrap().len(),
            1,
            "only the first tick's fetch — a cue does not re-poll the events feed",
        );
        let (WaitItem::GithubEvent { event, title, .. }, _) = item else {
            panic!("a github event");
        };
        assert_eq!(event, "ci_failed");
        assert_eq!(title.as_deref(), Some("CI failed: test"));
        handle.abort();
        let _ = handle.await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_watch_with_no_ci_kinds_ignores_the_cue() {
        // check_suite is only subscribed for a CI watch, but a relay
        // can deliver what it likes: a watch that did not ask for
        // conclusions must not spend requests on them.
        struct CountingCi(std::sync::Arc<std::sync::atomic::AtomicUsize>);
        impl CiReader for CountingCi {
            async fn read(&self) -> anyhow::Result<Vec<(ci::PrHead, ci::CiRollup)>> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(Vec::new())
            }
        }
        let reads: std::sync::Arc<std::sync::atomic::AtomicUsize> = Default::default();
        let f = std::sync::Arc::new(ScriptFetcher::new(vec![]));
        let calls = f.clone();
        let inbox = std::sync::Arc::new(RelayInbox::new());
        inbox.started();
        inbox.push((
            "check_suite".to_string(),
            serde_json::json!({ "action": "completed" }),
        ));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::cli::event_source::EmissionSink::from(tx);
        let s = timed_src(&[GithubEventKind::IssueOpened], true);
        let loop_inbox = inbox.clone();
        let counter = reads.clone();
        let handle = tokio::spawn(async move {
            poll_loop(
                &s,
                FeedSide {
                    fetcher: &*f,
                    login: &FixedLogin("me"),
                },
                ChecksSide {
                    reader: &CountingCi(counter),
                    state: None,
                },
                &tx,
                &loop_inbox,
                "k",
                None,
                Default::default(),
            )
            .await;
        });
        tokio::time::sleep(Duration::from_secs(600)).await;
        handle.abort();
        let _ = handle.await;
        assert_eq!(
            reads.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "no ci kind configured, so nothing reads the checks",
        );
        // Ten windows at the 60s cadence: the cue must not have
        // swallowed one of them by ending a window it has no business
        // ending.
        assert!(
            calls.calls.lock().unwrap().len() >= 10,
            "the feed kept its own cadence: {} fetches",
            calls.calls.lock().unwrap().len(),
        );
    }

    /// codex 9bfe171: a cue must not postpone the FEED. Cues spaced
    /// between the checks floor and the feed interval used to reset
    /// the deadline every time, so the feed never polled again — and
    /// a watch's timer is the backstop for what the feed lags on.
    #[tokio::test(start_paused = true)]
    async fn a_stream_of_cues_does_not_starve_the_feed() {
        struct QuietCi;
        impl CiReader for QuietCi {
            async fn read(&self) -> anyhow::Result<Vec<(ci::PrHead, ci::CiRollup)>> {
                Ok(Vec::new())
            }
        }
        let f = std::sync::Arc::new(ScriptFetcher::new(vec![]));
        let fetches = f.clone();
        let inbox = std::sync::Arc::new(RelayInbox::new());
        inbox.started();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::cli::event_source::EmissionSink::from(tx);
        let s = timed_src(
            &[GithubEventKind::IssueOpened, GithubEventKind::CiFailed],
            true,
        );
        let loop_inbox = inbox.clone();
        let handle = tokio::spawn(async move {
            poll_loop(
                &s,
                FeedSide {
                    fetcher: &*f,
                    login: &FixedLogin("me"),
                },
                ChecksSide {
                    reader: &QuietCi,
                    state: None,
                },
                &tx,
                &loop_inbox,
                "k",
                None,
                Default::default(),
            )
            .await;
        });
        // A cue every 20s for 140s: past the 15s checks floor, inside
        // the 60s feed interval. The feed is due at 0, 60 and 120.
        for _ in 0..7 {
            inbox.push((
                "check_suite".to_string(),
                serde_json::json!({ "action": "completed" }),
            ));
            tokio::time::sleep(Duration::from_secs(20)).await;
        }
        handle.abort();
        let _ = handle.await;
        assert!(
            fetches.calls.lock().unwrap().len() >= 3,
            "the feed kept its own schedule through the cues: {} fetches",
            fetches.calls.lock().unwrap().len(),
        );
    }

    /// codex 9bfe171: a FAILED read has to consume the cooldown too.
    /// Recording only successes let every queued cue retry a broken
    /// endpoint immediately — a storm exactly when it costs most.
    #[tokio::test(start_paused = true)]
    async fn a_failing_checks_endpoint_is_still_rate_limited() {
        struct CountingDeadCi(std::sync::Arc<std::sync::atomic::AtomicUsize>);
        impl CiReader for CountingDeadCi {
            async fn read(&self) -> anyhow::Result<Vec<(ci::PrHead, ci::CiRollup)>> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                anyhow::bail!("checks unreachable")
            }
        }
        let attempts: std::sync::Arc<std::sync::atomic::AtomicUsize> = Default::default();
        let f = ScriptFetcher::new(vec![]);
        let inbox = std::sync::Arc::new(RelayInbox::new());
        inbox.started();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::cli::event_source::EmissionSink::from(tx);
        let s = src(&[GithubEventKind::CiFailed], true);
        let loop_inbox = inbox.clone();
        let counter = attempts.clone();
        let handle = tokio::spawn(async move {
            poll_loop(
                &s,
                FeedSide {
                    fetcher: &f,
                    login: &FixedLogin("me"),
                },
                ChecksSide {
                    reader: &CountingDeadCi(counter),
                    state: None,
                },
                &tx,
                &loop_inbox,
                "k",
                None,
                Default::default(),
            )
            .await;
        });
        tokio::time::sleep(Duration::from_secs(1)).await;
        let before = attempts.load(std::sync::atomic::Ordering::SeqCst);
        for _ in 0..10 {
            inbox.push((
                "check_suite".to_string(),
                serde_json::json!({ "action": "completed" }),
            ));
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
        let after = attempts.load(std::sync::atomic::Ordering::SeqCst);
        handle.abort();
        let _ = handle.await;
        assert!(
            after - before <= 1,
            "a failing endpoint gets the same floor as a working one: {before} → {after}",
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cues_inside_the_floor_do_not_each_cost_a_read() {
        // A busy PR finishes several suites at once. Re-reading for
        // every one of them is a round of requests per app per push,
        // and an unscoped watch pays it for every open PR.
        struct CountingCi(std::sync::Arc<std::sync::atomic::AtomicUsize>);
        impl CiReader for CountingCi {
            async fn read(&self) -> anyhow::Result<Vec<(ci::PrHead, ci::CiRollup)>> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(vec![red(7, "head1", "fmt")])
            }
        }
        let reads: std::sync::Arc<std::sync::atomic::AtomicUsize> = Default::default();
        let f = ScriptFetcher::new(vec![]);
        let inbox = std::sync::Arc::new(RelayInbox::new());
        inbox.started();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::cli::event_source::EmissionSink::from(tx);
        let s = src(&[GithubEventKind::CiFailed], true);
        let loop_inbox = inbox.clone();
        let counter = reads.clone();
        let handle = tokio::spawn(async move {
            poll_loop(
                &s,
                FeedSide {
                    fetcher: &f,
                    login: &FixedLogin("me"),
                },
                ChecksSide {
                    reader: &CountingCi(counter),
                    state: None,
                },
                &tx,
                &loop_inbox,
                "k",
                None,
                Default::default(),
            )
            .await;
        });
        tokio::time::sleep(Duration::from_secs(1)).await;
        let before = reads.load(std::sync::atomic::Ordering::SeqCst);
        for _ in 0..10 {
            inbox.push((
                "check_suite".to_string(),
                serde_json::json!({ "action": "completed" }),
            ));
        }
        // Well inside the floor.
        tokio::time::sleep(Duration::from_secs(5)).await;
        let after = reads.load(std::sync::atomic::Ordering::SeqCst);
        handle.abort();
        let _ = handle.await;
        assert!(
            after - before <= 1,
            "ten cues in five seconds bought at most one read, not ten: {before} → {after}",
        );
    }

    /// codex 4b6035b: a feed whose baseline saw NO events, followed
    /// by a relay delivery, leaves a WAL with rows but no feed ids
    /// and a saved ETag. That is an established feed, and reading
    /// "no feed ids" as "never baselined" cost it twice: the restored
    /// ETag 304s forever so the baseline never completes, and a
    /// genuinely new issue is recorded as history instead of
    /// delivered.
    #[tokio::test(start_paused = true)]
    async fn an_empty_baseline_and_a_relay_row_is_an_established_feed() {
        let dir = wal_tempdir();
        // Arm 1: the feed baselines on an EMPTY feed and saves an
        // ETag; a webhook arrives and is emitted through the gate.
        let inbox = std::sync::Arc::new(RelayInbox::new());
        inbox.started();
        inbox.push((
            "issues".to_string(),
            serde_json::json!({
                "action": "opened",
                "issue": { "number": 4, "title": "t", "html_url": "u" },
                "sender": { "login": "them" }
            }),
        ));
        let f = ScriptFetcher::new(vec![Box::new(|_p, _e| {
            Ok(ok(vec![], Some("\"e1\""), None))
        })]);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::cli::event_source::EmissionSink::from(tx);
        let s1 = src(&[GithubEventKind::IssueOpened], true);
        let d = dir.clone();
        let loop_inbox = inbox.clone();
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
                &loop_inbox,
                "k",
                Some(open_log(&d)),
                Default::default(),
            )
            .await;
        });
        let relayed = tokio::time::timeout(Duration::from_secs(300), rx.recv())
            .await
            .expect("the webhook emits once the empty baseline is done")
            .expect("open");
        let (WaitItem::GithubEvent { number, .. }, _) = relayed else {
            panic!("a github event");
        };
        assert_eq!(number, Some(4));
        run1.abort();
        let _ = run1.await;
        // Ack it, so the backlog cannot be what wakes arm 2.
        {
            let mut log = open_log(&dir);
            let seqs: Vec<u64> = log
                .read_rows()
                .unwrap()
                .map(|r| r.rows.iter().map(|row| row.seq).collect())
                .unwrap_or_default();
            for seq in seqs {
                log.append_ack(seq).unwrap();
            }
        }

        // Arm 2: a new issue, created after that baseline. It is
        // CATCH-UP, not history.
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
                &listening().inbox,
                "k",
                Some(open_log(&d)),
                Default::default(),
            )
            .await;
        });
        let woke = tokio::time::timeout(Duration::from_secs(300), rx.recv())
            .await
            .expect("an event after an empty baseline is delivered, not re-baselined")
            .expect("open");
        let (WaitItem::GithubEvent { number, .. }, _) = woke else {
            panic!("a github event");
        };
        assert_eq!(number, Some(9));
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
                &listening().inbox,
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
                &listening().inbox,
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
                &listening().inbox,
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

    /// codex ae48e92 [P2]: a log holding only checks rows must not
    /// open the RELAY's gate. That gate is the feed's statement that
    /// it has a horizon; without one, a queued delivery would emit
    /// history nobody had baselined.
    #[tokio::test(start_paused = true)]
    async fn a_checks_only_log_does_not_release_a_queued_webhook() {
        struct DeadFeed;
        impl EventFetcher for DeadFeed {
            async fn fetch(&self, _page: u32, _etag: Option<String>) -> anyhow::Result<GhResponse> {
                anyhow::bail!("events unreachable")
            }
        }
        let dir = wal_tempdir();
        // Arm 1: only the checks read works, so the log holds only
        // its rows.
        let ci = ScriptCi::new(vec![
            vec![red(7, "head1", "fmt")],
            vec![red(7, "head2", "test")],
        ]);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::cli::event_source::EmissionSink::from(tx);
        let s1 = src(
            &[GithubEventKind::IssueOpened, GithubEventKind::CiFailed],
            true,
        );
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
                &listening().inbox,
                "k",
                Some(open_log(&d)),
                Default::default(),
            )
            .await;
        });
        let _ = tokio::time::timeout(Duration::from_secs(300), rx.recv()).await;
        run1.abort();
        let _ = run1.await;

        // Arm 2: the feed is still down and an issues webhook is
        // waiting. It must stay buffered.
        let inbox = std::sync::Arc::new(RelayInbox::new());
        inbox.started();
        inbox.push((
            "issues".to_string(),
            serde_json::json!({
                "action": "opened",
                "issue": { "number": 9, "title": "t", "html_url": "u" },
                "sender": { "login": "them" }
            }),
        ));
        let ci2 = ScriptCi::new(vec![vec![red(7, "head2", "test")]]);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::cli::event_source::EmissionSink::from(tx);
        let s2 = src(
            &[GithubEventKind::IssueOpened, GithubEventKind::CiFailed],
            true,
        );
        let d = dir.clone();
        let loop_inbox = inbox.clone();
        let run2 = tokio::spawn(async move {
            poll_loop(
                &s2,
                FeedSide {
                    fetcher: &DeadFeed,
                    login: &FixedLogin("me"),
                },
                ChecksSide {
                    reader: &ci2,
                    state: Some(d.join("t.ci.json")),
                },
                &tx,
                &loop_inbox,
                "k",
                Some(open_log(&d)),
                Default::default(),
            )
            .await;
        });
        tokio::time::sleep(Duration::from_secs(600)).await;
        // Arm 1's CI wake was never acked, so it re-presents here —
        // that is the at-least-once backlog, not the webhook.
        let mut seen = Vec::new();
        while let Ok((item, _)) = rx.try_recv() {
            let WaitItem::GithubEvent { event, .. } = &item else {
                continue;
            };
            seen.push(event.clone());
        }
        assert!(
            !seen.iter().any(|e| e == "issue_opened"),
            "the feed has never baselined, so its relay gate stays shut — got {seen:?}",
        );
        run2.abort();
        let _ = run2.await;
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
                    &listening().inbox,
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
        let s = timed_src(&[GithubEventKind::CiFailed], true);
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
                &listening().inbox,
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
                &listening().inbox,
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
                &listening().inbox,
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
                &listening().inbox,
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
                &timed_src(&[GithubEventKind::IssueOpened], false),
                FeedSide {
                    fetcher: &f,
                    login: &login,
                },
                ChecksSide {
                    reader: &NoCi,
                    state: None,
                },
                &tx,
                &listening().inbox,
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
                &listening().inbox,
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
                &timed_src(&[GithubEventKind::IssueOpened], false),
                FeedSide {
                    fetcher: &*f2,
                    login: &FixedLogin("me"),
                },
                ChecksSide {
                    reader: &NoCi,
                    state: None,
                },
                &tx,
                &listening().inbox,
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

    #[tokio::test(start_paused = true)]
    async fn relay_deliveries_emit_immediately_and_dedup_against_the_poll() {
        // The full M2 lifecycle through the REAL poll_loop, no gh and
        // no sockets: baseline poll seeds the horizon; a relay
        // delivery emits IMMEDIATELY between ticks; the next poll's
        // copy of the SAME action (same key) is suppressed by the
        // coordinator; a poll-only action still emits; and the relay
        // closing falls back to polling alone.
        let s = timed_src(&[GithubEventKind::IssueOpened], false);
        let f = ScriptFetcher::new(vec![
            // Tick 1: the baseline — issue #1 exists at arm time.
            Box::new(|_p, _e| Ok(ok(vec![issue("1", 1)], Some("\"e1\""), None))),
            Box::new(|_p, _e| Ok(ok(vec![], None, None))),
            // Tick 2: the feed catches up — issue #2 (already emitted
            // via the relay: SAME action key) and issue #3 (poll-only).
            Box::new(|_p, _e| {
                Ok(ok(
                    vec![
                        serde_json::json!({"id":"102","type":"IssuesEvent","actor":{"login":"them"},
                            "payload":{"action":"opened","issue":{"number":2}}}),
                        serde_json::json!({"id":"103","type":"IssuesEvent","actor":{"login":"them"},
                            "payload":{"action":"opened","issue":{"number":3}}}),
                        issue("1", 1),
                    ],
                    None,
                    None,
                ))
            }),
        ]);
        let inbox = std::sync::Arc::new(RelayInbox::new());
        inbox.started();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::cli::event_source::EmissionSink::from(tx);
        let loop_inbox = inbox.clone();
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
                &loop_inbox,
                "k",
                None,
                Default::default(),
            )
            .await;
        });
        // A webhook delivery for issue #2 lands between ticks — it must
        // emit without waiting for the next poll.
        inbox.push((
            "issues".to_string(),
            serde_json::json!({"action":"opened","sender":{"login":"them"},
                "issue":{"number":2}}),
        ));
        let first = tokio::time::timeout(std::time::Duration::from_secs(300), rx.recv())
            .await
            .expect("the relay delivery emits")
            .expect("open");
        let (WaitItem::GithubEvent { number, .. }, _) = first else {
            unreachable!()
        };
        assert_eq!(number, Some(2), "relay latency path");
        // The next poll carries #2 (suppressed — same key) and #3
        // (poll-only → emits). Exactly ONE more emission, #3.
        let second = tokio::time::timeout(std::time::Duration::from_secs(300), rx.recv())
            .await
            .expect("the poll-only action emits")
            .expect("open");
        let (WaitItem::GithubEvent { number, .. }, _) = second else {
            unreachable!()
        };
        assert_eq!(number, Some(3), "the poll's copy of #2 was suppressed");
        tokio::task::yield_now().await;
        assert!(rx.try_recv().is_err(), "no duplicate of #2 ever surfaced");
        handle.abort();
        let _ = handle.await;
    }

    #[tokio::test(start_paused = true)]
    async fn pre_login_relay_intake_is_bounded_and_filtered_after_resolution() {
        // codex d7f4a8f: while /user is down the relay channel must not
        // grow unboundedly — intake drains into the bounded drop-oldest
        // backlog, and classification waits for the login so a
        // SELF-AUTHORED delivery buffered pre-login is filtered after
        // resolution (fail-closed own-actor, preserved end to end).
        let login = ScriptLogin(std::sync::Mutex::new(
            vec![
                Err(anyhow::anyhow!("gh down")),
                Err(anyhow::anyhow!("still down")),
                Ok("me".to_string()),
            ]
            .into(),
        ));
        // Fetcher: the (post-login) baseline is empty; then 304s.
        let f = ScriptFetcher::new(vec![Box::new(|_p, _e| Ok(ok(vec![], None, None)))]);
        let inbox = std::sync::Arc::new(RelayInbox::new());
        inbox.started();
        // MORE than the cap of foreign deliveries + one self-authored,
        // all pushed while the login is still failing — the bound
        // applies at INGRESS (codex 840725c).
        for i in 0..(RELAY_BUFFER_CAP as u64 + 3) {
            inbox.push((
                "issues".to_string(),
                serde_json::json!({"action":"opened","sender":{"login":"them"},
                    "issue":{"number":i}}),
            ));
        }
        inbox.push((
            "issues".to_string(),
            serde_json::json!({"action":"opened","sender":{"login":"me"},
                "issue":{"number":9999}}),
        ));
        assert_eq!(inbox.len(), RELAY_BUFFER_CAP, "bounded at the inbox itself");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::cli::event_source::EmissionSink::from(tx);
        let loop_inbox = inbox.clone();
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
                &loop_inbox,
                "k",
                None,
                Default::default(),
            )
            .await;
        });
        // Expected: 259 foreign + 1 own sent; the bound keeps the LAST
        // 256 (foreign 4..=258 plus the own one); the own delivery is
        // filtered at classification ⇒ 255 emissions, starting at 4.
        let expect = RELAY_BUFFER_CAP - 1;
        let got: Vec<u64> = tokio::time::timeout(std::time::Duration::from_secs(600), async {
            let mut got = Vec::new();
            while got.len() < expect {
                let Some(item) = rx.recv().await else { break };
                let (WaitItem::GithubEvent { number, .. }, _) = item else {
                    unreachable!()
                };
                got.push(number.unwrap());
            }
            got
        })
        .await
        .expect("the bounded backlog drains after login+baseline");
        assert_eq!(got.len(), expect, "drop-oldest bound applied");
        assert!(!got.contains(&9999), "own delivery filtered post-login");
        assert_eq!(*got.first().unwrap(), 4, "oldest dropped first");
        // Nothing further: the own delivery never emits.
        tokio::task::yield_now().await;
        assert!(rx.try_recv().is_err());
        handle.abort();
        let _ = handle.await;
    }

    #[tokio::test(start_paused = true)]
    async fn inbox_bounds_ingress_while_login_resolution_hangs() {
        // codex 840725c: the bound must hold even while the consumer is
        // stuck INSIDE an await — a login resolution that never
        // returns. Producers exceed the cap; the inbox itself evicts.
        struct PendingLogin;
        impl LoginResolver for PendingLogin {
            async fn resolve(&self) -> anyhow::Result<String> {
                std::future::pending::<()>().await;
                unreachable!()
            }
        }
        let f = ScriptFetcher::new(vec![]);
        let inbox = std::sync::Arc::new(RelayInbox::new());
        inbox.started();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::cli::event_source::EmissionSink::from(tx);
        let loop_inbox = inbox.clone();
        let handle = tokio::spawn(async move {
            poll_loop(
                &src(&[GithubEventKind::IssueOpened], false),
                FeedSide {
                    fetcher: &f,
                    login: &PendingLogin,
                },
                ChecksSide {
                    reader: &NoCi,
                    state: None,
                },
                &tx,
                &loop_inbox,
                "k",
                None,
                Default::default(),
            )
            .await;
        });
        tokio::task::yield_now().await; // the loop is now inside resolve()
        for i in 0..(RELAY_BUFFER_CAP as u64 * 2) {
            inbox.push((
                "issues".to_string(),
                serde_json::json!({"action":"opened","sender":{"login":"them"},
                    "issue":{"number":i}}),
            ));
        }
        assert_eq!(
            inbox.len(),
            RELAY_BUFFER_CAP,
            "eviction at the inbox boundary, consumer hung or not"
        );
        handle.abort();
        let _ = handle.await;
    }

    #[tokio::test(start_paused = true)]
    async fn relay_closing_while_login_gated_falls_back_without_ending_the_loop() {
        // The relay dies while the source is still login-gated: one
        // fallback line, and the poll loop keeps retrying login and
        // then polls normally.
        let login = ScriptLogin(std::sync::Mutex::new(
            vec![Err(anyhow::anyhow!("gh down")), Ok("me".to_string())].into(),
        ));
        let f = ScriptFetcher::new(vec![
            // Baseline (post-login), then a delta with a new action —
            // proof the loop survived the relay's death.
            Box::new(|_p, _e| Ok(ok(vec![], None, None))),
            Box::new(|_p, _e| Ok(ok(vec![issue("7", 7)], None, None))),
        ]);
        let inbox = std::sync::Arc::new(RelayInbox::new());
        inbox.started();
        inbox.down(RelayDown {
            cause: RelayFailure::NoPing,
            retry_in: Duration::from_secs(5),
        });
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::cli::event_source::EmissionSink::from(tx);
        let loop_inbox = inbox.clone();
        let handle = tokio::spawn(async move {
            poll_loop(
                &timed_src(&[GithubEventKind::IssueOpened], false),
                FeedSide {
                    fetcher: &f,
                    login: &login,
                },
                ChecksSide {
                    reader: &NoCi,
                    state: None,
                },
                &tx,
                &loop_inbox,
                "k",
                None,
                Default::default(),
            )
            .await;
        });
        let item = tokio::time::timeout(std::time::Duration::from_secs(300), rx.recv())
            .await
            .expect("the loop out-lives the relay and still polls")
            .expect("open");
        let (WaitItem::GithubEvent { number, .. }, _) = item else {
            unreachable!()
        };
        assert_eq!(number, Some(7));
        handle.abort();
        let _ = handle.await;
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
                &listening().inbox,
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
                &listening().inbox,
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
                &listening().inbox,
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
                &listening().inbox,
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
    /// forever and the baseline (which unblocks buffered relay
    /// deliveries) would never establish. Cold start = unconditional
    /// fetch, real baseline, relay drains.
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
        let inbox = std::sync::Arc::new(RelayInbox::new());
        inbox.started();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::cli::event_source::EmissionSink::from(tx);
        let s = timed_src(&[GithubEventKind::IssueOpened], true);
        let d = dir.clone();
        let loop_inbox = inbox.clone();
        // A pre-baseline relay delivery: only a REAL baseline drains it.
        inbox.push((
            "issues".to_string(),
            serde_json::json!({"action":"opened","sender":{"login":"them"},
                "issue":{"number":5}}),
        ));
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
                &loop_inbox,
                "k",
                Some(open_log(&d)),
                Default::default(),
            )
            .await;
        });
        let first = tokio::time::timeout(std::time::Duration::from_secs(300), rx.recv())
            .await
            .expect("the baseline must establish and drain the buffered relay delivery")
            .expect("open");
        let (WaitItem::GithubEvent { number, .. }, _) = &first else {
            unreachable!()
        };
        assert_eq!(*number, Some(5), "the buffered relay delivery drained");
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
                &listening().inbox,
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
                &listening().inbox,
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
        let s = timed_src(&[GithubEventKind::IssueOpened], true);
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
                &listening().inbox,
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

    /// codex 82a221a's restart scenario: a relay-emitted action whose
    /// event the poll NEVER observed must not double-emit after a
    /// restart — the action-key horizon is rebuilt from the WAL.
    #[tokio::test(start_paused = true)]
    async fn relay_emitted_action_stays_suppressed_across_restart() {
        let dir = wal_tempdir();
        // Run 1: empty baseline, then a relay delivery emits issue #2.
        let f = ScriptFetcher::new(vec![Box::new(|_p, _e| Ok(ok(vec![], Some("\"e\""), None)))]);
        let inbox = std::sync::Arc::new(RelayInbox::new());
        inbox.started();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::cli::event_source::EmissionSink::from(tx);
        let s1 = src(&[GithubEventKind::IssueOpened], true);
        let d = dir.clone();
        let loop_inbox = inbox.clone();
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
                &loop_inbox,
                "k",
                Some(open_log(&d)),
                Default::default(),
            )
            .await;
        });
        inbox.push((
            "issues".to_string(),
            serde_json::json!({"action":"opened","sender":{"login":"them"},
                "issue":{"number":2}}),
        ));
        let first = tokio::time::timeout(std::time::Duration::from_secs(300), rx.recv())
            .await
            .expect("relay emits")
            .expect("open");
        let (WaitItem::GithubEvent { number, .. }, _) = &first else {
            unreachable!()
        };
        assert_eq!(*number, Some(2));
        run1.abort();
        let _ = run1.await;
        // Ack it (the agent reacted) so run 2's backlog is empty and
        // any second emission could only be a dedup failure.
        {
            let mut log = open_log(&dir);
            let seq = log.load().unhandled[0].seq;
            log.append_ack(seq).unwrap();
        }

        // Run 2: the poll NOW observes the same action from the feed
        // (new feed id, same action key). It must be suppressed.
        let f = ScriptFetcher::new(vec![Box::new(|_p, _e| {
            Ok(ok(vec![issue("77", 2)], None, None))
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
                &listening().inbox,
                "k",
                Some(open_log(&d)),
                Default::default(),
            )
            .await;
        });
        // Give the loop several ticks' worth of virtual time.
        tokio::time::sleep(std::time::Duration::from_secs(120)).await;
        assert!(
            rx.try_recv().is_err(),
            "the poll's copy of the relay-emitted action double-emitted after restart"
        );
        // Its durable decision is an observation, so the feed id joins
        // the cursor.
        let loaded = open_log(&dir).load();
        assert!(loaded.feed_ids.iter().any(|id| id == "77"));
        run2.abort();
        let _ = run2.await;
    }

    // ── the listening lifecycle (github-watches-are-event-driven) ──

    use crate::cli::github::relay::tests::{FakeForwarder, Run, settle};
    use crate::cli::github::relay::{
        FORWARDING_ERROR, Subscription, Transports, webhook_event_names,
    };

    type Rollups = Vec<(ci::PrHead, ci::CiRollup)>;

    /// A CI reader that answers from a script — then the last answer,
    /// or nothing — records when it was asked, and can hold its next
    /// read until released.
    #[derive(Default)]
    struct TestCi {
        answers: Mutex<std::collections::VecDeque<Result<Rollups, &'static str>>>,
        reads: Mutex<Vec<tokio::time::Instant>>,
        hold: Mutex<Option<std::sync::Arc<tokio::sync::Notify>>>,
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
            let hold = self.hold.lock().unwrap().take();
            if let Some(hold) = hold {
                hold.notified().await;
            }
            let mut answers = self.answers.lock().unwrap();
            let next = match answers.len() {
                0 => Ok(Vec::new()),
                1 => answers.front().cloned().unwrap(),
                _ => answers.pop_front().unwrap(),
            };
            next.map_err(|e| anyhow::anyhow!(e))
        }
    }

    fn pending(number: u64, head: &str) -> (ci::PrHead, ci::CiRollup) {
        (
            ci::PrHead {
                number,
                head: head.into(),
            },
            ci::CiRollup::Pending,
        )
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

    fn webhook_issue(n: u64) -> crate::cli::github::relay::RawDelivery {
        (
            "issues".into(),
            serde_json::json!({"action":"opened","sender":{"login":"them"},"issue":{"number":n}}),
        )
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

    /// One watch under test, owning its subscription as the
    /// supervisor does.
    #[allow(clippy::too_many_arguments)]
    fn spawn_watch<
        L: LoginResolver + Send + Sync + 'static,
        C: CiReader + Send + Sync + 'static,
    >(
        s: GithubSource,
        fetcher: std::sync::Arc<ScriptFetcher>,
        login: std::sync::Arc<L>,
        reader: std::sync::Arc<C>,
        sub: Subscription,
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
                &sub.inbox,
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

    fn inbox() -> std::sync::Arc<RelayInbox> {
        std::sync::Arc::new(RelayInbox::new())
    }

    const DAY: Duration = Duration::from_secs(24 * 3600);

    /// The ordering and the idle contract together: nothing is read
    /// before the relay proves itself, the catch-up is exactly one
    /// feed reconcile and one checks read, and then a long quiet run
    /// reads nothing at all.
    #[tokio::test(start_paused = true)]
    async fn the_catch_up_waits_for_proof_and_then_nothing_is_read() {
        let f = std::sync::Arc::new(ScriptFetcher::new(vec![]));
        let ci = TestCi::answering(vec![]);
        let relay = inbox();
        let (tx, _rx) = sink();
        let run = spawn_watch(
            src(
                &[GithubEventKind::IssueOpened, GithubEventKind::CiFailed],
                false,
            ),
            f.clone(),
            std::sync::Arc::new(FixedLogin("me")),
            ci.clone(),
            Subscription::detached(relay.clone()),
            None,
            tx,
        );
        tokio::time::sleep(Duration::from_secs(3600)).await;
        assert_eq!(
            (feed_reads(&f), ci.reads()),
            (0, 0),
            "nothing is read before the relay is proven ready"
        );
        relay.started();
        settle().await;
        assert_eq!((feed_reads(&f), ci.reads()), (1, 1), "one catch-up");
        tokio::time::sleep(DAY).await;
        assert_eq!(
            (feed_reads(&f), ci.reads()),
            (1, 1),
            "a healthy idle watch with no timer reads nothing"
        );
        run.abort();
    }

    /// A forwarder that exits restarts, and the restart is a new start:
    /// its catch-up delivers what fired while it was down, once, even
    /// when the webhook later delivers the same action.
    #[tokio::test(start_paused = true)]
    async fn a_restart_catches_up_once_more_and_wakes_the_missed_event_once() {
        let fake = FakeForwarder::default();
        let transports = Transports::new(fake.clone());
        let s = src(&[GithubEventKind::IssueOpened], false);
        let f = std::sync::Arc::new(ScriptFetcher::new(vec![
            Box::new(|_p, _e| Ok(ok(vec![], None, None))),
            Box::new(|_p, _e| Ok(ok(vec![feed_issue("9", 9)], None, None))),
            Box::new(|_p, _e| Ok(ok(vec![], None, None))),
        ]));
        let (tx, mut rx) = sink();
        let sub =
            transports.subscribe("o/r", &webhook_event_names(&[GithubEventKind::IssueOpened]));
        let run = spawn_watch(
            s,
            f.clone(),
            std::sync::Arc::new(FixedLogin("me")),
            std::sync::Arc::new(NoCi),
            sub,
            None,
            tx,
        );
        settle().await;
        assert_eq!(feed_reads(&f), 1);
        fake.exit();
        settle().await;
        tokio::time::sleep(Duration::from_secs(5)).await;
        settle().await;
        assert_eq!(fake.started(), 2, "restarted after its backoff");
        assert_eq!(feed_reads(&f), 2, "exactly one more catch-up");
        assert_eq!(woke_numbers(&mut rx), vec![9]);
        fake.deliver(webhook_issue(9).0.as_str(), webhook_issue(9).1);
        settle().await;
        assert!(
            woke_numbers(&mut rx).is_empty(),
            "the webhook's copy is the same event"
        );
        tokio::time::sleep(DAY).await;
        assert_eq!(feed_reads(&f), 2);
        run.abort();
    }

    /// Two agents' watches and a differently scoped one of the same
    /// repo: one forwarder, each delivery reaches exactly the watches
    /// whose filter admits it, each logs and acks on its own, leaving
    /// keeps the forwarder for the rest, and the last out stops it.
    #[tokio::test(start_paused = true)]
    async fn agents_share_one_forwarder_and_keep_their_own_logs() {
        let fake = FakeForwarder::default();
        let transports = Transports::new(fake.clone());
        let opened = src(&[GithubEventKind::IssueOpened], false);
        let mut closed = src(&[GithubEventKind::IssueClosed], false);
        closed.include_own_actions = true;
        let dirs: Vec<std::path::PathBuf> = (0..3).map(|_| wal_tempdir()).collect();
        let mut rxs = Vec::new();
        let mut runs = Vec::new();
        for (i, s) in [opened.clone(), opened, closed].into_iter().enumerate() {
            let (tx, rx) = sink();
            rxs.push(rx);
            let sub = transports.subscribe("o/r", &webhook_event_names(&Selection::of(&s).kinds()));
            runs.push(spawn_watch(
                s,
                std::sync::Arc::new(ScriptFetcher::new(vec![Box::new(|_p, _e| {
                    Ok(ok(vec![], None, None))
                })])),
                std::sync::Arc::new(FixedLogin("me")),
                std::sync::Arc::new(NoCi),
                sub,
                Some(open_log(&dirs[i])),
                tx,
            ));
        }
        settle().await;
        assert_eq!(fake.started(), 1, "one forwarder for the repo");
        let (name, payload) = webhook_issue(5);
        fake.deliver(&name, payload);
        settle().await;
        assert_eq!(woke_numbers(&mut rxs[0]), vec![5]);
        assert_eq!(woke_numbers(&mut rxs[1]), vec![5]);
        assert!(
            woke_numbers(&mut rxs[2]).is_empty(),
            "a closed-only watch does not admit it"
        );

        let unhandled = |dir: &std::path::Path| {
            open_log(dir)
                .read_rows()
                .unwrap()
                .unwrap()
                .rows
                .into_iter()
                .filter(|r| !r.acked && !r.baseline)
                .count()
        };
        let seq = open_log(&dirs[0]).read_rows().unwrap().unwrap().rows[0].seq;
        open_log(&dirs[0]).append_ack(seq).unwrap();
        assert_eq!(
            (
                unhandled(&dirs[0]),
                unhandled(&dirs[1]),
                unhandled(&dirs[2])
            ),
            (0, 1, 0),
            "one agent's ack is its own"
        );

        runs.remove(0).abort();
        settle().await;
        let (name, payload) = webhook_issue(6);
        fake.deliver(&name, payload);
        settle().await;
        assert_eq!(woke_numbers(&mut rxs[1]), vec![6], "the other still hears");
        assert_eq!(fake.started(), 1, "and nothing restarted");
        for run in runs {
            run.abort();
        }
        settle().await;
        assert_eq!(fake.alive(), 0, "the last one out stops the child");
    }

    /// A CI-only watch joining an issues-only one needs events the
    /// forwarder does not carry: it restarts on the union — the head
    /// changes and the check cues included — and both watches catch up.
    #[tokio::test(start_paused = true)]
    async fn a_ci_watch_joining_widens_the_forwarder_and_everyone_catches_up() {
        let fake = FakeForwarder::default();
        let transports = Transports::new(fake.clone());
        let issues = src(&[GithubEventKind::IssueOpened], false);
        let f1 = std::sync::Arc::new(ScriptFetcher::new(vec![]));
        let (tx1, _rx1) = sink();
        let sub =
            transports.subscribe("o/r", &webhook_event_names(&[GithubEventKind::IssueOpened]));
        let r1 = spawn_watch(
            issues,
            f1.clone(),
            std::sync::Arc::new(FixedLogin("me")),
            std::sync::Arc::new(NoCi),
            sub,
            None,
            tx1,
        );
        settle().await;
        assert_eq!(feed_reads(&f1), 1);

        let ci_only = src(&[GithubEventKind::CiFailed], true);
        let ci = TestCi::answering(vec![]);
        let (tx2, _rx2) = sink();
        let sub = transports.subscribe("o/r", &webhook_event_names(&[GithubEventKind::CiFailed]));
        let r2 = spawn_watch(
            ci_only,
            std::sync::Arc::new(ScriptFetcher::new(vec![])),
            std::sync::Arc::new(FixedLogin("me")),
            ci.clone(),
            sub,
            None,
            tx2,
        );
        settle().await;
        let runs = fake.runs.lock().unwrap().clone();
        assert_eq!(
            runs,
            vec![
                "issues".to_string(),
                "check_suite,issues,pull_request,status".to_string()
            ]
        );
        assert_eq!(feed_reads(&f1), 2, "the old watch's new start");
        assert_eq!(ci.reads(), 1, "the joiner's catch-up");
        r1.abort();
        r2.abort();
    }

    /// Someone else's forwarder holds the slot: never ready, named as a
    /// conflict, and a watch with a timer keeps polling meanwhile.
    #[tokio::test(start_paused = true)]
    async fn a_held_slot_is_reported_and_a_timed_watch_keeps_polling() {
        let fake = FakeForwarder::scripted(&vec![Run::SlotTaken; 8]);
        let transports = Transports::new(fake.clone());
        let f = std::sync::Arc::new(ScriptFetcher::new(vec![]));
        let (etx, _erx) = tokio::sync::mpsc::unbounded_channel();
        let (ltx, mut lrx) = tokio::sync::mpsc::unbounded_channel();
        let label = clank_core::ids::AgentLabel::parse("claude").unwrap();
        let tx = crate::cli::event_source::EmissionSink::for_author(etx, label.clone())
            .reporting(ltx, label, 1);
        let s = timed_src(&[GithubEventKind::IssueOpened], false);
        let sub =
            transports.subscribe("o/r", &webhook_event_names(&[GithubEventKind::IssueOpened]));
        let run = spawn_watch(
            s,
            f.clone(),
            std::sync::Arc::new(FixedLogin("me")),
            std::sync::Arc::new(NoCi),
            sub,
            None,
            tx,
        );
        settle().await;
        let (_, _, _, state) = lrx.try_recv().expect("the watch reports it");
        let crate::cli::event_source::Lifecycle::Degraded { why, polling } = state else {
            panic!("{state:?}");
        };
        assert!(
            why.contains("another forwarder holds the webhook slot for o/r"),
            "{why}"
        );
        assert!(!why.contains("admin"), "{why}");
        assert!(polling);
        tokio::time::sleep(Duration::from_secs(300) + Duration::from_millis(1)).await;
        assert_eq!(feed_reads(&f), 5, "the timer polls through the conflict");
        assert_eq!(fake.started(), 2, "one transport retry, minutes apart");
        run.abort();
    }

    /// A child that stays alive but cannot hand deliveries over is
    /// restarted, and its restart catches up.
    #[tokio::test(start_paused = true)]
    async fn a_forwarding_error_restarts_the_relay_and_catches_up() {
        let fake = FakeForwarder::default();
        let transports = Transports::new(fake.clone());
        let f = std::sync::Arc::new(ScriptFetcher::new(vec![]));
        let (tx, _rx) = sink();
        let sub =
            transports.subscribe("o/r", &webhook_event_names(&[GithubEventKind::IssueOpened]));
        let run = spawn_watch(
            src(&[GithubEventKind::IssueOpened], false),
            f.clone(),
            std::sync::Arc::new(FixedLogin("me")),
            std::sync::Arc::new(NoCi),
            sub,
            None,
            tx,
        );
        settle().await;
        assert_eq!(feed_reads(&f), 1);
        fake.say(&format!(
            "{FORWARDING_ERROR} Post \"http://127.0.0.1:1\": connection refused"
        ));
        settle().await;
        tokio::time::sleep(Duration::from_secs(5)).await;
        settle().await;
        assert_eq!(fake.started(), 2);
        assert_eq!(feed_reads(&f), 2);
        run.abort();
    }

    /// A login that answers only when the test says so.
    struct HeldLogin(tokio::sync::Notify);
    impl LoginResolver for HeldLogin {
        async fn resolve(&self) -> anyhow::Result<String> {
            self.0.notified().await;
            Ok("me".into())
        }
    }

    /// Deliveries the intake had to evict are owed back: a reconcile
    /// runs for them and the event log says the relay overflowed.
    #[tokio::test(start_paused = true)]
    async fn an_intake_overflow_owes_a_reconcile_and_is_logged() {
        let dir = wal_tempdir();
        let f = std::sync::Arc::new(ScriptFetcher::new(vec![]));
        let login = std::sync::Arc::new(HeldLogin(tokio::sync::Notify::new()));
        let relay = inbox();
        relay.started();
        let (tx, _rx) = sink();
        let run = spawn_watch(
            src(&[GithubEventKind::IssueOpened], false),
            f.clone(),
            login.clone(),
            std::sync::Arc::new(NoCi),
            Subscription::detached(relay.clone()),
            Some(open_log(&dir)),
            tx,
        );
        settle().await;
        for n in 0..(RELAY_BUFFER_CAP as u64 + 3) {
            relay.push(webhook_issue(n));
        }
        login.0.notify_one();
        settle().await;
        assert_eq!(feed_reads(&f), 2, "the catch-up, then the owed reconcile");
        let text = std::fs::read_to_string(dir.join("t.jsonl")).unwrap();
        assert!(
            text.contains(r#""t":"relay_overflow""#) && text.contains(r#""dropped":3"#),
            "{text}"
        );
        run.abort();
    }

    /// Cold start: an action delivered live — before the baseline or
    /// after it — that the baseline page also holds wakes exactly
    /// once; the page's pre-watch history does not.
    #[tokio::test(start_paused = true)]
    async fn a_live_delivery_on_the_baseline_page_wakes_once() {
        let dir = wal_tempdir();
        let f = std::sync::Arc::new(ScriptFetcher::new(vec![Box::new(|_p, _e| {
            Ok(ok(
                vec![
                    feed_issue("105", 5),
                    feed_issue("104", 4),
                    feed_issue("103", 3),
                ],
                None,
                None,
            ))
        })]));
        let relay = inbox();
        relay.started();
        relay.push(webhook_issue(5));
        let (tx, mut rx) = sink();
        let run = spawn_watch(
            src(&[GithubEventKind::IssueOpened], false),
            f.clone(),
            std::sync::Arc::new(FixedLogin("me")),
            std::sync::Arc::new(NoCi),
            Subscription::detached(relay.clone()),
            Some(open_log(&dir)),
            tx,
        );
        settle().await;
        relay.push(webhook_issue(4));
        relay.push(webhook_issue(5));
        settle().await;
        let mut woke = woke_numbers(&mut rx);
        woke.sort();
        assert_eq!(woke, vec![4, 5], "each live action once, history (3) never");
        let rows = open_log(&dir).read_rows().unwrap().unwrap().rows;
        let pending: Vec<_> = rows
            .iter()
            .filter(|r| !r.baseline && !r.acked)
            .filter_map(|r| match &r.item {
                WaitItem::GithubEvent { number, .. } => *number,
                _ => None,
            })
            .collect();
        assert_eq!(
            pending.len(),
            2,
            "both live ones are pending in the log: {pending:?}"
        );
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
            listening(),
            Some(open_log(&dir)),
            tx,
        );
        settle().await;
        assert_eq!(woke_numbers(&mut rx), vec![2]);
        run.abort();
    }

    /// One catch-up is one logical reconcile: a failed attempt — the
    /// login, the network, or half a page — is retried with no webhook
    /// arriving, re-reads from the committed cursor, and stops once it
    /// completes.
    #[tokio::test(start_paused = true)]
    async fn a_failed_catch_up_is_retried_until_it_completes() {
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
            listening(),
            Some(open_log(&dir)),
            tx,
        );
        tokio::time::sleep(Duration::from_secs(120)).await;
        let mut woke = woke_numbers(&mut rx);
        woke.sort();
        assert_eq!(woke, vec![2, 3], "each once, through the committed cursor");
        let reads = feed_reads(&f);
        assert_eq!(
            reads, 3,
            "login, network and the half page each cost one retry"
        );
        tokio::time::sleep(DAY).await;
        assert_eq!(feed_reads(&f), reads, "and success ends the retrying");
        run.abort();
    }

    /// A rate limit's reset is honoured: the retry waits it out rather
    /// than backing off on its own, shorter, schedule.
    #[tokio::test(start_paused = true)]
    async fn a_rate_limited_catch_up_waits_for_the_reset() {
        let f = std::sync::Arc::new(ScriptFetcher::new(vec![Box::new(|_p, _e| {
            Err(crate::cli::github::session::RateLimited {
                url: "https://api.github.com/repos/o/r/events".into(),
                retry_in: Duration::from_secs(600),
            }
            .into())
        })]));
        let (tx, _rx) = sink();
        let run = spawn_watch(
            src(&[GithubEventKind::IssueOpened], false),
            f.clone(),
            std::sync::Arc::new(FixedLogin("me")),
            std::sync::Arc::new(NoCi),
            listening(),
            None,
            tx,
        );
        settle().await;
        tokio::time::sleep(Duration::from_secs(599)).await;
        assert_eq!(feed_reads(&f), 1, "not before the reset");
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(feed_reads(&f), 2);
        run.abort();
    }

    /// The last cue's read fails: it is read again until it succeeds,
    /// with no further cue, and the conclusion arrives.
    #[tokio::test(start_paused = true)]
    async fn a_failed_cue_read_is_retried_without_another_cue() {
        let ci = TestCi::answering(vec![
            Ok(vec![pending(7, "h1")]),
            Err("502"),
            Err("502"),
            Ok(vec![red(7, "h1", "test")]),
        ]);
        let relay = inbox();
        relay.started();
        let (tx, mut rx) = sink();
        let run = spawn_watch(
            src(&[GithubEventKind::CiFailed], true),
            std::sync::Arc::new(ScriptFetcher::new(vec![])),
            std::sync::Arc::new(FixedLogin("me")),
            ci.clone(),
            Subscription::detached(relay.clone()),
            None,
            tx,
        );
        settle().await;
        relay.push((
            "check_suite".into(),
            serde_json::json!({"action":"completed"}),
        ));
        tokio::time::sleep(Duration::from_secs(120)).await;
        assert_eq!(ci.reads(), 4);
        assert_eq!(woke_numbers(&mut rx), vec![7]);
        tokio::time::sleep(DAY).await;
        assert_eq!(ci.reads(), 4, "pending work was the only reading");
        run.abort();
    }

    /// A cue that lands while a read is in flight owes one more read.
    #[tokio::test(start_paused = true)]
    async fn a_cue_during_a_read_buys_one_more_read() {
        let ci = TestCi::answering(vec![
            Ok(vec![pending(7, "h1")]),
            Ok(vec![red(7, "h1", "lint")]),
        ]);
        let hold = std::sync::Arc::new(tokio::sync::Notify::new());
        *ci.hold.lock().unwrap() = Some(hold.clone());
        let relay = inbox();
        relay.started();
        let (tx, mut rx) = sink();
        let run = spawn_watch(
            src(&[GithubEventKind::CiFailed], true),
            std::sync::Arc::new(ScriptFetcher::new(vec![])),
            std::sync::Arc::new(FixedLogin("me")),
            ci.clone(),
            Subscription::detached(relay.clone()),
            None,
            tx,
        );
        settle().await;
        assert_eq!(ci.reads(), 1, "in flight");
        relay.push(("status".into(), serde_json::json!({"state":"failure"})));
        hold.notify_one();
        tokio::time::sleep(Duration::from_secs(60)).await;
        assert_eq!(ci.reads(), 2);
        assert_eq!(woke_numbers(&mut rx), vec![7]);
        run.abort();
    }

    /// N cues inside one rate window cost ONE trailing read, after the
    /// last of them, and it reports a conclusion that changed between
    /// the first cue and the last.
    #[tokio::test(start_paused = true)]
    async fn cues_inside_a_window_coalesce_into_one_trailing_read() {
        let ci = TestCi::answering(vec![
            Ok(vec![pending(7, "h1")]),
            Ok(vec![red(7, "h1", "test")]),
        ]);
        let relay = inbox();
        relay.started();
        let (tx, mut rx) = sink();
        let run = spawn_watch(
            src(&[GithubEventKind::CiFailed], true),
            std::sync::Arc::new(ScriptFetcher::new(vec![])),
            std::sync::Arc::new(FixedLogin("me")),
            ci.clone(),
            Subscription::detached(relay.clone()),
            None,
            tx,
        );
        settle().await;
        assert_eq!(ci.reads(), 1, "the catch-up");
        let start = tokio::time::Instant::now();
        let mut last_cue = start;
        for _ in 0..5 {
            tokio::time::sleep(Duration::from_secs(2)).await;
            relay.push((
                "check_suite".into(),
                serde_json::json!({"action":"completed"}),
            ));
            last_cue = tokio::time::Instant::now();
            settle().await;
        }
        assert_eq!(ci.reads(), 1, "nothing inside the window");
        tokio::time::sleep(Duration::from_secs(60)).await;
        let reads = ci.reads.lock().unwrap().clone();
        assert_eq!(reads.len(), 2, "five cues, one trailing read");
        assert!(
            reads[1] >= last_cue,
            "the trailing read follows the last cue"
        );
        assert_eq!(
            woke_numbers(&mut rx),
            vec![7],
            "the changed conclusion is reported"
        );
        run.abort();
    }

    /// The stated gap: an action the catch-up's feed read did not have
    /// yet, from before the hook, is never delivered without a timer,
    /// and is by the first timer poll with one.
    #[tokio::test(start_paused = true)]
    async fn an_action_lagging_in_the_feed_needs_the_timer() {
        for timed in [false, true] {
            let f = std::sync::Arc::new(ScriptFetcher::new(vec![
                Box::new(|_p, _e| Ok(ok(vec![feed_issue("1", 1)], None, None))),
                Box::new(|_p, _e| Ok(ok(vec![], None, None))),
                Box::new(|_p, _e| Ok(ok(vec![feed_issue("2", 2), feed_issue("1", 1)], None, None))),
            ]));
            let s = match timed {
                true => timed_src(&[GithubEventKind::IssueOpened], false),
                false => src(&[GithubEventKind::IssueOpened], false),
            };
            let (tx, mut rx) = sink();
            let run = spawn_watch(
                s,
                f,
                std::sync::Arc::new(FixedLogin("me")),
                std::sync::Arc::new(NoCi),
                listening(),
                None,
                tx,
            );
            settle().await;
            tokio::time::sleep(Duration::from_secs(61)).await;
            let woke = woke_numbers(&mut rx);
            match timed {
                false => {
                    tokio::time::sleep(DAY).await;
                    assert!(
                        woke.is_empty() && woke_numbers(&mut rx).is_empty(),
                        "the stated gap"
                    );
                }
                true => assert_eq!(woke, vec![2], "the first timer poll closes it"),
            }
            run.abort();
        }
    }

    /// A relay that cannot start, with no timer: the diagnostic names
    /// the cause, nothing is read, and the retry that succeeds catches
    /// up once. With a timer, polling carries on meanwhile.
    #[tokio::test(start_paused = true)]
    async fn a_relay_that_cannot_start_is_loud_and_reads_only_on_a_timer() {
        for timed in [false, true] {
            let f = std::sync::Arc::new(ScriptFetcher::new(vec![]));
            let relay = inbox();
            let (etx, _erx) = tokio::sync::mpsc::unbounded_channel();
            let (ltx, mut lrx) = tokio::sync::mpsc::unbounded_channel();
            let label = clank_core::ids::AgentLabel::parse("claude").unwrap();
            let tx = crate::cli::event_source::EmissionSink::for_author(etx, label.clone())
                .reporting(ltx, label, 1);
            let s = match timed {
                true => timed_src(&[GithubEventKind::IssueOpened], false),
                false => src(&[GithubEventKind::IssueOpened], false),
            };
            let run = spawn_watch(
                s,
                f.clone(),
                std::sync::Arc::new(FixedLogin("me")),
                std::sync::Arc::new(NoCi),
                Subscription::detached(relay.clone()),
                None,
                tx,
            );
            relay.down(RelayDown {
                cause: RelayFailure::NoAdmin,
                retry_in: Duration::from_secs(300),
            });
            settle().await;
            let (_, _, _, state) = lrx.try_recv().expect("reported");
            assert_eq!(
                state,
                crate::cli::event_source::Lifecycle::Degraded {
                    why: "needs admin on o/r to create a webhook".into(),
                    polling: timed,
                }
            );
            tokio::time::sleep(Duration::from_secs(180) + Duration::from_millis(1)).await;
            assert_eq!(feed_reads(&f), if timed { 3 } else { 0 });
            relay.started();
            settle().await;
            assert_eq!(
                feed_reads(&f),
                if timed { 4 } else { 1 },
                "the successful retry catches up"
            );
            run.abort();
        }
    }

    /// The optional timer: N intervals give N polls on top of the
    /// catch-up.
    #[tokio::test(start_paused = true)]
    async fn a_poll_interval_adds_exactly_its_polls() {
        let f = std::sync::Arc::new(ScriptFetcher::new(vec![]));
        let (tx, _rx) = sink();
        let run = spawn_watch(
            timed_src(&[GithubEventKind::IssueOpened], false),
            f.clone(),
            std::sync::Arc::new(FixedLogin("me")),
            std::sync::Arc::new(NoCi),
            listening(),
            None,
            tx,
        );
        settle().await;
        tokio::time::sleep(Duration::from_secs(4 * 60) + Duration::from_millis(1)).await;
        assert_eq!(feed_reads(&f), 1 + 4);
        run.abort();
    }

    /// A final status cue arrives during a held checks read and is
    /// followed by a full inbox of issues: the cue is not lost to the
    /// eviction, the red build wakes this untimed watch, and once the
    /// trailing read succeeds nothing more is read (codex 141f85d).
    #[tokio::test(start_paused = true)]
    async fn a_ci_cue_survives_an_unrelated_action_overflow() {
        let ci = TestCi::answering(vec![
            Ok(vec![pending(7, "h1")]),
            Ok(vec![pending(7, "h1")]),
            Ok(vec![red(7, "h1", "test")]),
        ]);
        let relay = inbox();
        relay.started();
        let (tx, mut rx) = sink();
        let run = spawn_watch(
            src(
                &[GithubEventKind::CiFailed, GithubEventKind::IssueOpened],
                true,
            ),
            std::sync::Arc::new(ScriptFetcher::new(vec![])),
            std::sync::Arc::new(FixedLogin("me")),
            ci.clone(),
            Subscription::detached(relay.clone()),
            None,
            tx,
        );
        settle().await;
        let hold = std::sync::Arc::new(tokio::sync::Notify::new());
        *ci.hold.lock().unwrap() = Some(hold.clone());
        relay.push(("status".into(), serde_json::json!({"state":"pending"})));
        tokio::time::sleep(Duration::from_secs(16)).await;
        assert_eq!(ci.reads(), 2, "the held read");
        relay.push(("status".into(), serde_json::json!({"state":"failure"})));
        for n in 0..RELAY_BUFFER_CAP as u64 {
            relay.push(webhook_issue(n));
        }
        hold.notify_one();
        tokio::time::sleep(Duration::from_secs(120)).await;
        assert_eq!(ci.reads(), 3, "the cue still owes its checks read");
        assert!(woke_numbers(&mut rx).contains(&7));
        tokio::time::sleep(DAY).await;
        assert_eq!(ci.reads(), 3, "and success returns the watch to idle");
        run.abort();
    }

    /// A mixed watch whose own-login lookup never succeeds: its feed
    /// stays fail-closed, but the checks need no login and their cues
    /// are not held behind it (codex 141f85d).
    #[tokio::test(start_paused = true)]
    async fn ci_cues_do_not_wait_on_the_own_login() {
        let ci = TestCi::answering(vec![
            Ok(vec![pending(7, "h1")]),
            Ok(vec![red(7, "h1", "test")]),
        ]);
        struct NoLogin;
        impl LoginResolver for NoLogin {
            async fn resolve(&self) -> anyhow::Result<String> {
                anyhow::bail!("own login unavailable")
            }
        }
        let f = std::sync::Arc::new(ScriptFetcher::new(vec![]));
        let relay = inbox();
        relay.started();
        let (tx, mut rx) = sink();
        let run = spawn_watch(
            src(
                &[GithubEventKind::CiFailed, GithubEventKind::IssueOpened],
                false,
            ),
            f.clone(),
            std::sync::Arc::new(NoLogin),
            ci.clone(),
            Subscription::detached(relay.clone()),
            None,
            tx,
        );
        settle().await;
        assert_eq!(ci.reads(), 1);
        relay.push(webhook_issue(3));
        relay.push(("status".into(), serde_json::json!({"state":"failure"})));
        tokio::time::sleep(Duration::from_secs(120)).await;
        assert_eq!(
            ci.reads(),
            2,
            "a failed own-login lookup cannot suppress a CI cue"
        );
        assert_eq!(
            woke_numbers(&mut rx),
            vec![7],
            "and the issue stays fail-closed"
        );
        assert_eq!(feed_reads(&f), 0, "the feed is never read unfiltered");
        tokio::time::sleep(DAY).await;
        assert_eq!(ci.reads(), 2, "the checks return to idle");
        run.abort();
    }

    /// The consumer is busy in a read while the transport proves a
    /// start and then fails: the watch ends up reporting the failure,
    /// never the stale start, and still runs the start's catch-up
    /// (codex 141f85d).
    #[tokio::test(start_paused = true)]
    async fn a_start_then_a_failure_during_a_read_reports_the_failure() {
        use crate::cli::event_source::Lifecycle;
        let ci = TestCi::answering(vec![]);
        let hold = std::sync::Arc::new(tokio::sync::Notify::new());
        *ci.hold.lock().unwrap() = Some(hold.clone());
        let f = std::sync::Arc::new(ScriptFetcher::new(vec![]));
        let relay = inbox();
        relay.started();
        let (etx, _erx) = tokio::sync::mpsc::unbounded_channel();
        let (ltx, mut lrx) = tokio::sync::mpsc::unbounded_channel();
        let label = clank_core::ids::AgentLabel::parse("claude").unwrap();
        let tx = crate::cli::event_source::EmissionSink::for_author(etx, label.clone())
            .reporting(ltx, label, 1);
        let run = spawn_watch(
            src(
                &[GithubEventKind::CiFailed, GithubEventKind::IssueOpened],
                true,
            ),
            f.clone(),
            std::sync::Arc::new(FixedLogin("me")),
            ci.clone(),
            Subscription::detached(relay.clone()),
            None,
            tx,
        );
        settle().await;
        assert_eq!(ci.reads(), 1, "busy in the first catch-up's checks read");
        relay.started();
        relay.down(RelayDown {
            cause: RelayFailure::SlotConflict,
            retry_in: Duration::from_secs(120),
        });
        hold.notify_one();
        tokio::time::sleep(Duration::from_secs(60)).await;
        let mut last = None;
        while let Ok((_, _, _, state)) = lrx.try_recv() {
            last = Some(state);
        }
        assert!(
            matches!(last, Some(Lifecycle::Degraded { polling: false, .. })),
            "current health is down: {last:?}"
        );
        assert_eq!(
            (feed_reads(&f), ci.reads()),
            (2, 2),
            "the queued start still owed its catch-up"
        );
        run.abort();
    }

    /// Lost deliveries owe the checks a read too: whatever went might
    /// have been a reason to look.
    #[tokio::test(start_paused = true)]
    async fn an_overflow_owes_the_checks_a_read_as_well() {
        let ci = TestCi::answering(vec![]);
        let hold = std::sync::Arc::new(tokio::sync::Notify::new());
        *ci.hold.lock().unwrap() = Some(hold.clone());
        let relay = inbox();
        relay.started();
        let (tx, _rx) = sink();
        let run = spawn_watch(
            src(
                &[GithubEventKind::CiFailed, GithubEventKind::IssueOpened],
                true,
            ),
            std::sync::Arc::new(ScriptFetcher::new(vec![])),
            std::sync::Arc::new(FixedLogin("me")),
            ci.clone(),
            Subscription::detached(relay.clone()),
            None,
            tx,
        );
        settle().await;
        for n in 0..(RELAY_BUFFER_CAP as u64 + 1) {
            relay.push(webhook_issue(n));
        }
        hold.notify_one();
        tokio::time::sleep(Duration::from_secs(60)).await;
        assert_eq!(ci.reads(), 2);
        tokio::time::sleep(DAY).await;
        assert_eq!(ci.reads(), 2);
        run.abort();
    }
}
