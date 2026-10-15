# a-closing-host-stops-relaunching
# A command-source host whose producer registry has closed stops launching, and never reports that as the source failing

## Why

Life's recurring event `assistent command-voice-7f9bc9f9@191` (2026-10-15 11:20:09 Sydney)
carried the new diagnostic: "Its last run did not start: its group could not be
recorded (this process is exiting)". anxiety-attack's codex traced it:
- **The refusal.** `source_group::Owners::admit` refuses every new group once
  `closing` is set, with exactly that error. `closing` is terminal: `drain` and
  `retire_all` set it when the host process begins to exit, which includes a
  monitor retiring because a new build was installed.
- **The loop.** `wait::run_leased_command_source` turns that launch error into a
  failed run: `Ended::Launch`, `failures += 1`, back off, loop. Nothing checks
  whether admission has closed.

So a host that is shutting down keeps trying to start new producers. Each refused
attempt counts toward `FAILURES_BEFORE_SURFACING`, and enough of them raise a false
"has stopped 3 times" failure event for a source that never failed. An earlier
revision of `a-monitor-keeps-its-lease` had a `closing` check after each run. It was
removed as untested.

## What to build

### 1. A closed registry ends the host, not the source

**A typed refusal.** `source_group::launch` reports a refusal because admission has
closed as its own outcome, distinct from a real spawn, record or arm failure. It is
a typed variant, not a message to match.

**What the host does with it.** On that outcome, and before each relaunch when the
registry is already closing, `run_leased_command_source` returns:
- no failure is counted, no failure event is raised, and no backoff is taken;
- it reports no `BackingOff` lifecycle;
- it keeps the source's lease and log as the existing exit paths do;
- it releases nothing early that the drain or the shutdown retirement still owns.

### 2. Everything else is unchanged

A genuine launch failure is still `Ended::Launch`, counts toward the threshold, and
is reported with its diagnostic. Drain, output custody, leases, owed work, and the
one-unacked-failure policy are unchanged.

## Acceptance

- **Shutdown during backoff.** Through the real host with injected owners: a
  producer exits, the host backs off, and the owners close (drain or `retire_all`)
  during the backoff. The host returns without starting another anchor or producer,
  and no failure event is logged, even past the threshold's worth of attempts.
- **Closing just before admission.** The race at admission: `launch` returns the
  typed closed outcome, the host returns, and no failure is counted.
- **Genuine failures still count.** A command that cannot spawn still counts, and
  raises the failure event with its launch error at the threshold.
- **The source after an orderly drain:** no anchor or producer process is left
  running.

## As built

- **`source_group::launch`** returns `LaunchError`, either `Closed` or
  `Failed(String)`.
  - It returns `Closed` before spawning any anchor when the registry is already
    closing.
  - Any later error — anchor spawn, record, arming, spawn into the group — is
    `Closed` when the registry is closing by the time it surfaces, so a group
    the shutdown killed mid-arming is not the source failing.
  - Admission (`admit`) and `spawn_into` refuse with an `io::Error` wrapping the
    private `Closed` type, recognised by `refused` by type, not by message.
  - `Owners::closing` is now `pub(crate)`.
- **The host** (`run_leased_command_source`) treats closing as terminal at every
  phase:
  - at the top of each pass, before an unended predecessor is waited on or
    retired, so a closed host never waits on a deferring retirement;
  - `Err(Closed)` from `launch` returns at once;
  - after every outcome, launched or not, before the failure count, so nothing
    is counted, backed off or reported.
- **Tests:**
  - `source_group::a_closed_registry_refuses_as_closed_not_failed`: a missing
    command is `Failed`; a closed registry is `Closed` with no group recorded; and
    admission's refusal is typed.
  - `source_group::closing_while_arming_is_closed_not_failed`: an anchor that
    never acknowledges; the registry closes while `launch` is arming it; the
    launch ends `Closed`.
  - `wait::a_closing_host_stops_relaunching`: the registry closes during the
    backoff after a failed run; the host returns, with no failure event.
  - `wait::a_run_ended_by_closing_is_not_backed_off`: the registry closes during a
    run; the host returns within the first backoff.
  - `wait::a_closed_host_does_not_wait_on_a_deferred_predecessor`: the registry
    is already closed and every retirement defers; the host returns within 2 s.
  - `wait::closing_after_two_failures_raises_no_third`: two real failures, then
    closing during the backoff; no third run and no "has stopped" event.
- **Mutation checks:**
  - `launch` ignoring closing for errors after admission: caught by
    `closing_while_arming_is_closed_not_failed`.
  - No check at the top of the pass: caught by
    `a_closed_host_does_not_wait_on_a_deferred_predecessor`.
  - No check after the outcome: caught by
    `a_run_ended_by_closing_is_not_backed_off`.
  - Both host checks removed: both of those fail.
