# a-missing-monitor-is-heard-not-polled

Invariants: I10.

## Why

Clients that cannot reach a monitor retry it on a timer with no bound,
recorded as residual polls in `crates/cli/tests/it/timed_wait_boundary.rs`:

- `wait.rs monitored sleep#0`: once the outage budget is spent and its
  notice was already delivered (`announce_outage` returns `false`), a
  parked wait retries every `OUTAGE_RETRY_CAP` (8 s) forever.
- `turn_child.rs carry sleep#2`: a stale child, or a report that cannot
  reach the agent, pauses `OUTAGE_RETRY_CAP` until the report's deadline,
  or forever without one.
- `monitor_keeper.rs Live::hear recv_timeout#0`: the status TUI's keeper
  retries with backoff to `RETRY_CAP` (300 s) while no monitor attaches.

Left in no-polls-but-github because `Unserved::Failed` covers causes with
different events, and one with none: the repo's lease held by a wedged
monitor (its release: a blocking flock on a thread, as
`github::log::when_released` does), another client starting a monitor (its
socket appearing in the runtime dir), a new install (`upgrade::Authority`),
a broken config (the repo's and the user's config files), and a monitor
that exits at start for a transient reason (resource limits) with none of
those changing. A wait on the events alone strands a parked agent in the
last case, which is the failure the outage notice exists to prevent.

## Build

1. Classify why each attempt failed by what can end it, in
   `monitor::Unserved`, not by a string.
2. Wait on the events the class names: the lease's release, the socket,
   the installation, the config files (`tree_watch`).
3. **Two separate obligations: when to try again, and when to report.**
   - **Trying again is event-driven only.** A client attempts the monitor
     again only when one of its failure class's events fires. No class,
     and no state of the outage, retries on a clock: the user's rule is
     that the only poll clank may keep is GitHub's. A class with no event
     (a transient exit at start) is never retried after its budget.
   - **Reporting has a one-shot deadline, whatever the class.** An
     unresolved outage reaches its report by a finite deadline even if the
     event it waits for never fires (a lease held forever by a wedged
     monitor, a config never edited). That keeps today's guarantee from
     `monitor::Outage::lost`: `Retiring` is reported by
     `RETIREMENT_BUDGET`, everything else by `OUTAGE_BUDGET`. A single
     deadline timer per outage is not a periodic poll.
   - **After the report.** A parked wait ends with the failure as its
     answer, so the agent is told rather than stranded. A session already
     told of this outage (`announce_outage` returns `false`, the
     `bound.outage` marker stands) is not told again: its wait stays
     parked on the class's events with no deadline, and the marker's
     clear-on-serve ends the outage. A turn child gives up its report with
     the failure recorded. The status TUI shows that no monitor could be
     attached and why, and attempts again only on a class event or when
     the user asks.
   - **A watcher that cannot be armed or dies ends the wait.** Creating a
     class's watcher (lease-release thread, runtime-dir or config
     `tree_watch`, installation watch) can fail, and its channel can
     close. Either is reported at once as the failure, never taken as a
     quiet subscription that will simply not fire.
4. Tests:
   - no lost wake: a monitor that appears between the failed attempt and
     the watch's arming is found;
   - progress on release: a held lease's release leads to an attach;
   - a lease held forever with no socket, config or install change: the
     outage is reported once by its deadline and no attach is attempted in
     between (an attempt counter under a paused clock);
   - an already-reported outage: no second notice, no attempt and no wake
     while nothing changes, and an attach once a class event fires;
   - a watcher that fails to arm, and one whose channel closes: each ends
     the wait with the failure.
5. Reclassify the three sites; update I10.
