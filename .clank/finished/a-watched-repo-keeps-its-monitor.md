# a-watched-repo-keeps-its-monitor
# A repo with a TUI open keeps its monitor running

## Problem

On 2026-10-23 the ~/src/clankstack master went idle with its Stop-hook wait parked. Its monitor (pid 11481) then retired. The likely cause is that a new clank build was installed around 16:52 AEDT, since monitors retire themselves when the installed build changes. At 17:36 AEDT Claude Code "compacted while idle". After that there was no `clank wait` for the master's session. When a plan was queued three days later, nothing delivered it, and the user found the master asleep with a `clank tui` open on the repo showing "CLAUDE promote clank-branch-label".

Three gaps lined up:
1. **Only a parked wait (and a couple of hooks) ever starts a monitor** (`spawn_monitor` callers: wait.rs, turn_child.rs, attend.rs, recovery_hook.rs, monitor_upgrade.rs). A wait whose monitor retires falls back to watching locally (wait.rs:575) and does not start a successor. The status TUI subscribes "through the monitor if there is one" (`monitor::core_wakes`) and never starts one. So a monitor that retires on install stays gone until some agent's turn ends.
2. **A Claude session can lose its parked wait without a turn ending.** The evidence here is circumstantial: the wait was gone after an idle compaction. The Claude Code docs don't say whether compaction cancels a running asyncRewake hook. The SessionStart(compact) hook ran (the binding was rewritten at 06:36:35Z) but only rebinds.
3. **The monitor's typed-nudge path is the backstop** for an idle agent that owes work and has no wait. It can only run while a monitor is up.

Gap 1 is the one clank controls end to end, and fixing it makes gap 2 recoverable through gap 3. That holds whatever Claude Code does to hooks at compaction.

## Change

### The status TUI owns one observer subscription with a recovery lifecycle

This applies to the status TUI (`clank tui`) only. Plain `clank status` and `clank status --watch` stay read-only: they keep today's subscribe-if-present `core_wakes` and never start a monitor.

**One worker owns it.** The TUI owns exactly one *monitor keeper*, a worker started with the TUI and stopped when the TUI drops. It holds the one current observer subscription and at most one recovery attempt at a time. It never runs on the UI event or render loop: attach and ownership settlement can take seconds, and the loop only ever receives the keeper's wake beats.

**When it attaches:**
- **At startup,** if the first subscribe finds no compatible monitor, the keeper makes one bounded attempt through the established `attach_or_spawn_with(.., Who::Observer, spawn_monitor)`. That path does the versioned greeting, takes the lease and repair permit, and spawns the installed build.
- **On loss,** when the current subscription's connection ends (today's `forward_beats` loss), the keeper makes one such attempt off the loop. A successful attempt replaces the old subscription. There is never a second live subscription, and no subscription or watcher is created per status refresh.
- **What doesn't trigger an attempt:** a refresh, a keypress, an animation frame, a heartbeat or a repo edit.

**When an attempt fails, it backs off.** These all count as a failed attempt for scheduling: a busy repairer, a failed spawn, a held lease, an untrusted or unproven namespace, or an incompatible peer (`Seen::Theirs`, `Seen::Newer`, `Seen::Unproven`).
- Every unavailable outcome, including `Newer` and `Theirs`, schedules one reclassification on the same backoff: 5s, doubling, capped at 5 minutes. That one low-cost timer wakes it, with no fast polling.
- **A reclassification is never itself permission to start.** It runs the established attach and repair machinery again, so a spawn happens only when the normal ownership checks find the repo genuinely vacant.
  - While the peer is live, the result is `Newer` or `Theirs` again, and the spawner is never called.
  - Once the peer has exited and the lease is free, the result is vacant and exactly one start is allowed.
  - No persistent connection is held to a peer that rejected the greeting. Its death is never inferred from that greeting closing, and a stale pid or descriptor is never treated as permission to spawn.
- Signals that arrive while an attempt or a backoff is pending are coalesced into nothing.
- A successful attach resets the backoff.

**Local watching fills any gap.** While there is no shared subscription, the TUI keeps today's local `RepoStateWatcher` fallback, so its display stays live. When sharing resumes, the local watcher is released. The handoff then forces one fresh full rebuild of the snapshot, bypassing the unchanged-signature gate once, so changes made during the gap aren't baselined away.

**Dropping the TUI stops everything the keeper started:** its retry timer, its pending attempt (cancelled, or left to finish and then discarded) and its local watcher. It never signals or kills the monitor, which is independent and keeps serving its agents. No helper thread is left behind or recreated.

**Live peers follow the existing version negotiation.** The keeper's greeting is the standard one. A newer client asking an older monitor to stand down is the established behaviour. The TUI adds no eviction or kill policy of its own, and "it never retires one" means exactly that. An old TUI facing a newer live monitor gets `Newer` and reclassifies on the backoff, never starting a monitor while that one lives.

**The attempt's outcome crosses the keeper boundary as a typed value.** Today `attach_or_spawn_with` erases its classification into `Attached::Alone(String)`, and diagnostic text is not a control protocol. The keeper gets a typed result built from the existing `Seen` and repair classification: shared (with its connection), newer peer, other peer, unproven, busy repairer, spawn failed, or vacant-but-not-started. That result is not parsed from strings and not reached through a second lookup. Existing callers keep their behaviour.

**Monitor children are still reaped.** A monitor the keeper started stays under the existing process-lifetime reaper, and cancelling subscription or retry work never drops a `Started` child unreaped. Teardown never kills a monitor; the reaper is separate from the subscription worker.

**It attaches as an observer.** `Who::Observer` holds no delivery, never counts as an agent's waiter, and never satisfies "an agent is parked".

### Idle recovery for a waiterless agent

The plan should check, and extend only if needed, that an agent gets the monitor's existing nudge or recovery look when it is:
- bound, with auto on, and owing work;
- with no parked wait;
- and *observed* idle, from positive idle evidence.

The existing gates stay: known launch provenance, and the question, block and attendance gates. `activity: unobserved` is not idle. An agent whose activity is Unknown or unobserved is not made eligible just to produce `Nudge::At`.

## Acceptance

All lifecycle fixtures inject time, the spawner and the attachment outcomes. Nothing is spawned or connected for real.
- **Loss, then a successor.** Loss followed by a successor attaches exactly once, with no repo edit, and switches the subscription.
- **Failure, then quiet recovery.** A spawn or attach failure followed by backoff recovers at the next backoff step with no other signal.
- **Signals coalesce.** Repeated refresh, key and beat signals while an attempt or backoff is pending start no extra attempt.
- **Peers never cause a start.** Busy, `Newer`, `Theirs` and `Unproven` peers lead to zero calls to the spawner, and an old client facing a newer monitor makes no retry storm.
- **A rejecting peer that later exits:** the first reclassification gives `Newer` or `Theirs`, so the peer stays live and there are zero starts across several backoff steps. Then the peer is gone and the lease is free, and the next scheduled reclassification makes exactly one start and attaches. There is no repo edit, and backoff and coalescing still hold.
- **The keeper's typed outcome:** each classification maps to its typed variant, with no string parsing.
- **Reaping:** a keeper-started child is still held by the reaper after the keeper is cancelled.
- **The local-to-shared handoff** releases the local watcher and forces exactly one fresh rebuild that shows a change made during the gap.
- **Cancellation** stops all retry work and releases the watcher. No subscription or thread remains, and the monitor is never signalled.
- **Identity:** the keeper attaches as `Who::Observer`, and the monitor's delivery and waiter accounting never counts it.
- **Read-only paths:** plain `status` and `status --watch` never call the spawner.
- **Idle recovery:** an observed-idle, bound, auto, owing, waiterless agent with known provenance gets `Nudge::At`. The same agent with activity unobserved or Unknown does not, and neither does one with a question, block or qualifying attendance.
- **README:** in the monitor section, monitors are started by parked waits and kept up by an open status TUI.

This is one implementation of the subscription lifecycle, delivered in one commit.
