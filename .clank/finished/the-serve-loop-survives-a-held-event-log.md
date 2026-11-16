# the-serve-loop-survives-a-held-event-log

Invariants: I9.

## Why

Serving a report reads each agent's event logs under the sidecar `flock` (`github/log.rs` `SidecarLock`; shared for reads and appends, exclusive for acks, compaction and replies), blocking, on the monitor's loop: `serve_reports` → `gather` → `admitted_items` → `event_source::owed_from_sources` → `EventLog::read_rows`, and likewise `watch_store::owed`, `timer_source::Backlog::owed`, `inbox::pending` and `recovery_facts` → `admissions`. The loop's timer host (`Wake::TimersDue` → `TimerSet::due` → `accept`) loads, appends and compacts under it too. Agent and CLI processes take it exclusively: `clank events ack` (`append_checked_ack`, `compact_after_ack`), `clank send` compaction, reply operations, and a `clank wait` client's local producers. A shared read queues behind any of them, so one stalled holder stalls serving for every agent.

The loop needs a result from this lock: the unacked rows it presents, and for timers the ordering "durable append before presentation". `read_rows` swallows a lock error with `.ok()` and reads unlocked, which is not a contention outcome either.

## Build

An earlier draft made event-log readers lock-free. It is dropped (review of 2bdd2c1). That argument did not cover in-place tail repair (`repaired_log_file` truncates with `set_len` before appending), and the loop also takes the sidecar lock outside `read_rows`:

- recurring arming (`recurring_next_due`, shared);
- due acceptance (`accept_recurring_with_interrupt` and one-shot `accept`, exclusive, which later reacquire through `retained_timer_ids`, `load`, `append` and `maybe_compact`);
- the recovering backlog read that monitor report paths use (`read_timer_backlog(true)`, exclusive, syncing recurring evidence before publication).

Readers keep their locks. The loop simply never waits for one.

1. **One guard per operation, taken without waiting.** Each event-log operation the monitor's loop performs takes its sidecar lock once, at the start, with a non-blocking acquire (`try_exclusive_lock`, or a shared `LOCK_SH | LOCK_NB` twin), and holds that guard to the end. Inner steps take the held guard instead of acquiring again, so no nested or later blocking acquisition remains. That means `*_unlocked` variants of `retained_timer_ids`, `load`, `append` and `maybe_compact` under a guard type that proves the lock is held. This covers:
   - `read_rows` and its callers on the loop;
   - recovery's `admissions`;
   - `recurring_next_due`;
   - `accept`, `accept_recurring_with_interrupt` and the compaction after them;
   - `read_timer_backlog(true)`.
2. **Contention makes the source unavailable for this pass.** It never reads as empty or handled.
   - **Serve and admission:** that source's items are neither presented nor counted as handled. A report owed only that source's items parks, as if nothing new were owed yet, and is re-served when the retry below runs. Recovery and the ledger treat the source as unknown for the pass.
   - **Recovering backlog:** if recovery cannot take the lock, nothing from that source is published. The sync-before-publication guarantee is kept by withholding, not by waiting.
   - **Timers:** a recurring arm or a due acceptance that is contended is left pending, never dropped or emitted twice. Compaction is skipped for the pass.
3. **One bounded retry wake.** Any contended source arms the monitor's existing `timer_retry` deadline (`Wake::TimerRetry`), with bounded backoff (250 ms, doubling to 5 s). That wake re-runs the contended operations and re-serves parked reports. It is a bounded retry, not a poll: it is armed only by contention and disarmed once the operation succeeds. Classify it so for `no-polls-but-github`.
4. **Writers off the loop** (`clank events ack`, `clank send`, reply transactions, `clank wait` producers) keep their blocking protocol. They are not on the monitor's loop.
5. **Today's corruption handling is unchanged.** `event_source::owed_from_sources` skips a source whose `read_rows` is `Ok(None)` or an error (lines 291-293). This plan neither relies on nor changes that. A correction to the earlier draft: it does not "never read as no events" today. Record this in I9's `Today`, or leave it as a note for `the-purge-protections-return`.
6. **Registry:** I9's `Today` drops the event-log locks, and this plan leaves I9's `Plans`.

## Acceptance

With another thread holding a source's sidecar lock exclusive for the whole test, each of these completes before the lock is released. Use channels, no sleeps, and bound each by a timeout:

- a report owed that source's event parks without it, and is not told it was handled;
- a report owed nothing is served;
- a recurring timer's arming and a due acceptance are left pending;
- a recovering backlog read publishes nothing from that source;
- compaction after acceptance is skipped;
- the monitor retires.

After release, the retry wake presents the event, arms and accepts the timer exactly once, and compacts.

Also:

- No inner step of a guarded operation acquires the sidecar again. A test-only acquisition counter per operation shows exactly one.
- With no lock held, behaviour is unchanged. The existing log, inbox and timer tests pass.
