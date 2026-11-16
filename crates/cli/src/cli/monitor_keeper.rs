//! The status TUI's monitor keeper.
//!
//! A monitor retires itself when the installed build changes. If only a
//! parked wait started the next one, a TUI open on a repo whose agents
//! were all idle would watch for itself indefinitely, and the monitor's
//! typed-nudge backstop — which only a running monitor can fire — would
//! never run. The keeper is the TUI's one observer
//! subscription and the lifecycle that brings it back: one attempt at a
//! time through the established attach-or-spawn path, another only when
//! something that can end the failure moves, the outage shown once at its
//! deadline, and a local watcher for the gap.
//!
//! Plain `clank status` and `status --watch` do not use it: they stay
//! read-only through [`super::core_wakes`].

use super::outage::{Ears, Heard, Hearing, Outage, Unserved};
use super::{Attached, Conn, Hangup, Started, Who, attempt_with, reconcile_after, subscribe};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Instant;

/// Where the keeper is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    /// Subscribed to the monitor's fold.
    Shared,
    /// An attempt is due.
    Attempting,
    /// No monitor. Attempted again when a source of the failure moves.
    Unattached,
}

/// What the keeper heard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Event {
    /// The fold moved, or the local watcher saw the repo change.
    Wake,
    /// The subscription's connection ended.
    Lost,
    /// Something that can end the failure moved.
    Moved,
    /// The person asked for an attempt.
    Asked,
    /// What can end the failure can no longer be heard, and why.
    Deaf(String),
    /// Time passed with nothing to hear.
    Quiet,
}

/// What the keeper drives: its subscription, its attempts, what it hears
/// while it has none, its local watcher and the panel it wakes.
pub(crate) trait Port {
    fn now(&self) -> Instant;
    /// Whether the keeper is to stop.
    fn stopped(&self) -> bool;
    /// The next thing to hear, waiting no later than `until`, or `None`
    /// once the keeper is to stop.
    fn hear(&mut self, until: Option<Instant>) -> Option<Event>;
    /// Make one attempt: its subscription, or why there is none.
    fn attempt(&mut self) -> Result<Conn, Unserved>;
    /// Arm what can end `unserved`, as [`Hearing::arm`] does.
    fn arm(&mut self, unserved: &Unserved) -> Result<bool, String>;
    /// Make `conn` the keeper's subscription; nothing more is heard for
    /// the outage it ends.
    fn adopt(&mut self, conn: Conn);
    fn forward(&mut self);
    fn watch_locally(&mut self);
    fn handoff(&mut self);
    /// Show that no monitor could be attached, and why.
    fn report(&mut self, why: String);
}

/// Run the keeper until its port says stop, starting from whether the
/// first subscribe attached.
///
/// A wake only ever forwards. A refresh, a keypress or a repo edit is
/// not evidence that the repo has become attachable, and an attempt on
/// each would turn a busy working tree into a fork per edit against a
/// repairer, a held lease or a newer peer.
pub(crate) fn drive(port: &mut impl Port, attached: bool) {
    let mut outage = Outage::default();
    let mut why = String::new();
    let mut asked = false;
    let mut phase = Phase::Shared;
    if !attached {
        // A stop can land before the thread first runs; no watcher or
        // attempt may follow it.
        if port.stopped() {
            return;
        }
        port.watch_locally();
        phase = Phase::Attempting;
    }
    loop {
        if port.stopped() {
            return;
        }
        // On every transition, and so before every attempt: wakes or moves
        // that keep coming must not keep the report from its deadline.
        report_if_due(port, &mut outage, &why);
        if phase == Phase::Attempting {
            let tried = port.attempt();
            // Cancelled in flight: the connection is dropped unadopted,
            // and a monitor the attempt started stays with its own reaper.
            if port.stopped() {
                return;
            }
            phase = match tried {
                Ok(conn) => {
                    port.adopt(conn);
                    outage.answered();
                    asked = false;
                    port.handoff();
                    Phase::Shared
                }
                Err(unserved) => {
                    why = unserved.why().to_string();
                    tracing::debug!("the monitor keeper did not attach: {why}");
                    outage.lost(tokio::time::Instant::from_std(port.now()), &unserved);
                    match port.arm(&unserved) {
                        Ok(true) => Phase::Attempting,
                        // The person who asked is answered now, not at a
                        // deadline already spent.
                        Ok(false) if std::mem::take(&mut asked) => {
                            port.report(why.clone());
                            outage.reported();
                            Phase::Unattached
                        }
                        Ok(false) => Phase::Unattached,
                        Err(deaf) => {
                            asked = false;
                            port.report(format!("{why}, and {deaf}"));
                            outage.reported();
                            Phase::Unattached
                        }
                    }
                }
            };
            continue;
        }
        let until = match phase {
            Phase::Unattached => outage.report_by().map(tokio::time::Instant::into_std),
            Phase::Shared | Phase::Attempting => None,
        };
        let Some(event) = port.hear(until) else {
            return;
        };
        phase = match (phase, event) {
            (phase, Event::Wake) => {
                port.forward();
                phase
            }
            (Phase::Shared, Event::Lost) => {
                port.watch_locally();
                Phase::Attempting
            }
            (Phase::Unattached, Event::Moved) => Phase::Attempting,
            (Phase::Unattached, Event::Asked) => {
                asked = true;
                Phase::Attempting
            }
            (Phase::Unattached, Event::Deaf(deaf)) => {
                port.report(format!("{why}, and {deaf}"));
                outage.reported();
                Phase::Unattached
            }
            (phase, _) => phase,
        };
    }
}

fn report_if_due(port: &mut impl Port, outage: &mut Outage, why: &str) {
    if outage
        .report_by()
        .is_some_and(|by| by.into_std() <= port.now())
    {
        port.report(why.to_string());
        outage.reported();
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

/// The outage as the panel sees it: why no monitor could be attached, once
/// the keeper has said so, and the person's asks to try again.
#[derive(Debug, Clone, Default)]
pub(crate) struct Unattached(Arc<Mutex<Said>>);

#[derive(Debug, Default)]
struct Said {
    /// Counted, so the same reason said again is a new report.
    why: Option<(u64, String)>,
    reports: u64,
    asked: bool,
    keeper: Option<mpsc::Sender<()>>,
}

impl Unattached {
    fn said(&self) -> std::sync::MutexGuard<'_, Said> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn set(&self, why: Option<String>) {
        let mut said = self.said();
        said.why = why.map(|why| {
            said.reports += 1;
            (said.reports, why)
        });
    }

    /// The last report, numbered, while no monitor is attached.
    pub(crate) fn why(&self) -> Option<(u64, String)> {
        self.said().why.clone()
    }

    /// Ask the keeper for one attempt. It is heard only while the keeper
    /// is unattached.
    pub(crate) fn ask(&self) {
        let mut said = self.said();
        said.asked = true;
        if let Some(keeper) = &said.keeper {
            let _ = keeper.send(());
        }
    }

    fn take_ask(&self) -> bool {
        std::mem::take(&mut self.said().asked)
    }

    fn heard_by(&self, keeper: mpsc::Sender<()>) {
        self.said().keeper = Some(keeper);
    }
}

/// The production port: a monitor connection or a local watcher of type
/// `W`, made by `watch`, attempts made by `attempt`, and what can end a
/// failed one heard through `ears`.
pub(crate) struct Live<A, M, W, E> {
    attempt: A,
    watch: M,
    ears: E,
    sub: Option<Conn>,
    local: Option<W>,
    hearing: Option<Hearing>,
    /// What the hearing heard, each with a poke on the local channel.
    heard: Arc<Mutex<std::collections::VecDeque<Heard>>>,
    local_tx: mpsc::Sender<()>,
    local_rx: mpsc::Receiver<()>,
    wake: mpsc::Sender<()>,
    fresh: FreshRebuild,
    unattached: Unattached,
    stop: Arc<Hangup>,
    closed: bool,
}

impl<A, M, W, E> Live<A, M, W, E>
where
    A: FnMut() -> Result<Conn, Unserved>,
    M: FnMut(mpsc::Sender<()>) -> anyhow::Result<W>,
    E: Ears,
{
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        attempt: A,
        watch: M,
        ears: E,
        sub: Option<Conn>,
        wake: mpsc::Sender<()>,
        fresh: FreshRebuild,
        unattached: Unattached,
        stop: Arc<Hangup>,
    ) -> Self {
        let (local_tx, local_rx) = mpsc::channel();
        stop.poke_on_stop(local_tx.clone());
        unattached.heard_by(local_tx.clone());
        if let Some(conn) = &sub {
            stop.hang_up_on(conn);
        }
        Self {
            attempt,
            watch,
            ears,
            sub,
            local: None,
            hearing: None,
            heard: Arc::default(),
            local_tx,
            local_rx,
            wake,
            fresh,
            unattached,
            stop,
            closed: false,
        }
    }

    fn deafen(&mut self) {
        self.hearing = None;
        self.heard
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }
}

impl<A, M, W, E> Port for Live<A, M, W, E>
where
    A: FnMut() -> Result<Conn, Unserved>,
    M: FnMut(mpsc::Sender<()>) -> anyhow::Result<W>,
    E: Ears,
{
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn stopped(&self) -> bool {
        self.closed || self.stop.stopped()
    }

    fn hear(&mut self, until: Option<Instant>) -> Option<Event> {
        if self.stopped() {
            return None;
        }
        // Blocked on rather than polled: a stop shuts the socket and
        // pokes the local channel, and an observer is sent a note only
        // when the fold moved, so silence is never loss. A monitor that
        // dies or is stood down by its watchdog closes the socket.
        if let Some(conn) = &mut self.sub {
            return match conn.next_note() {
                Ok(_) => Some(Event::Wake),
                Err(_) if self.stopped() => None,
                Err(e) => {
                    eprintln!("clank: the monitor went away ({e}); watching this repo directly");
                    self.sub = None;
                    Some(Event::Lost)
                }
            };
        }
        let poked = match until {
            Some(at) => self
                .local_rx
                .recv_timeout(at.saturating_duration_since(Instant::now())),
            None => self
                .local_rx
                .recv()
                .map_err(|_| mpsc::RecvTimeoutError::Disconnected),
        };
        if self.stopped() {
            return None;
        }
        if poked.is_err() {
            return Some(Event::Quiet);
        }
        if self.unattached.take_ask() {
            return Some(Event::Asked);
        }
        let heard = self
            .heard
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front();
        match (heard, &mut self.hearing) {
            (Some(heard), Some(hearing)) => match hearing.heard(heard) {
                Ok(()) => Some(Event::Moved),
                Err(why) => {
                    self.deafen();
                    Some(Event::Deaf(why))
                }
            },
            _ => Some(Event::Wake),
        }
    }

    fn attempt(&mut self) -> Result<Conn, Unserved> {
        (self.attempt)()
    }

    fn arm(&mut self, unserved: &Unserved) -> Result<bool, String> {
        let hearing = self.hearing.get_or_insert_with(|| {
            let (heard, poke) = (self.heard.clone(), self.local_tx.clone());
            Hearing::new(move |h| {
                heard
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push_back(h);
                let _ = poke.send(());
            })
        });
        let armed = hearing.arm(&self.ears, unserved);
        if armed.is_err() {
            self.deafen();
        }
        armed
    }

    fn adopt(&mut self, conn: Conn) {
        self.deafen();
        self.unattached.take_ask();
        self.stop.hang_up_on(&conn);
        self.sub = Some(conn);
    }

    fn forward(&mut self) {
        if self.wake.send(()).is_err() {
            self.closed = true;
        }
    }

    fn watch_locally(&mut self) {
        self.local = reconcile_after(
            &crate::fs_notice::Wake::from(self.wake.clone()),
            (self.watch)(self.local_tx.clone()),
        );
    }

    fn handoff(&mut self) {
        self.local = None;
        self.unattached.set(None);
        // Owed before the wake, so the refresh it raises finds it.
        self.fresh.owe();
        self.forward();
    }

    fn report(&mut self, why: String) {
        self.unattached.set(Some(why));
        self.forward();
    }
}

/// The TUI's monitor keeper, on its own thread. Dropping it stops the
/// thread's attempts and releases its watchers; the monitor itself is
/// independent and never signalled.
pub(crate) struct MonitorKeeper {
    stop: Arc<Hangup>,
    fresh: FreshRebuild,
    unattached: Unattached,
    fault: crate::fs_notice::Fault,
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
        let stop = Arc::new(Hangup::default());
        let fresh = FreshRebuild::default();
        let unattached = Unattached::default();
        let fault = crate::fs_notice::Fault::default();
        let first = match subscribe(repo) {
            Attached::Shared(conn) => Some(conn),
            Attached::Alone(why) => {
                tracing::debug!("{} has no monitor to share yet: {why}", repo.display());
                None
            }
        };
        let repo = repo.to_path_buf();
        {
            let (stop, fresh, unattached, watching) = (
                stop.clone(),
                fresh.clone(),
                unattached.clone(),
                fault.clone(),
            );
            std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build();
                let attached = first.is_some();
                let watched = repo.clone();
                let ears = super::outage::Kernel::new(&repo, crate::upgrade::process());
                let mut live = Live::new(
                    || match &runtime {
                        Ok(runtime) => runtime
                            .block_on(attempt_with(&repo, Who::Observer, &spawn))
                            .served(&repo),
                        Err(e) => Err(Unserved::Unstartable(format!(
                            "no runtime to attach on: {e}"
                        ))),
                    },
                    |tx| {
                        crate::repo_watch::RepoStateWatcher::attach_reporting_to(
                            &watched,
                            tx,
                            watching.clone(),
                        )
                        .inspect_err(|e| {
                            crate::fs_notice::record(
                                &watching,
                                format!("cannot watch {}: {e:#}", watched.display()),
                            )
                        })
                    },
                    ears,
                    first,
                    wake,
                    fresh,
                    unattached,
                    stop,
                );
                drive(&mut live, attached);
            });
        }
        Self {
            stop,
            fresh,
            unattached,
            fault,
        }
    }

    /// Why the watcher it keeps while no monitor serves failed, once it
    /// has: from then on nothing wakes the panel for the repo's state.
    pub(crate) fn fault(&self) -> Option<String> {
        crate::fs_notice::read(&self.fault)
    }

    /// Why no monitor could be attached, numbered, once the outage was
    /// reported and until one is.
    pub(crate) fn unattached(&self) -> Option<(u64, String)> {
        self.unattached.why()
    }

    /// Attempt once more now, as the person asked: heard only while no
    /// monitor is attached, and through the keeper's own attempts, one at
    /// a time. A failure is reported at once and its watches armed again.
    pub(crate) fn retry(&self) {
        self.unattached.ask();
    }

    /// The fresh rebuilds this keeper owes the panel.
    pub(crate) fn fresh(&self) -> FreshRebuild {
        self.fresh.clone()
    }
}

impl Drop for MonitorKeeper {
    fn drop(&mut self) {
        self.stop.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::super::Attempt;
    use super::super::outage::scripted::Scripted as ScriptedEars;
    use super::super::outage::{Source, Unserved};
    use super::*;
    use std::time::Duration;

    /// A repo nothing is published for, so no attempt reads as a
    /// retirement.
    const NOWHERE: &str = "/nonexistent/clank-keeper-test-repo";

    fn served(mut attempt: impl FnMut() -> Attempt) -> impl FnMut() -> Result<Conn, Unserved> {
        move || attempt().served(std::path::Path::new(NOWHERE))
    }

    enum Step {
        Hear(Event),
        /// `event`, once this long has passed.
        After(Duration, Event),
        /// Nothing until the report is due.
        Idle,
        Do(Box<dyn FnOnce()>),
    }

    /// A port that plays a script on a virtual clock and records what the
    /// keeper did.
    struct Scripted<A: FnMut() -> Attempt> {
        now: Instant,
        script: std::collections::VecDeque<Step>,
        attempt: A,
        ears: ScriptedEars,
        hearing: Hearing,
        attempts: Vec<Instant>,
        forwarded: usize,
        local: bool,
        watched: usize,
        handoffs: usize,
        reports: Vec<(Instant, String)>,
        shared: Option<Conn>,
    }

    impl<A: FnMut() -> Attempt> Scripted<A> {
        fn new(script: Vec<Step>, attempt: A) -> Self {
            Self {
                now: Instant::now(),
                script: script.into(),
                attempt,
                ears: ScriptedEars::default(),
                hearing: Hearing::new(|_| {}),
                attempts: Vec::new(),
                forwarded: 0,
                local: false,
                watched: 0,
                handoffs: 0,
                reports: Vec::new(),
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

        fn attempt(&mut self) -> Result<Conn, Unserved> {
            self.attempts.push(self.now);
            (self.attempt)().served(std::path::Path::new(NOWHERE))
        }

        fn arm(&mut self, unserved: &Unserved) -> Result<bool, String> {
            self.hearing.arm(&self.ears, unserved)
        }

        fn adopt(&mut self, conn: Conn) {
            self.hearing = Hearing::new(|_| {});
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

        fn report(&mut self, why: String) {
            self.reports.push((self.now, why));
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

    fn busy() -> Attempt {
        Attempt::Busy("another client is already starting one".into())
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

    /// With nothing moving, a failed attempt is looked at once more as its
    /// watches are armed, and then nothing is attempted: the outage is
    /// shown once, at its deadline, and time passing after that starts
    /// nothing either.
    #[test]
    fn an_unattached_keeper_reports_once_at_its_deadline_and_attempts_nothing_more() {
        let mut port = Scripted::new(
            (0..4).map(|_| Step::Idle).collect(),
            outcomes(vec![busy, busy]),
        );
        drive(&mut port, false);
        let t0 = port.attempts[0];
        assert_eq!(port.attempts, [t0, t0]);
        assert_eq!(
            port.reports,
            [(
                t0 + super::super::OUTAGE_BUDGET,
                "another client is already starting one".to_string()
            )]
        );
        assert_eq!(
            port.ears.armed(),
            [Source::Permit, Source::Published],
            "what can end another client's repair"
        );
    }

    /// Wakes that never stop, across four outage budgets, start nothing and
    /// do not keep the outage from being shown once, at the first of them
    /// past its deadline.
    #[test]
    fn wakes_across_the_deadline_cannot_starve_the_report() {
        let step = super::super::OUTAGE_BUDGET / 2;
        let mut port = Scripted::new(
            (0..8).map(|_| Step::After(step, Event::Wake)).collect(),
            outcomes(vec![busy, busy]),
        );
        drive(&mut port, false);
        let t0 = port.attempts[0];
        assert_eq!(port.attempts, [t0, t0]);
        assert_eq!(port.forwarded, 8);
        assert_eq!(
            port.reports,
            [(
                t0 + super::super::OUTAGE_BUDGET,
                "another client is already starting one".to_string()
            )]
        );
    }

    /// Moves that keep failing, across the deadline: each is attempted on,
    /// but the one past the deadline is reported before its attempt.
    #[test]
    fn moves_across_the_deadline_are_reported_before_the_next_attempt() {
        let step = super::super::OUTAGE_BUDGET / 2;
        let mut port = Scripted::new(
            (0..4).map(|_| Step::After(step, Event::Moved)).collect(),
            busy,
        );
        drive(&mut port, false);
        let t0 = port.attempts[0];
        let at = |n: u32| t0 + step * n;
        assert_eq!(port.attempts, [at(0), at(0), at(1), at(2), at(3), at(4)]);
        assert_eq!(port.reports.len(), 1);
        assert_eq!(
            port.reports[0].0,
            at(2),
            "before the attempt at the deadline"
        );
    }

    /// Wakes of every kind while unattached start nothing, and every one
    /// reaches the panel.
    #[test]
    fn wakes_while_unattached_start_nothing() {
        let mut script = vec![];
        for _ in 0..20 {
            script.push(Step::Hear(Event::Wake));
            script.push(Step::Hear(Event::Quiet));
        }
        let mut port = Scripted::new(script, outcomes(vec![busy, busy]));
        drive(&mut port, false);
        assert_eq!(port.attempts.len(), 2, "the startup attempt and its look");
        assert_eq!(port.forwarded, 20);
    }

    /// After the outage was shown, a source of its failure moving is
    /// attempted on, and the attach ends the outage.
    #[test]
    fn a_moved_source_is_attempted_on_after_the_report() {
        let mut port = Scripted::new(
            vec![
                Step::Idle,
                Step::Hear(Event::Wake),
                Step::Idle,
                Step::Hear(Event::Moved),
            ],
            outcomes(vec![busy, busy, || Attempt::Shared(conn())]),
        );
        drive(&mut port, false);
        assert_eq!(port.reports.len(), 1);
        assert_eq!(port.attempts.len(), 3);
        assert!(port.shared.is_some() && !port.local);
        assert_eq!(port.handoffs, 1);
    }

    /// A keeper whose watches cannot be armed, or whose watch stops, shows
    /// why at once and attempts nothing more.
    #[test]
    fn a_watch_refused_or_stopped_is_shown_and_ends_the_attempts() {
        let attempts = Arc::new(AtomicU64::new(0));
        let failing = |attempts: Arc<AtomicU64>| {
            served(move || {
                attempts.fetch_add(1, Ordering::AcqRel);
                Attempt::SpawnFailed("starting one failed".into())
            })
        };
        let run = |ears: ScriptedEars| {
            let (wake, woken) = mpsc::channel();
            let unattached = Unattached::default();
            let stop = Arc::new(Hangup::default());
            let (watch, _, _) = counted_watcher();
            let live = Live::new(
                failing(attempts.clone()),
                watch,
                ears,
                None,
                wake,
                FreshRebuild::default(),
                unattached.clone(),
                stop.clone(),
            );
            let keeper = std::thread::spawn(move || {
                let mut live = live;
                drive(&mut live, false);
            });
            (keeper, woken, unattached, stop)
        };

        let refused = ScriptedEars::default();
        refused.refuse("no descriptors left");
        let (keeper, woken, unattached, stop) = run(refused);
        while unattached.why().is_none() {
            woken.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        assert_eq!(
            unattached.why().map(|(_, why)| why).as_deref(),
            Some("starting one failed, and cannot watch the config files: no descriptors left")
        );
        stop.stop();
        keeper.join().unwrap();
        assert_eq!(attempts.swap(0, Ordering::AcqRel), 1);

        let ears = ScriptedEars::default();
        let (keeper, woken, unattached, stop) = run(ears.clone());
        let giving_up = Instant::now() + Duration::from_secs(5);
        while attempts.load(Ordering::Acquire) < 2 || ears.armed().len() < 3 {
            assert!(
                Instant::now() < giving_up,
                "it never armed and looked again"
            );
            let _ = woken.recv_timeout(Duration::from_millis(10));
        }
        ears.end(Source::Published);
        while unattached.why().is_none() {
            woken.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        assert_eq!(
            unattached.why().map(|(_, why)| why).as_deref(),
            Some(
                "starting one failed, and the watch on the repo's published monitor ended: \
                 its watch stopped"
            )
        );
        assert!(!ears.fire(Source::Config), "nothing is heard any more");
        stop.stop();
        keeper.join().unwrap();
        assert_eq!(attempts.load(Ordering::Acquire), 2);
    }

    /// After a watch stopped, idle time, wakes and quiet start nothing; the
    /// person's ask makes one attempt, and its failure is reported at once.
    #[test]
    fn only_the_persons_ask_attempts_after_a_watch_stopped() {
        let mut port = Scripted::new(
            vec![
                Step::Hear(Event::Deaf("its watch stopped".into())),
                Step::Idle,
                Step::Hear(Event::Wake),
                Step::Hear(Event::Quiet),
                Step::Idle,
                Step::Hear(Event::Asked),
            ],
            busy,
        );
        drive(&mut port, false);
        let t0 = port.attempts[0];
        assert_eq!(port.attempts, [t0, t0, t0], "one attempt for the ask");
        assert_eq!(port.forwarded, 1);
        let why = "another client is already starting one".to_string();
        assert_eq!(
            port.reports,
            [(t0, format!("{why}, and its watch stopped")), (t0, why),]
        );
    }

    /// The live keeper, its watch stopped: its own watcher's wakes start
    /// nothing, and the person's ask attempts, arms the failure's watches
    /// again and reports again at once.
    #[test]
    fn an_ask_after_a_stopped_watch_attempts_and_arms_again() {
        let attempts = Arc::new(AtomicU64::new(0));
        let ears = ScriptedEars::default();
        let (wake, woken) = mpsc::channel();
        let unattached = Unattached::default();
        let stop = Arc::new(Hangup::default());
        let repo_wakes = Arc::new(Mutex::new(None));
        let live = Live::new(
            {
                let attempts = attempts.clone();
                served(move || {
                    attempts.fetch_add(1, Ordering::AcqRel);
                    Attempt::SpawnFailed("starting one failed".into())
                })
            },
            {
                let repo_wakes = repo_wakes.clone();
                move |tx| {
                    *repo_wakes.lock().unwrap() = Some(tx);
                    Ok(())
                }
            },
            ears.clone(),
            None,
            wake,
            FreshRebuild::default(),
            unattached.clone(),
            stop.clone(),
        );
        let keeper = std::thread::spawn(move || {
            let mut live = live;
            drive(&mut live, false);
        });
        let until = |what: &str, done: &dyn Fn() -> bool| {
            let giving_up = Instant::now() + Duration::from_secs(5);
            while !done() {
                assert!(Instant::now() < giving_up, "{what}");
                let _ = woken.recv_timeout(Duration::from_millis(10));
            }
        };
        until("armed", &|| ears.armed().len() == 3);
        ears.end(Source::Published);
        until("reported", &|| unattached.why().is_some());
        assert!(ears.armed().is_empty(), "nothing is heard any more");
        let reported = unattached.why().unwrap().0;

        let repo_wakes = repo_wakes.lock().unwrap().clone().unwrap();
        for _ in 0..5 {
            repo_wakes.send(()).unwrap();
        }
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(
            attempts.load(Ordering::Acquire),
            2,
            "a wake attempts nothing"
        );

        unattached.ask();
        until("reported again", &|| {
            unattached.why().is_some_and(|(n, _)| n > reported)
        });
        assert_eq!(
            attempts.load(Ordering::Acquire),
            4,
            "the ask, and the look as its watches were armed again"
        );
        assert_eq!(ears.armed().len(), 3, "its failure's watches, armed again");
        assert_eq!(
            unattached.why().map(|(_, why)| why).as_deref(),
            Some("starting one failed")
        );
        stop.stop();
        keeper.join().unwrap();
    }

    /// The local-to-shared handoff releases the local watcher once and
    /// owes the panel exactly one fresh rebuild.
    #[test]
    fn the_handoff_releases_the_watcher_and_owes_one_fresh_rebuild() {
        let (wake, woken) = mpsc::channel();
        let fresh = FreshRebuild::default();
        let (watch, watching, _) = counted_watcher();
        let mut live = Live::new(
            served(outcomes(vec![|| Attempt::Shared(conn())])),
            watch,
            ScriptedEars::default(),
            None,
            wake,
            fresh.clone(),
            Unattached::default(),
            Arc::new(Hangup::default()),
        );
        live.watch_locally();
        assert_eq!(watching.load(Ordering::Acquire), 1);
        assert_eq!(woken.try_iter().count(), 1, "the gap is reconciled");
        let conn = live.attempt().expect("it attaches");
        live.adopt(conn);
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

    /// Cancelling while unattached stops the keeper's loop and leaves
    /// nothing of it: neither subscription nor watcher.
    #[test]
    fn cancellation_stops_the_keeper_and_releases_the_watcher() {
        let (wake, _woken) = mpsc::channel();
        let stop = Arc::new(Hangup::default());
        let (watch, watching, _) = counted_watcher();
        let (attempted, attempts) = mpsc::channel();
        let ears = ScriptedEars::default();
        let live = Live::new(
            served(move || {
                let _ = attempted.send(());
                Attempt::NotStarted("lease held".into())
            }),
            watch,
            ears.clone(),
            None,
            wake,
            FreshRebuild::default(),
            Unattached::default(),
            stop.clone(),
        );
        let keeper = std::thread::spawn(move || {
            let mut live = live;
            drive(&mut live, false);
        });
        for _ in 0..2 {
            attempts
                .recv_timeout(Duration::from_secs(5))
                .expect("the attempt and the look on arming");
        }
        stop.stop();
        let began = Instant::now();
        keeper.join().unwrap();
        assert!(began.elapsed() < Duration::from_secs(5), "not held");
        assert_eq!(
            watching.load(Ordering::Acquire),
            0,
            "the watcher went with it"
        );
        assert!(ears.armed().is_empty(), "and so did what it heard");
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
                served(move || {
                    attempts.fetch_add(1, Ordering::AcqRel);
                    Attempt::Shared(conn())
                })
            },
            watch,
            ScriptedEars::default(),
            None,
            wake,
            FreshRebuild::default(),
            Unattached::default(),
            {
                let stop = Arc::new(Hangup::default());
                stop.stop();
                stop
            },
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
        let stop = Arc::new(Hangup::default());
        let (watch, watching, _) = counted_watcher();
        let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        let fresh = FreshRebuild::default();
        let mut live = Live::new(
            {
                let stop = stop.clone();
                let mut ours = Some(ours);
                served(move || {
                    stop.stop();
                    Attempt::Shared(Conn::over(ours.take().expect("one attempt")))
                })
            },
            watch,
            ScriptedEars::default(),
            None,
            wake,
            fresh.clone(),
            Unattached::default(),
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
        gone: Arc<std::sync::atomic::AtomicBool>,
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
        let gone = Arc::new(std::sync::atomic::AtomicBool::new(false));
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
        assert!(
            matches!(got, Attempt::LeaseHeld(_)),
            "a lease that stays held is a monitor going, said as that: {got:?}"
        );

        let quiet = super::super::TempRepo::new();
        assert!(matches!(
            observer_attempt(quiet.path(), |_| Ok(Box::new(NoChild) as Box<dyn Started>)),
            Attempt::Unattached(_)
        ));
    }

    /// Busy, newer, other and unproven peers are never a reason to start,
    /// and with nothing moving each is attempted once and looked at once
    /// more as its watches are armed, then shown at its deadline.
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
            let mut port = Scripted::new((0..4).map(|_| Step::Idle).collect(), move || {
                observer_attempt(&path, &spawn)
            });
            drive(&mut port, false);
            assert_eq!(starts.load(Ordering::Acquire), 0, "{name}: nothing started");
            let t0 = port.attempts[0];
            assert_eq!(port.attempts, [t0, t0], "{name}: no attempt on a clock");
            assert_eq!(port.reports.len(), 1, "{name}: shown once");
        }
    }

    /// A newer peer refuses the keeper and stays live: nothing more is
    /// attempted while it does. Once it has exited and that is heard, one
    /// attempt starts exactly one monitor and attaches, with no repo edit
    /// in between.
    #[test]
    fn a_rejecting_peer_that_exits_is_succeeded_once_its_going_is_heard() {
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
                Step::Do(exits),
                Step::Hear(Event::Moved),
                Step::Hear(Event::Wake),
                Step::Hear(Event::Quiet),
            ],
            move || observer_attempt(&path, &spawn),
        );
        drive(&mut port, false);
        assert_eq!(port.attempts.len(), 3, "two while live, one after");
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
                std::thread::spawn(move || {
                    let _ = self.exit.recv();
                    let _ = self.reaped.send(());
                });
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
