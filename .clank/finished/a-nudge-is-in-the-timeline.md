# a-nudge-is-in-the-timeline
# Every nudge an agent receives is an event in the timeline

## Why

Clank pushes work into an agent's terminal in several ways, and none of them leaves a
trace a person can see in the log:

- `clank nudge` (`cli/nudge.rs`): interrupt and submit, by hand. By design it "writes
  nothing to disk".
- provider-error recovery (`recovery.rs` `Act::Dispatch`): a retry through a parked
  recovery helper, or Codex's `turn/start`, after an overloaded or rate-limited turn;
- the follow-up plan `the-agent-state-drives-nudges`, which nudges an idle agent whose turn
  it is.

When an agent suddenly starts working, nothing says clank started it, or why. When a nudge
fails, nothing says it was tried. The user: "It would be good to have an event in the
timeline which shows nudges no matter how they are triggered."

## What to build

A nudge ATTEMPT is the unit, and its outcome is only what its transport can actually tell
(codex on 168ad7a). Intent to dispatch, transport acceptance, an ambiguous acceptance and
the agent starting are different facts. This plan records the first three honestly and
never claims the fourth.

- **Outcomes, as each transport knows them.**
  - `refused(reason)`: nothing reached the harness. Examples: no pane, no launch
    provenance, input not empty, helper unavailable, Codex `Refused`/`Unavailable`.
  - `submitted`: the transport accepted it. This is not proof the agent read it or started.
  - `reported`: a recovery helper reported it is submitting the wake; the native wake is
    unconfirmed. Claude's helper writes its `Submitted` reply BEFORE it prints the wake and
    exits 2, because it cannot answer afterwards (`stop_hook::run_recovery`). So the reply
    is a report of intent, and an interruption before the exit can still prevent the wake.
    The row reads `↻ @claude nudge reported by helper · wake unconfirmed`.
  - `indeterminate(reason)`: it may have reached the harness. Examples: pane_input's
    unconfirmed interrupt or partial submission, Codex `Indeterminate`. Like recovery today,
    it is never retried automatically.
  - `failed(reason)`: an error after which nothing can have reached the harness.

  The one-line row says what happened: `↻ @claude nudged`, `↻ @claude nudge refused: no
  pane`, `↻ @claude nudge indeterminate: interrupt unconfirmed`. It never says "nudged"
  for anything but `submitted`.
- **One recording owner per path, at its transport boundary.**
  - `clank nudge`: a wrapper around its attempt records once, after the submit returns, or
    on an early refusal before any submit. Repeated manual nudges are separate attempts,
    even with identical text.
  - provider-error recovery through a helper: the monitor records, when it processes the
    helper's reply (its existing result handoff through `reply_path`), never at
    `Act::Dispatch` creation or queue admission. It records the helper's report as
    `reported`, never as `submitted`. The helper's reply and exit protocol is unchanged: it
    adds no audit I/O and waits on no journal write. The monitor's write may land before or
    after the helper exits; the two are independent. A dispatch no helper took is recorded
    as `refused(helper unavailable)` by the monitor.
  - Codex `turn/start`: the monitor records `start_if_failed`'s result class.

  Recovery attempts are identified by their existing `AttemptRef` (episode and attempt), so
  the dispatch, the helper's reply and the monitor's processing cannot each make a row. The
  trigger, error class and text are captured with the attempt, never read later from
  whatever episode is current. `the-agent-state-drives-nudges` uses this same recorder.
- **A history store of its own, not an event source.** Each agent gets
  `.clank/agents/<label>/nudges.jsonl`: append-only lines of `{at, attempt_id, trigger,
  text, outcome}`. It is not an `EventLog`, so it can never enter unhandled work, wait
  replies, acks, reply targets, presentation records, wake receipts or repeat counts, and
  it needs no fake ack or baseline flag. The timeline reads it as a separate item kind,
  rendered with `↻` and no delivery mark; its page shows the trigger, text and outcome,
  with no actions. One `attempt_id` is one row, and duplicates collapse on read. It
  survives reloads, and event-log compaction never touches it. It is small, so it has no
  retention policy of its own yet.
- **Small synchronous audit I/O, after the transport, never before it.** Every record is
  written after its transport has returned and its outcome is fixed, as one appended line
  with no lock, monitor connection, binding or inbox lookup, and no retry. Its failure is
  ignored: it cannot change the nudge's outcome or exit code, trigger a resend, or stop the
  nudge, which has already happened. The honest tradeoff: the write is ordinary blocking
  file I/O, so a slow filesystem can delay its caller by that write. For `clank nudge`,
  that is the command's exit. For the monitor, it is the processing it is doing, and so
  anything queued behind it. No transport depends on it: the helper never waits on it, and
  the monitor writes only after a result is fixed. So it cannot retract a result or cause
  a resend. An unwritable store loses the row, and nothing else.
- **The journal is read like the rest of the timeline.** A torn final line (a crash
  mid-append) and a malformed line are skipped, not fatal. Concurrent writers each append
  whole lines (one `write` of a line under `O_APPEND`), and the reader tolerates any
  interleaving of whole lines. The timeline reads it on the snapshot and on-demand paths
  it already has, never per frame.

## Acceptance

- `clank nudge`: an early refusal (no pane, no provenance) is recorded `refused` and still
  exits with its error. A good submit is `submitted`. An unconfirmed interrupt or partial
  submission is `indeterminate`. Two identical nudges make two rows.
- Recovery: helper unavailable is `refused`. A helper's reply is `reported`, also when the
  test holds the helper after its reply and before its stderr and exit, and when it dies
  there. The row then still says the wake is unconfirmed; it claims no start, and earns no
  receipt. The helper has no new dependency on the journal. The
  Codex result classes map one to one. The dispatch, the reply and the monitor's processing
  of one attempt make one row.
- Rows survive a reload and an event-log compaction. They are never unhandled work, never
  in a wait reply, never acked, never a receipt, and never counted; they show `↻` and no
  delivery mark.
- With an unwritable store injected, a fake submitter's call and result are unchanged, and
  so is the command's exit. Recording runs only after the submitter returns: a test
  ordering the calls fails if the record precedes it. The helper path writes nothing.
- A torn final line and a malformed line in the journal are skipped, and the rows around
  them still render.
- Mutation-checked: a path that nudges without recording, "nudged" shown for a
  non-submitted outcome, a row routed as work, a recorder able to change the outcome, and a
  record written before the transport returns each fail a test.

## Out of scope

When to nudge (`the-agent-state-drives-nudges`); the stall notice (replaced there).

## As built

- `cli::nudge_log`: `Record {at, attempt, trigger, text, outcome}`, appended to
  `.clank/agents/<label>/nudges.jsonl` as one `write` per line, never retried (a short
  write is a torn record; a torn last line gets a leading newline so the next record is
  not glued to it). It is read as bytes, line by line, so a record torn inside a UTF-8
  character costs only itself; torn and foreign lines are skipped, and each attempt is
  read once. `Record::summary` is the row's words, and says
  "nudged" only for `Submitted`.
- `clank nudge`: `nudge_with` awaits the attempt, then records it (`record` ignores its
  own failure), then returns the attempt's result unchanged. `pane_input`'s
  interrupt-then-submit now returns a typed `Unconfirmed` (`Empty`, `Interrupt`, `Submit`)
  instead of an `anyhow` chain, so the outcome is read from the result's kind, never from
  error text: `Empty` → refused, `Interrupt`/`Submit` → indeterminate, a refusal before
  the terminal (no zellij, no pane, no provenance) → refused. Its messages are unchanged.
- A retry's history belongs to the attempt, with a lifecycle of its own, not to
  recovery's control state or the live roster (codex on ecac36c, 5c94e9c, 437ae69).
  - **Captured at dispatch.** When the `Driver` emits `Act::Dispatch`, it records a
    `Pending` attempt: its `AttemptRef`, its failure class, its transport, its
    `wake_text`, and its own answer deadline (dispatch + `ANSWER_WITHIN`). The pending
    set is saved beside the agent as `nudge-pending.json` before the dispatch leaves the
    driver.
  - **Settled once, by the driver itself.** `settle` appends the row to the agent's
    journal and only then retires the pending attempt (`settle_with` takes the append,
    so a test can stop the process between the two). A stop after the append and before
    the retire leaves both, the attempt settles again on the next start, and the
    journal's per-attempt deduplication keeps one row. A failed append loses the row,
    never a retry. There is no hand-off act to the monitor any more.
  - **What settles it:** its first answer, from `answer` or a helper's reply, before
    and regardless of the episode's decision. Replies are read beside current agents
    and beside every agent with an attempt pending, roster or not. Otherwise its own
    deadline, which is part of `next_due`, settles it in `due` as `Indeterminate: no
    answer came`. `due` runs the reply pass first, so a reply already on disk (a
    monitor restarting past the deadline, with the timer firing before the first beat)
    is recorded as its own outcome, never replaced by "no answer came".
  - **Restart.** `discover_pending` reads every agent directory's pending file once,
    when the monitor starts (`monitor.rs`, before its loop; that one call is not under a
    test, since the monitor's start is not unit-tested). So an attempt whose agent left
    the roster, or whose monitor restarted, is still settled, and nothing is
    dispatched again. A settled attempt leaves no file and no deadline.
  - A helper's `Submitted` is `Reported`; only Codex's `turn/start` is `Submitted`. The
    helper process is unchanged.
- Timeline: `timeline_snapshot` reads every agent's journal into `nudges`, never into
  `events`. `interleave_with_plain` merges nudges with events under the same floor.
  `TimelineItem::Nudge` and `OnelineRow::Nudge` render as `↻ @agent summary` with an age
  and no delivery mark, in the TUI, `clank log` (human, oneline, and a `nudge` JSON
  kind), and the remote page's ledger. Enter in the TUI opens a read-only page
  (`OverlayData::Typed`, `render_typed_doc`). It shows the summary as prose and the text
  through `push_body`, which keeps indentation, spacing, tabs and blank lines, and makes
  control characters safe.
- Mutation-checked, each caught: an unrecorded manual nudge, recording before the
  transport, a recorder changing the result, "nudged" for a helper's report, a helper's
  report recorded as submitted, nudges escaping the floor, a torn line gluing the next
  record, a delivery mark on a nudge row, lost deduplication, an answer's record gated on
  recovery's control state, one attempt recorded twice, the page reflowing the text, the
  journal read as text, a pending attempt with no deadline of its own, one never expired,
  one not reloaded after a restart, one not saved, one not reclaimed, retiring before
  appending, an off-roster agent's reply ignored, no startup discovery, and expiring
  before reading a reply already on disk.
