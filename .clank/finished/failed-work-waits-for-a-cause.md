# failed-work-waits-for-a-cause

Invariants: I10.

## Why

Several loops retry failed or deferred work on a backoff whose interval
is capped but whose episode is not. By no-polls-but-github's rule a capped
interval is not a bound, so each is a poll, recorded as a residual in
`crates/cli/tests/it/timed_wait_boundary.rs`:

- `TimerSet::retry_failed` (a timer occurrence that will not write), on
  the monitor's `timer_retry` arm and `wait.rs items_with_context
  sleep_until#1`: 1 s doubling to 60 s.
- the status TUI's failed rebuild (`status_tui/mod.rs run_tui
  recv_until#1`, `retry_delay`, 1 s to 30 s) and `status_watch.rs
  StatusWatch::wait recv_timeout#0` (a member's failed refresh).
- `upgrade::Mover`'s failed replacement (`upgrade.rs Replacing::pass_until
  sleep#0`, and the same retry inside the TUI and status watch waits).
- a command source that keeps failing (`wait.rs run_leased_command_source
  pause_unless_stopped#1`, `cs::backoff` to 300 s) and a deferred
  retirement (`wait.rs until_retirement_may_settle`, both sites).
- the web bridge's connector reconnecting every second
  (`web/bridge_instance.rs connector sleep#0`) and the pane supervisor's
  dead-subscription restart (`web/mod.rs pane_supervisor sleep_until#0`).

## Build

1. For each, decide by what the failure waits on:
   - an event that can end it (the installation changing again, the
     events dir changing, the host child exiting, a member exiting):
     wait for that event and drop the timer;
   - no event: give it a finite budget, then fail or report it and stop
     retrying until something re-triggers the work.
   Follow the model `a-missing-monitor-is-heard-not-polled` settled
   (`monitor::outage`), reusing its pieces where they fit:
   - trying again is event-driven only; arm the watcher, then look once
     more before waiting, so an event in between is not lost;
   - the budget is one fixed deadline per failure episode, set at its
     first failure, never postponed by later failures or unrelated
     events, and checked before every attempt;
   - a watcher that cannot arm or whose channel closes ends the wait
     with the failure, never reads as quiet;
   - where a person is looking (the status TUI, the web bridge), the
     report says what failed and they can ask to retry.
2. Each replacement gets a no-lost-wake test, a no-periodic-wake test (a
   counter or a paused clock), and, where it has a budget, a test that
   unrelated activity across the deadline cannot postpone the report.
3. Reclassify each site in the gate; update I10.

## Acceptance

- None of the listed sites is a `Poll` in the gate.
- A failure that persists costs no wake after its budget.
