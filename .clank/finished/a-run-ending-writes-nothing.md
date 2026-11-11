# a-run-ending-writes-nothing
# When a `clank run` process ends, its attendance ends and nothing else happens: no `run_ended` record, no message, no wake

## Why

On 2026-11-11 a repo's TUI timeline showed 22 rows like `9h ✓✓ ✉ @claude "binsec core"`, one per `clank run --desc …` an agent had started. Each one was a `run_ended` record.

`all-background-work-is-attended` added them. When a run's registered process is gone, the monitor writes one durable `run_ended { desc, started, ended_seen }` record, pins its key in the agent's message log (`run-ended#`), and delivers it as a `WaitItem::RunEnded` wake through the inbox until it is acked. Because it lives in the message log, the TUI shows it as a message (`✉`).

A `clank run` is started as the harness's own background task, and the harness notifies the agent when that process ends (LLFourn, 2026-11-11: "When you make a background task through clank run, the agent is notified when that process ends. The harness does that."). So this record repeats what the agent learns anyway, and it fills the timeline. The original plan gives no reason the harness's own notification wasn't enough, and none is recorded elsewhere.

LLFourn, 2026-11-11: "I don't really know where this idea came from … maybe there is a real problem that some agent in the past noticed. But now we don't seem to have a record of what this is, so I think the safe bet is just to remove this thing."

## Build

1. **A run's end is the end of its attendance.** When the monitor sees a run's registered process gone (PID dead, or a different birth), the attendance entry leaves, as any ended entry does, and releases whatever it was holding quiet. Nothing else happens: no record, no message-log key, no wake item, no notice.
2. **Delete the record and its paths.** Remove everything that exists only for the record:
   - `WaitItem::RunEnded`, including its rendering, `wake_render` text, ledger kind and what text, and its `scroll.rs` timeline arm;
   - the durable terminal record, its writer and replay, and the `run-ended#` message-log key;
   - its inbox delivery and ack handling;
   - the tests and fixtures that exist only for it.

   Check every reader, including recovery, the ledger, `status -j` and the web UI.
3. **Old bytes.**
   - `run_ended` records already in message logs and event inboxes load without error. They are not shown as messages, are never delivered or woken for, and count as handled, so nothing re-presents them.
   - A `turn.json` or state file that references one still loads.
   - Fixture an old message log holding one.
4. **Docs.** Update the README's `clank run` and attending text, the skills' text that mentions a run-ended wake, the `clank run --help` doc, and the book reference page (regenerated).

## Tests

No harness, monitor or real process is started; process death goes through the existing seam.

- **A run's death ends its attendance, and nothing else happens:** no record, no inbox entry, no timeline row, and no wake for the agent. Work the attendance held quiet is delivered as it would be when any attendance ends.
- **Old logs:** a message log and inbox already holding `run_ended` records load. Nothing is presented or woken, and the TUI timeline shows no message row for them.
- **A killed run** behaves the same as one that exited.
- **Mutation:** restoring the record write fails the first test.

## Done when

A `clank run` ending leaves no trace except its attendance disappearing.

## As landed

- A `clank run` whose registered process is gone (dead, killed, or a reused PID) ends its attendance and nothing else: nothing is written to the message log, nothing is owed or shown, there is no wake. Unknown liveness still keeps the registration.
- The monitor serves the agent's parked wait as soon as a run is retired (`retire_gone_runs`), so work the run held quiet is delivered then. Before, that delivery came only from the record's own write waking a fold.
- Deleted: `WaitItem::RunEnded`, `WaitKind::RunEnded`, `inbox::record_run_end` and its key helpers, the `run-ended#` pinned key, and every place that rendered, kinded, presented or delivered them. Core's `ended_runs`, `end_run` and `RunEnd` are replaced by `SessionState::end_gone_runs`.
- Old bytes: legacy `run_ended` inbox rows parse as `Line::Retired { seq }` at `EventLog::parse`. They are never owed, shown or presented, are not foreign, and their seq stays reserved until compaction drops them with their acks. Frozen fixtures cover the log, `turn.json`, the ledger and receipts.
- The wire protocol goes from 20 to 21, so a protocol-20 monitor that could still send `run_ended` is declined.
- The README, the shared skill text, the check-in text, `clank run --help` and the book reference say the harness reports a run's end.
- Left open: `retire_gone_runs` takes the monitor-loop locals as arguments; folding them into the `Serving` context is a follow-up. Retired draft rows still use the older `Foreign` path.
