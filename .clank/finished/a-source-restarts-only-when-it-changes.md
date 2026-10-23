# a-source-restarts-only-when-it-changes
# A source restarts only when it changes

## Problem

The monitor hosts an agent's event sources as one unit per author. In
`crates/cli/src/cli/monitor.rs`, the reconcile pass (around the
`running.spec == spec` check) compares the author's WHOLE resolved spec.
On any difference it stops the previous `Hosted` and respawns every
command source, together with a new `TimerSet`. The test near the
per-author restart case expects exactly that.

So a change to one source restarts all the others. That includes adding a
timer, which runs no process. On 23 Oct 06:47:05, and again on 16 Oct,
life's assistent added a timer to `.clank/agents/assistent/config.json`.
The monitor then restarted life's Signal receiver while it was sending,
which left a message whose delivery was unknown.

The stop compounds it. `command_source.rs` `ESCALATE_AFTER` gives a process
group 200 ms between SIGTERM and SIGKILL. A receiver that drains in-flight
sends needs seconds, and the signal-cli child in its group is killed with
it. The monitor already gives a lease 10 s to retire (`DRAIN_GRACE`);
a command source gets a fiftieth of that.

## Model

What gets hosted is a source, not an author's set of sources. A source's
identity is its durable storage key (name-based; it owns the log,
checkpoint, state dir and lease). A reconcile matches by that key and
compares each source's resolved definition with its own accepted one:

- unchanged: left running, never touched;
- removed or changed: that one source is stopped, with its own grace;
- added or changed: that one source is spawned.

Timers are reconciled by themselves and never stop or start a command
source. If adding a timer would restart a process, the model is wrong.
The per-author "all or nothing" restart goes, and so does the test
that expects it.

Stopping a source sends SIGTERM and waits for that source's stop grace
before SIGKILL. The grace is configurable per command source
(`stop_grace`, in seconds or a duration string, whichever the config
already uses for durations). The default is long enough to drain, at
least `DRAIN_GRACE` (10 s), rather than 200 ms. Monitor shutdown and
upgrade handoff use the same per-source grace, bounded where a lease
already bounds them.

Optional, if small: when a receipt lease is held by another live run,
`clank events` reports "in progress elsewhere", not "source busy".

## Acceptance

- In a pure test of the reconcile diff, adding or removing a timer, or
  changing an unrelated source, leaves an unchanged command source's
  running task as the same instance. Mutating the diff back to
  per-author equality fails the test.
- Only the changed source is restarted when it changes.
- The stop grace is honoured: a group that exits on SIGTERM within its
  grace is never sent SIGKILL. A configured `stop_grace` is parsed,
  validated and documented in the README/book reference. Test this
  through the existing signal seam; never mutate into a real signal
  path.
- The source of a removed author is still stopped.
- Run the tests for `monitor`, `command_source` and `timer_source`, plus
  the `it` modules that cover hosted sources.

## Out of scope

Signal-specific delivery semantics in the life repo, which life's master
owns.
