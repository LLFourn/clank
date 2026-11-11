//! The monitor's watchdog: armed while the loop has work in flight or is
//! retiring, and asleep otherwise.
//!
//! Why the monitor evicts itself at all: a lease serialises owners and
//! cannot evict a wedged one, and unlinking the lock to force it would
//! leave two processes holding flocks on two inodes — two owners. So the
//! monitor stands down on its own, from a thread that is not the loop
//! (the thing that may be stuck), and the lease frees honestly for the
//! next client to take. Its clients see the socket close.
//!
//! An idle loop blocked waiting for its next event is not wedged: it has
//! nothing to do. What can wedge is a turn — handling an event — or a
//! fold that keeps failing, so those are what arm the deadline.

use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use super::{LeaseWatch, Retiring};

/// How long one turn, or a run of failing folds, may last before the
/// monitor stands down; and the margin a retirement gets past its own
/// deadline.
pub(crate) const BUDGET: Duration = Duration::from_secs(20);

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct State {
    turn: Option<Instant>,
    failing: Option<Instant>,
}

/// What the loop tells the watchdog: when its current turn began, and
/// since when its folds have been failing. Idle, it is told nothing.
#[derive(Debug, Default)]
pub(crate) struct Busy {
    state: Mutex<State>,
    changed: Condvar,
}

impl Busy {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A turn begins now and lasts until the guard drops.
    pub(crate) fn turn(self: &Arc<Self>) -> Turn {
        self.lock().turn = Some(Instant::now());
        self.changed.notify_all();
        Turn(self.clone())
    }

    /// Whether the last fold failed. A run of failures is work in
    /// flight from the first of them until a fold succeeds.
    pub(crate) fn failing(&self, failed: bool) {
        let mut state = self.lock();
        state.failing = match (failed, state.failing) {
            (true, since @ Some(_)) => since,
            (true, None) => Some(Instant::now()),
            (false, _) => None,
        };
        self.changed.notify_all();
    }

    /// Since when the folds have been failing, if they are.
    #[cfg(test)]
    pub(crate) fn failing_since(&self) -> Option<Instant> {
        self.lock().failing
    }

    /// Something the watchdog judges by changed elsewhere: a retirement
    /// began.
    pub(crate) fn poke(&self) {
        self.changed.notify_all();
    }
}

/// A turn in progress. Dropping it ends the turn.
pub(crate) struct Turn(Arc<Busy>);

impl Drop for Turn {
    fn drop(&mut self) {
        self.0.lock().turn = None;
        self.0.changed.notify_all();
    }
}

/// What one look decides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Look {
    /// Nothing is in flight: sleep until told otherwise.
    Idle,
    /// Look again after this long, or when told.
    Within(Duration),
    StandDown(String),
}

/// The watchdog's decision at `now`, from the work in flight and the
/// retirement under way.
pub(crate) fn look(
    turn: Option<Instant>,
    failing: Option<Instant>,
    retiring: Retiring,
    retiring_left: Option<Duration>,
    now: Instant,
    budget: Duration,
) -> Look {
    match retiring {
        Retiring::Overdue => return Look::StandDown("its retirement overran its deadline".into()),
        // A monitor retiring has stopped turning on purpose, so its
        // turns' age is not judged while its own deadline holds.
        Retiring::Within => return Look::Within(retiring_left.unwrap_or(budget)),
        Retiring::No => {}
    }
    let oldest = [turn, failing].into_iter().flatten().min();
    let Some(since) = oldest else {
        return Look::Idle;
    };
    match budget.checked_sub(now.saturating_duration_since(since)) {
        Some(left) if !left.is_zero() => Look::Within(left),
        _ if failing == oldest => Look::StandDown(format!("its folds have failed for {budget:?}")),
        _ => Look::StandDown(format!("one turn has run for {budget:?}")),
    }
}

/// Watch `busy` from a thread of its own, and stand down if a turn or a
/// run of failing folds outlasts `budget`, or a retirement its deadline.
///
/// Not a task: the runtime is what may be wedged, and a task on a
/// stalled executor is exactly as stuck as the loop it is meant to
/// judge. A thread is scheduled by the kernel regardless.
pub(crate) fn start(busy: Arc<Busy>, lease: Arc<LeaseWatch>, budget: Duration) {
    std::thread::spawn(move || {
        let mut state = busy.lock();
        loop {
            let now_ms = super::unix_ms();
            let decided = look(
                state.turn,
                state.failing,
                lease.retiring(now_ms, budget),
                lease.retiring_left(now_ms, budget),
                Instant::now(),
                budget,
            );
            state = match decided {
                Look::Idle => busy
                    .changed
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner),
                Look::Within(left) => {
                    busy.changed
                        .wait_timeout(state, left)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0
                }
                Look::StandDown(why) => {
                    eprintln!(
                        "monitor: lease {} ({}) at {}: forced: {why}; producers are cut off, \
                         not drained",
                        lease.path.display(),
                        lease.held(),
                        super::timestamp(),
                    );
                    crate::cli::source_group::exit(super::STOOD_DOWN_EXIT);
                }
            };
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const B: Duration = BUDGET;

    #[test]
    fn an_idle_loop_arms_nothing() {
        let now = Instant::now();
        assert_eq!(look(None, None, Retiring::No, None, now, B), Look::Idle);
    }

    #[test]
    fn a_turn_is_judged_by_its_own_age() {
        let began = Instant::now();
        assert_eq!(
            look(Some(began), None, Retiring::No, None, began + B / 4, B),
            Look::Within(B - B / 4)
        );
        assert!(matches!(
            look(Some(began), None, Retiring::No, None, began + B, B),
            Look::StandDown(why) if why.contains("turn")
        ));
    }

    #[test]
    fn failing_folds_stand_it_down_though_turns_end() {
        let first = Instant::now();
        assert!(matches!(
            look(None, Some(first), Retiring::No, None, first + B / 2, B),
            Look::Within(_)
        ));
        assert!(matches!(
            look(None, Some(first), Retiring::No, None, first + B, B),
            Look::StandDown(why) if why.contains("failed")
        ));
    }

    #[test]
    fn a_retirement_is_judged_by_its_deadline_not_a_turns_age() {
        let old = Instant::now();
        let now = old + B * 3;
        assert_eq!(
            look(Some(old), None, Retiring::Within, Some(B / 2), now, B),
            Look::Within(B / 2)
        );
        assert!(matches!(
            look(None, None, Retiring::Overdue, None, now, B),
            Look::StandDown(why) if why.contains("retirement")
        ));
    }

    /// The guard is the turn: it arms on creation and disarms on drop,
    /// and a success clears a run of failures.
    #[test]
    fn the_busy_state_follows_the_loop() {
        let busy = Arc::new(Busy::default());
        assert_eq!(*busy.lock(), State::default());
        let turn = busy.turn();
        assert!(busy.lock().turn.is_some());
        drop(turn);
        assert!(busy.lock().turn.is_none());
        busy.failing(true);
        let first = busy.lock().failing;
        busy.failing(true);
        assert_eq!(busy.lock().failing, first, "from the first failure");
        busy.failing(false);
        assert_eq!(*busy.lock(), State::default());
    }
}
