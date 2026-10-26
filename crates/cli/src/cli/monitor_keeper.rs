//! The status TUI's monitor keeper.
//!
//! A monitor retires itself when the installed build changes. If only a
//! parked wait started the next one, a TUI open on a repo whose agents
//! were all idle would watch for itself indefinitely, and the monitor's
//! typed-nudge backstop — which only a running monitor can fire — would
//! never run. The keeper is the TUI's one observer
//! subscription and the lifecycle that brings it back: one attempt at a
//! time through the established attach-or-spawn path, a backoff while
//! the repo is unavailable, and a local watcher for the gap.
//!
//! Plain `clank status` and `status --watch` do not use it: they stay
//! read-only through [`super::core_wakes`].

use super::{
    Attached, Attempt, BEAT_GRACE, Conn, SUBSCRIBE_POLL, Started, Who, attempt_with, beat_overdue,
    reconcile_after, subscribe,
};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

/// The wait after the first unavailable attempt.
pub(crate) const FIRST_RETRY: Duration = Duration::from_secs(5);

/// The longest wait between attempts.
pub(crate) const RETRY_CAP: Duration = Duration::from_secs(300);

/// Where the keeper is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    /// Subscribed to the monitor's fold.
    Shared,
    /// One attempt is out; `next` is the wait if it fails.
    Attempting { next: Duration },
    /// Unavailable, and the next reclassification is at `at`.
    Waiting { at: Instant, next: Duration },
}

impl Phase {
    /// When the keeper must look again with nothing else to wake it.
    pub(crate) fn due(self) -> Option<Instant> {
        match self {
            Phase::Waiting { at, .. } => Some(at),
            Phase::Shared | Phase::Attempting { .. } => None,
        }
    }
}

/// What the keeper heard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Event {
    /// The fold moved, or the local watcher saw the repo change.
    Wake,
    /// The subscription's connection ended.
    Lost,
    /// Time passed with nothing to hear.
    Quiet,
    /// The attempt came back, attached or not.
    Settled { attached: bool },
}

/// What the keeper must do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Effect {
    /// Pass a wake on to the panel.
    Forward,
    /// Watch the repo locally, and reconcile what the watcher baselines.
    WatchLocally,
    /// Make one attach-or-spawn attempt.
    Attempt,
    /// Release the local watcher and owe the panel one fresh rebuild.
    Handoff,
}

/// The keeper's first phase, from whether its first subscribe attached.
pub(crate) fn start(attached: bool) -> (Phase, Vec<Effect>) {
    if attached {
        (Phase::Shared, Vec::new())
    } else {
        lose()
    }
}

fn lose() -> (Phase, Vec<Effect>) {
    (
        Phase::Attempting { next: FIRST_RETRY },
        vec![Effect::WatchLocally, Effect::Attempt],
    )
}

/// The keeper's transition on `event` at `now`.
///
/// A wake only ever forwards. A refresh, a keypress or a repo edit is
/// not evidence that the repo has become attachable, and an attempt on
/// each would turn a busy working tree into a fork per edit against a
/// repairer, a held lease or a newer peer.
///
/// The retry deadline is read off `now`, not off the event: whatever was
/// heard, a backoff that has expired leaves `Waiting` with exactly one
/// attempt, so a repo that never goes quiet cannot starve it.
pub(crate) fn step(phase: Phase, event: Event, now: Instant) -> (Phase, Vec<Effect>) {
    let (phase, mut effects) = react(phase, event, now);
    match phase {
        Phase::Waiting { at, next } if at <= now => {
            effects.push(Effect::Attempt);
            (Phase::Attempting { next }, effects)
        }
        phase => (phase, effects),
    }
}

fn react(phase: Phase, event: Event, now: Instant) -> (Phase, Vec<Effect>) {
    match (phase, event) {
        (_, Event::Wake) => (phase, vec![Effect::Forward]),
        (Phase::Shared, Event::Lost) => lose(),
        (Phase::Attempting { .. }, Event::Settled { attached: true }) => {
            (Phase::Shared, vec![Effect::Handoff])
        }
        (Phase::Attempting { next }, Event::Settled { attached: false }) => (
            Phase::Waiting {
                at: now + next,
                next: (next * 2).min(RETRY_CAP),
            },
            Vec::new(),
        ),
        (phase, _) => (phase, Vec::new()),
    }
}

/// What the keeper drives: its subscription, its attempts, its local
/// watcher and the panel it wakes.
pub(crate) trait Port {
    fn now(&self) -> Instant;
    /// Whether the keeper is to stop.
    fn stopped(&self) -> bool;
    /// The next thing to hear, waiting no later than `until`, or `None`
    /// once the keeper is to stop.
    fn hear(&mut self, until: Option<Instant>) -> Option<Event>;
    /// Make one attempt, returning its subscription if it attached.
    fn attempt(&mut self) -> Option<Conn>;
    /// Make `conn` the keeper's subscription.
    fn adopt(&mut self, conn: Conn);
    fn forward(&mut self);
    fn watch_locally(&mut self);
    fn handoff(&mut self);
}

/// Run the keeper until its port says stop, starting from whether the
/// first subscribe attached.
pub(crate) fn drive(port: &mut impl Port, attached: bool) {
    let (mut phase, effects) = start(attached);
    let mut pending: std::collections::VecDeque<Effect> = effects.into();
    loop {
        while let Some(effect) = pending.pop_front() {
            // A stop can land before the thread first runs or between
            // hearing and acting; either way no new watcher or attempt
            // may follow it.
            if port.stopped() {
                return;
            }
            match effect {
                Effect::Forward => port.forward(),
                Effect::WatchLocally => port.watch_locally(),
                Effect::Handoff => port.handoff(),
                Effect::Attempt => {
                    let conn = port.attempt();
                    // Cancelled in flight: the connection is dropped
                    // unadopted, and a monitor the attempt started stays
                    // with its own reaper.
                    if port.stopped() {
                        return;
                    }
                    let attached = conn.is_some();
                    if let Some(conn) = conn {
                        port.adopt(conn);
                    }
                    let (next, more) = step(phase, Event::Settled { attached }, port.now());
                    phase = next;
                    pending.extend(more);
                }
            }
        }
        let Some(event) = port.hear(phase.due()) else {
            return;
        };
        let (next, more) = step(phase, event, port.now());
        phase = next;
        pending.extend(more);
    }
}

/// How many fresh rebuilds the keeper has owed the panel.
///
/// A count rather than a flag, so a handoff that lands while the panel
/// is rebuilding for an earlier one is not cleared by that rebuild.
#[derive(Debug, Clone, Default)]
pub(crate) struct FreshRebuild(Arc<AtomicU64>);

impl FreshRebuild {
    pub(crate) fn owe(&self) {
        self.0.fetch_add(1, Ordering::AcqRel);
    }

    /// How many have been owed so far.
    pub(crate) fn owed(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }
}

/// The production port: a monitor connection or a local watcher of type
/// `W`, made by `watch`, and attempts made by `attempt`.
pub(crate) struct Live<A, M, W> {
    attempt: A,
    watch: M,
    sub: Option<(Conn, Instant)>,
    local: Option<W>,
    local_tx: mpsc::Sender<()>,
    local_rx: mpsc::Receiver<()>,
    wake: mpsc::Sender<()>,
    fresh: FreshRebuild,
    stop: Arc<AtomicBool>,
    closed: bool,
}

impl<A, M, W> Live<A, M, W>
where
    A: FnMut() -> Attempt,
    M: FnMut(mpsc::Sender<()>) -> anyhow::Result<W>,
{
    pub(crate) fn new(
        attempt: A,
        watch: M,
        sub: Option<Conn>,
        wake: mpsc::Sender<()>,
        fresh: FreshRebuild,
        stop: Arc<AtomicBool>,
    ) -> Self {
        let (local_tx, local_rx) = mpsc::channel();
        Self {
            attempt,
            watch,
            sub: sub.map(|conn| (conn, Instant::now())),
            local: None,
            local_tx,
            local_rx,
            wake,
            fresh,
            stop,
            closed: false,
        }
    }
}

impl<A, M, W> Port for Live<A, M, W>
where
    A: FnMut() -> Attempt,
    M: FnMut(mpsc::Sender<()>) -> anyhow::Result<W>,
{
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn stopped(&self) -> bool {
        self.closed || self.stop.load(Ordering::Acquire)
    }

    fn hear(&mut self, until: Option<Instant>) -> Option<Event> {
        if self.stopped() {
            return None;
        }
        if let Some((conn, last)) = &mut self.sub {
            // Polled rather than blocked on so a stop is noticed; a
            // lapsed poll is not loss, silence past the grace is.
            return match conn.note(Instant::now() + SUBSCRIBE_POLL) {
                Ok(_) => {
                    *last = Instant::now();
                    Some(Event::Wake)
                }
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                    if !beat_overdue(last.elapsed(), BEAT_GRACE) {
                        return Some(Event::Quiet);
                    }
                    eprintln!("clank: the monitor went quiet; watching this repo directly");
                    self.sub = None;
                    Some(Event::Lost)
                }
                Err(_) => {
                    self.sub = None;
                    Some(Event::Lost)
                }
            };
        }
        let wait = until.map_or(SUBSCRIBE_POLL, |at| {
            at.saturating_duration_since(Instant::now())
                .min(SUBSCRIBE_POLL)
        });
        match self.local_rx.recv_timeout(wait) {
            Ok(()) => Some(Event::Wake),
            Err(_) => Some(Event::Quiet),
        }
    }

    fn attempt(&mut self) -> Option<Conn> {
        match (self.attempt)() {
            Attempt::Shared(conn) => Some(conn),
            unavailable => {
                tracing::debug!(
                    "the monitor keeper did not attach: {}",
                    unavailable.why().unwrap_or_default()
                );
                None
            }
        }
    }

    fn adopt(&mut self, conn: Conn) {
        self.sub = Some((conn, Instant::now()));
    }

    fn forward(&mut self) {
        if self.wake.send(()).is_err() {
            self.closed = true;
        }
    }

    fn watch_locally(&mut self) {
        self.local = reconcile_after(&self.wake, (self.watch)(self.local_tx.clone()));
    }

    fn handoff(&mut self) {
        self.local = None;
        // Owed before the wake, so the refresh it raises finds it.
        self.fresh.owe();
        self.forward();
    }
}

/// The TUI's monitor keeper, on its own thread. Dropping it stops the
/// thread's retry work and releases its watcher; the monitor itself is
/// independent and never signalled.
pub(crate) struct MonitorKeeper {
    stop: Arc<AtomicBool>,
    fresh: FreshRebuild,
}

impl MonitorKeeper {
    /// Subscribe to `repo`'s fold as an observer, waking `wake` on every
    /// change, and keep a monitor up for it through `spawn` while the
    /// keeper lives.
    pub(crate) fn start(
        repo: &std::path::Path,
        wake: mpsc::Sender<()>,
        spawn: impl Fn(&std::path::Path) -> std::io::Result<Box<dyn Started>> + Send + 'static,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let fresh = FreshRebuild::default();
        let first = match subscribe(repo) {
            Attached::Shared(conn) => Some(conn),
            Attached::Alone(why) => {
                tracing::debug!("{} has no monitor to share yet: {why}", repo.display());
                None
            }
        };
        let repo = repo.to_path_buf();
        {
            let (stop, fresh) = (stop.clone(), fresh.clone());
            std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build();
                let attached = first.is_some();
                let watched = repo.clone();
                let mut live = Live::new(
                    || match &runtime {
                        Ok(runtime) => runtime.block_on(attempt_with(&repo, Who::Observer, &spawn)),
                        Err(e) => Attempt::Unproven(format!("no runtime to attach on: {e}")),
                    },
                    |tx| crate::repo_watch::RepoStateWatcher::attach(&watched, false, tx),
                    first,
                    wake,
                    fresh,
                    stop,
                );
                drive(&mut live, attached);
            });
        }
        Self { stop, fresh }
    }

    /// The fresh rebuilds this keeper owes the panel.
    pub(crate) fn fresh(&self) -> FreshRebuild {
        self.fresh.clone()
    }
}

impl Drop for MonitorKeeper {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(t0: Instant, secs: u64) -> Instant {
        t0 + Duration::from_secs(secs)
    }

    /// Unavailable outcomes back off 5s, doubling, to a 5 minute cap, and
    /// a successful attach starts the next loss from 5s again.
    #[test]
    fn the_backoff_doubles_to_its_cap_and_resets_on_attach() {
        let t0 = Instant::now();
        let (mut phase, effects) = start(false);
        assert_eq!(effects, [Effect::WatchLocally, Effect::Attempt]);
        let mut now = t0;
        let mut waits = Vec::new();
        for _ in 0..8 {
            (phase, _) = step(phase, Event::Settled { attached: false }, now);
            let Phase::Waiting { at, .. } = phase else {
                panic!("{phase:?}");
            };
            waits.push((at - now).as_secs());
            now = at;
            let effects;
            (phase, effects) = step(phase, Event::Quiet, now);
            assert_eq!(effects, [Effect::Attempt]);
        }
        assert_eq!(waits, [5, 10, 20, 40, 80, 160, 300, 300]);

        (phase, _) = step(phase, Event::Settled { attached: true }, now);
        (phase, _) = step(phase, Event::Lost, now);
        (phase, _) = step(phase, Event::Settled { attached: false }, now);
        assert_eq!(phase.due(), Some(now + FIRST_RETRY));
    }

    /// A wake before the backoff is due — a refresh, a key, a beat, a repo
    /// edit — forwards and starts nothing, however many arrive; only the
    /// due time does.
    #[test]
    fn wakes_during_a_backoff_coalesce_into_nothing() {
        let t0 = Instant::now();
        let (phase, _) = start(false);
        let (mut phase, _) = step(phase, Event::Settled { attached: false }, t0);
        for secs in 0..5 {
            let effects;
            (phase, effects) = step(phase, Event::Wake, at(t0, secs));
            assert_eq!(effects, [Effect::Forward]);
            let effects;
            (phase, effects) = step(phase, Event::Quiet, at(t0, secs));
            assert!(effects.is_empty(), "{effects:?}");
        }
        let (phase, effects) = step(phase, Event::Quiet, at(t0, 5));
        assert_eq!(effects, [Effect::Attempt]);
        let (_, effects) = step(phase, Event::Wake, at(t0, 5));
        assert_eq!(effects, [Effect::Forward], "nor while the attempt is out");
    }

    /// Wakes that keep arriving through an expired backoff reclassify
    /// once per due step, on the first wake at or past it, and never
    /// once per wake.
    #[test]
    fn a_due_retry_runs_through_sustained_wakes() {
        let mut port = Scripted::new(
            (0..20)
                .map(|_| Step::After(Duration::from_secs(10), Event::Wake))
                .collect(),
            outcomes(vec![]),
        );
        drive(&mut port, false);
        let t0 = port.attempts[0];
        assert_eq!(
            port.attempts,
            [0, 10, 20, 40, 80, 160].map(|secs| at(t0, secs)),
        );
        assert_eq!(port.forwarded, 20);
    }

    enum Step {
        Hear(Event),
        /// `event`, once this long has passed.
        After(Duration, Event),
        /// Nothing until the backoff is due.
        Idle,
        Do(Box<dyn FnOnce()>),
    }

    /// A port that plays a script on a virtual clock and records what the
    /// keeper did.
    struct Scripted<A: FnMut() -> Attempt> {
        now: Instant,
        script: std::collections::VecDeque<Step>,
        attempt: A,
        attempts: Vec<Instant>,
        forwarded: usize,
        local: bool,
        watched: usize,
        handoffs: usize,
        shared: Option<Conn>,
    }

    impl<A: FnMut() -> Attempt> Scripted<A> {
        fn new(script: Vec<Step>, attempt: A) -> Self {
            Self {
                now: Instant::now(),
                script: script.into(),
                attempt,
                attempts: Vec::new(),
                forwarded: 0,
                local: false,
                watched: 0,
                handoffs: 0,
                shared: None,
            }
        }
    }

    impl<A: FnMut() -> Attempt> Port for Scripted<A> {
        fn now(&self) -> Instant {
            self.now
        }

        fn stopped(&self) -> bool {
            false
        }

        fn hear(&mut self, until: Option<Instant>) -> Option<Event> {
            loop {
                match self.script.pop_front()? {
                    Step::Hear(Event::Lost) => {
                        self.shared = None;
                        return Some(Event::Lost);
                    }
                    Step::Hear(event) => return Some(event),
                    Step::After(wait, event) => {
                        self.now += wait;
                        return Some(event);
                    }
                    Step::Idle => {
                        if let Some(until) = until {
                            self.now = self.now.max(until);
                        }
                        return Some(Event::Quiet);
                    }
                    Step::Do(f) => f(),
                }
            }
        }

        fn attempt(&mut self) -> Option<Conn> {
            self.attempts.push(self.now);
            match (self.attempt)() {
                Attempt::Shared(conn) => Some(conn),
                _ => None,
            }
        }

        fn adopt(&mut self, conn: Conn) {
            self.shared = Some(conn);
        }

        fn forward(&mut self) {
            self.forwarded += 1;
        }

        fn watch_locally(&mut self) {
            self.local = true;
            self.watched += 1;
        }

        fn handoff(&mut self) {
            self.local = false;
            self.handoffs += 1;
        }
    }

    fn conn() -> Conn {
        let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        std::mem::forget(theirs);
        Conn::over(ours)
    }

    /// Outcomes to hand out in order, then `Unproven` for any extra.
    fn outcomes(list: Vec<fn() -> Attempt>) -> impl FnMut() -> Attempt {
        let mut list: std::collections::VecDeque<_> = list.into();
        move || {
            list.pop_front()
                .map_or_else(|| Attempt::Unproven("spent".into()), |f| f())
        }
    }

    /// A shared subscription's loss makes exactly one attempt, with no
    /// repo edit to prompt it, and the successor replaces the subscription.
    #[test]
    fn a_lost_monitor_is_succeeded_once_with_no_repo_edit() {
        let mut port = Scripted::new(
            vec![Step::Hear(Event::Wake), Step::Hear(Event::Lost), Step::Idle],
            outcomes(vec![|| Attempt::Shared(conn())]),
        );
        port.shared = Some(conn());
        drive(&mut port, true);
        assert_eq!(port.attempts.len(), 1);
        assert!(port.shared.is_some(), "the successor is the subscription");
        assert!(!port.local);
        assert_eq!(port.handoffs, 1);
    }

    /// A failed spawn or attach recovers at the next backoff step, with
    /// nothing else to wake it.
    #[test]
    fn a_failed_attempt_recovers_quietly_at_the_next_step() {
        let mut port = Scripted::new(
            vec![Step::Idle, Step::Idle],
            outcomes(vec![|| Attempt::SpawnFailed("no binary".into()), || {
                Attempt::Shared(conn())
            }]),
        );
        drive(&mut port, false);
        let t0 = port.attempts[0];
        assert_eq!(port.attempts, [t0, t0 + FIRST_RETRY]);
        assert!(port.shared.is_some() && !port.local);
    }

    /// Wakes of every kind while the backoff is pending start no attempt.
    #[test]
    fn signals_during_a_pending_backoff_start_nothing() {
        let mut script = vec![];
        for _ in 0..20 {
            script.push(Step::Hear(Event::Wake));
            script.push(Step::Hear(Event::Quiet));
        }
        let mut port = Scripted::new(script, outcomes(vec![|| Attempt::Busy("busy".into())]));
        drive(&mut port, false);
        assert_eq!(port.attempts.len(), 1, "only the startup attempt");
        assert_eq!(port.forwarded, 20, "and every wake reached the panel");
    }

    /// The local-to-shared handoff releases the local watcher once and
    /// owes the panel exactly one fresh rebuild.
    #[test]
    fn the_handoff_releases_the_watcher_and_owes_one_fresh_rebuild() {
        let (wake, woken) = mpsc::channel();
        let fresh = FreshRebuild::default();
        let watching = Arc::new(AtomicU64::new(0));
        struct Watcher(Arc<AtomicU64>);
        impl Drop for Watcher {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::AcqRel);
            }
        }
        let mut live = Live::new(
            outcomes(vec![|| Attempt::Shared(conn())]),
            {
                let watching = watching.clone();
                move |_| {
                    watching.fetch_add(1, Ordering::AcqRel);
                    Ok(Watcher(watching.clone()))
                }
            },
            None,
            wake,
            fresh.clone(),
            Arc::new(AtomicBool::new(false)),
        );
        let (mut phase, effects) = start(false);
        assert_eq!(effects, [Effect::WatchLocally, Effect::Attempt]);
        live.watch_locally();
        assert_eq!(watching.load(Ordering::Acquire), 1);
        assert_eq!(woken.try_iter().count(), 1, "the gap is reconciled");
        let conn = live.attempt().expect("it attaches");
        live.adopt(conn);
        let effects;
        (phase, effects) = step(phase, Event::Settled { attached: true }, Instant::now());
        assert_eq!(phase, Phase::Shared);
        assert_eq!(effects, [Effect::Handoff]);
        live.handoff();
        assert_eq!(
            watching.load(Ordering::Acquire),
            0,
            "the watcher is released"
        );
        assert_eq!(fresh.owed(), 1);
        assert_eq!(
            woken.try_iter().count(),
            1,
            "and the panel is woken to rebuild"
        );
    }

    struct Watcher(Arc<AtomicU64>);

    impl Drop for Watcher {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::AcqRel);
        }
    }

    /// A watcher factory whose live watchers are counted in the result,
    /// alongside how many it ever made.
    fn counted_watcher() -> (
        impl FnMut(mpsc::Sender<()>) -> anyhow::Result<Watcher>,
        Arc<AtomicU64>,
        Arc<AtomicU64>,
    ) {
        let (live, made) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
        let factory = {
            let (live, made) = (live.clone(), made.clone());
            move |_| {
                made.fetch_add(1, Ordering::AcqRel);
                live.fetch_add(1, Ordering::AcqRel);
                Ok(Watcher(live.clone()))
            }
        };
        (factory, live, made)
    }

    /// Cancelling during a backoff stops the keeper's loop and leaves
    /// nothing of it: neither subscription nor watcher.
    #[test]
    fn cancellation_stops_retry_work_and_releases_the_watcher() {
        let (wake, _woken) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let (watch, watching, _) = counted_watcher();
        let (attempted, attempts) = mpsc::channel();
        let live = Live::new(
            move || {
                let _ = attempted.send(());
                Attempt::NotStarted("lease held".into())
            },
            watch,
            None,
            wake,
            FreshRebuild::default(),
            stop.clone(),
        );
        let keeper = std::thread::spawn(move || {
            let mut live = live;
            drive(&mut live, false);
        });
        attempts.recv().unwrap();
        stop.store(true, Ordering::Release);
        let began = Instant::now();
        keeper.join().unwrap();
        assert!(began.elapsed() < FIRST_RETRY, "not held for the backoff");
        assert_eq!(
            watching.load(Ordering::Acquire),
            0,
            "the watcher went with it"
        );
        assert_eq!(attempts.try_iter().count(), 0, "and no attempt followed");
    }

    /// A keeper cancelled before its thread first runs creates no watcher
    /// and makes no attempt.
    #[test]
    fn a_keeper_stopped_before_it_runs_does_nothing() {
        let (wake, woken) = mpsc::channel();
        let (watch, _, made) = counted_watcher();
        let attempts = Arc::new(AtomicU64::new(0));
        let mut live = Live::new(
            {
                let attempts = attempts.clone();
                move || {
                    attempts.fetch_add(1, Ordering::AcqRel);
                    Attempt::Shared(conn())
                }
            },
            watch,
            None,
            wake,
            FreshRebuild::default(),
            Arc::new(AtomicBool::new(true)),
        );
        drive(&mut live, false);
        assert_eq!(attempts.load(Ordering::Acquire), 0);
        assert_eq!(made.load(Ordering::Acquire), 0);
        assert_eq!(woken.try_iter().count(), 0);
    }

    /// An attempt that attaches after the keeper was cancelled is
    /// discarded: its connection is closed, not adopted, and the panel is
    /// owed nothing for it.
    #[test]
    fn an_attempt_cancelled_in_flight_is_discarded() {
        let (wake, woken) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let (watch, watching, _) = counted_watcher();
        let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        let fresh = FreshRebuild::default();
        let mut live = Live::new(
            {
                let stop = stop.clone();
                let mut ours = Some(ours);
                move || {
                    stop.store(true, Ordering::Release);
                    Attempt::Shared(Conn::over(ours.take().expect("one attempt")))
                }
            },
            watch,
            None,
            wake,
            fresh.clone(),
            stop,
        );
        theirs
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        drive(&mut live, false);
        assert!(live.sub.is_none(), "not adopted");
        assert_eq!(
            std::io::Read::read(&mut &theirs, &mut [0u8; 1]).unwrap(),
            0,
            "its connection is closed"
        );
        assert_eq!(fresh.owed(), 0, "no handoff is published");
        assert_eq!(woken.try_iter().count(), 1, "only the watcher's reconcile");
        drop(live);
        assert_eq!(
            watching.load(Ordering::Acquire),
            0,
            "the watcher is released"
        );
    }

    /// A monitor of this build, listening in-process until dropped.
    struct FakeMonitor {
        _dir: tempfile::TempDir,
        heard: mpsc::Receiver<super::super::Hello>,
        gone: Arc<AtomicBool>,
    }

    impl Drop for FakeMonitor {
        fn drop(&mut self) {
            self.gone.store(true, Ordering::Release);
        }
    }

    /// Serve `repo` answering every greeting with `reply`, until dropped.
    fn fake_monitor(repo: &std::path::Path, pid: i32, reply: String) -> FakeMonitor {
        use std::io::{Read, Write};
        let dir = tempfile::tempdir().unwrap();
        let path = super::super::socket_path(dir.path(), pid).unwrap();
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        super::super::publish(
            repo,
            &super::super::Descriptor {
                proto: super::super::PROTO,
                pid,
                repo: repo.to_string_lossy().to_string(),
                socket: path.to_string_lossy().to_string(),
                build: None,
            },
        )
        .unwrap();
        let gone = Arc::new(AtomicBool::new(false));
        let (tell, heard) = mpsc::channel();
        {
            let gone = gone.clone();
            std::thread::spawn(move || {
                let mut answered = Vec::new();
                while !gone.load(Ordering::Acquire) {
                    let Ok((mut stream, _)) = listener.accept() else {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    };
                    stream.set_nonblocking(false).unwrap();
                    let mut line = Vec::new();
                    let mut byte = [0u8; 1];
                    while stream.read(&mut byte).unwrap_or(0) == 1 && byte[0] != b'\n' {
                        line.push(byte[0]);
                    }
                    if let Ok(hello) = serde_json::from_slice(&line) {
                        let _ = tell.send(hello);
                    }
                    let _ = stream.write_all(reply.as_bytes());
                    answered.push(stream);
                }
            });
        }
        FakeMonitor {
            _dir: dir,
            heard,
            gone,
        }
    }

    fn welcome() -> String {
        format!(
            "{}\n",
            serde_json::to_string(&super::super::Welcome::Ok {
                proto: super::super::PROTO
            })
            .unwrap()
        )
    }

    fn newer() -> String {
        format!("{{\"proto\":{}}}\n", super::super::PROTO + 1)
    }

    fn decline() -> String {
        format!(
            "{}\n",
            serde_json::to_string(&super::super::Welcome::Decline {
                proto: super::super::PROTO,
                why: "not this one".into(),
            })
            .unwrap()
        )
    }

    /// The keeper's real attempt, on test budgets: as an observer, through
    /// the established attach-or-spawn path, with `spawn` counted.
    fn observer_attempt(
        repo: &std::path::Path,
        spawn: impl Fn(&std::path::Path) -> std::io::Result<Box<dyn Started>>,
    ) -> Attempt {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(super::super::attempt_by(
                repo,
                Who::Observer,
                Duration::from_millis(300),
                Duration::from_millis(400),
                4,
                spawn,
                super::super::connect_within,
                |_| true,
            ))
    }

    /// A start that never happens.
    fn forbidden(_: &std::path::Path) -> std::io::Result<Box<dyn Started>> {
        Err(std::io::Error::other("nothing may be started here"))
    }

    struct NoChild;

    impl Started for NoChild {
        fn reap(self: Box<Self>) {}
    }

    /// Each way an attempt ends is its own typed variant, read off the
    /// classification rather than out of a sentence.
    #[test]
    fn each_classification_is_its_own_typed_outcome() {
        let repo = super::super::TempRepo::new();
        let peer = fake_monitor(repo.path(), 4601, newer());
        assert!(matches!(
            observer_attempt(repo.path(), forbidden),
            Attempt::Newer(_)
        ));
        drop(peer);

        let peer = fake_monitor(repo.path(), 4602, decline());
        assert!(matches!(
            observer_attempt(repo.path(), forbidden),
            Attempt::Theirs(_)
        ));
        drop(peer);

        let peer = fake_monitor(repo.path(), 4603, welcome());
        assert!(matches!(
            observer_attempt(repo.path(), forbidden),
            Attempt::Shared(_)
        ));
        drop(peer);

        let other = super::super::TempRepo::new();
        assert!(matches!(
            observer_attempt(other.path(), forbidden),
            Attempt::SpawnFailed(_)
        ));

        let busy = super::super::TempRepo::new();
        let _permit = match super::super::take_repair_permit(busy.path()) {
            super::super::Repair::Acquired(permit) => permit,
            _ => panic!("the gate is usable"),
        };
        assert!(matches!(
            observer_attempt(busy.path(), forbidden),
            Attempt::Busy(_)
        ));

        let held = super::super::TempRepo::new();
        let got = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(super::super::attempt_by(
                held.path(),
                Who::Observer,
                Duration::from_millis(100),
                Duration::from_millis(100),
                4,
                forbidden,
                super::super::connect_within,
                |_| false,
            ));
        assert!(matches!(got, Attempt::NotStarted(_)), "{got:?}");

        let quiet = super::super::TempRepo::new();
        assert!(matches!(
            observer_attempt(quiet.path(), |_| Ok(Box::new(NoChild) as Box<dyn Started>)),
            Attempt::Unattached(_)
        ));
    }

    /// Busy, newer, other and unproven peers are never a reason to start,
    /// across every backoff step, and an old client facing a newer monitor
    /// makes one attempt per step and no more.
    #[test]
    fn peers_never_cause_a_start() {
        let starts = Arc::new(AtomicU64::new(0));
        let counted = |starts: Arc<AtomicU64>| {
            move |_: &std::path::Path| -> std::io::Result<Box<dyn Started>> {
                starts.fetch_add(1, Ordering::AcqRel);
                Err(std::io::Error::other("counted"))
            }
        };
        type Peer = Box<dyn Fn(&std::path::Path) -> Box<dyn std::any::Any>>;
        let peers: [(&str, Peer); 4] = [
            (
                "newer",
                Box::new(|r| Box::new(fake_monitor(r, 4611, newer()))),
            ),
            (
                "theirs",
                Box::new(|r| Box::new(fake_monitor(r, 4612, decline()))),
            ),
            (
                "unproven",
                Box::new(|r| Box::new(fake_monitor(r, 4613, "not json\n".into()))),
            ),
            (
                "busy",
                Box::new(|r| Box::new(super::super::take_repair_permit(r))),
            ),
        ];
        for (name, peer) in peers {
            let repo = super::super::TempRepo::new();
            let _peer = peer(repo.path());
            let path = repo.path().to_path_buf();
            let spawn = counted(starts.clone());
            let mut port = Scripted::new(
                vec![Step::Idle, Step::Idle, Step::Idle, Step::Idle],
                move || observer_attempt(&path, &spawn),
            );
            drive(&mut port, false);
            assert_eq!(starts.load(Ordering::Acquire), 0, "{name}: nothing started");
            let t0 = port.attempts[0];
            assert_eq!(
                port.attempts,
                [
                    t0,
                    t0 + Duration::from_secs(5),
                    t0 + Duration::from_secs(15),
                    t0 + Duration::from_secs(35),
                    t0 + Duration::from_secs(75),
                ],
                "{name}: one reclassification per step"
            );
        }
    }

    /// A newer peer refuses the keeper and stays live: the reclassifications
    /// find it there and start nothing. Once it has exited and the lease is
    /// free, the next scheduled one starts exactly one monitor and attaches,
    /// with no repo edit in between.
    #[test]
    fn a_rejecting_peer_that_exits_is_succeeded_once_on_the_backoff() {
        let repo = super::super::TempRepo::new();
        let peer = Arc::new(std::sync::Mutex::new(Some(fake_monitor(
            repo.path(),
            4621,
            newer(),
        ))));
        let starts = Arc::new(AtomicU64::new(0));
        let successor = Arc::new(std::sync::Mutex::new(None));
        let spawn = {
            let (starts, successor) = (starts.clone(), successor.clone());
            move |r: &std::path::Path| -> std::io::Result<Box<dyn Started>> {
                starts.fetch_add(1, Ordering::AcqRel);
                *successor.lock().unwrap() = Some(fake_monitor(r, 4622, welcome()));
                Ok(Box::new(NoChild))
            }
        };
        let exits = {
            let peer = peer.clone();
            Box::new(move || {
                peer.lock().unwrap().take();
            })
        };
        let path = repo.path().to_path_buf();
        let mut port = Scripted::new(
            vec![
                Step::Idle,
                Step::Idle,
                Step::Idle,
                Step::Do(exits),
                Step::Idle,
                Step::Hear(Event::Wake),
                Step::Hear(Event::Quiet),
            ],
            move || observer_attempt(&path, &spawn),
        );
        drive(&mut port, false);
        assert_eq!(
            port.attempts.len(),
            5,
            "startup, three while live, one after"
        );
        assert_eq!(starts.load(Ordering::Acquire), 1, "exactly one start");
        assert!(port.shared.is_some(), "and the keeper attached to it");
        assert!(!port.local);
    }

    /// The keeper greets as an observer.
    #[test]
    fn the_keeper_attaches_as_an_observer() {
        let repo = super::super::TempRepo::new();
        let monitor = fake_monitor(repo.path(), 4631, welcome());
        let path = repo.path().to_path_buf();
        let mut port = Scripted::new(vec![], move || observer_attempt(&path, forbidden));
        drive(&mut port, false);
        assert!(port.shared.is_some());
        let hello = monitor.heard.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(hello.who, Who::Observer);
    }

    /// A monitor the keeper started is still held by its reaper after the
    /// keeper is gone, and reaped only when it exits.
    #[test]
    fn a_started_child_outlives_the_keeper_in_its_reaper() {
        struct ExitsLater {
            exit: mpsc::Receiver<()>,
            reaped: mpsc::Sender<()>,
        }
        impl Started for ExitsLater {
            fn reap(self: Box<Self>) {
                let _ = self.exit.recv();
                let _ = self.reaped.send(());
            }
        }
        let repo = super::super::TempRepo::new();
        let (exit, exit_rx) = mpsc::channel();
        let (reaped_tx, reaped) = mpsc::channel();
        let child = std::sync::Mutex::new(Some((exit_rx, reaped_tx)));
        let successor = std::sync::Mutex::new(None);
        let path = repo.path().to_path_buf();
        let mut port = Scripted::new(vec![], move || {
            observer_attempt(&path, |r| {
                *successor.lock().unwrap() = Some(fake_monitor(r, 4641, welcome()));
                let (exit, reaped) = child.lock().unwrap().take().unwrap();
                Ok(Box::new(ExitsLater { exit, reaped }) as Box<dyn Started>)
            })
        });
        drive(&mut port, false);
        assert!(port.shared.is_some());
        drop(port);
        assert!(
            reaped.recv_timeout(Duration::from_millis(200)).is_err(),
            "not reaped while it runs"
        );
        exit.send(()).unwrap();
        reaped
            .recv_timeout(Duration::from_secs(5))
            .expect("reaped once it exits");
    }
}
