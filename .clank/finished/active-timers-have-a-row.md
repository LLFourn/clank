# active-timers-have-a-row
# Active timers have one row in the sources panel, listing them on Enter

## Why

`a-source-is-its-producer` took timers out of the TUI's SOURCES panel, following the
user's remark that a timer is built in. The user now wants them back in a better shape: "In
the tui sources it should have timers if there are any active and pressing enter on them
should show them." Timers are worth seeing, because they are an agent's pending reminders.
What was wrong before was one row per timer, each reading a bare "hosted". In
`~/src/life` that was five rows beside the real producers.

Each timer has a name (`rama-follow-up-2026-10-13`, `daily-inbox-review`), a `due_at`, an
optional `repeat_every`, and the content it delivers.

A timer's state is not a function of the clock (codex on c93f021). An overdue one-off
that nobody has accepted is OWED, not fired: `TimerSet` arms it immediately and keeps
trying, including after time offline. A one-off counts as fired only when its exact
occurrence, `timer/{due}`, is durably accepted (retained WAL evidence and the `Seen`
index, as `timer_source::accept` checks). A recurring timer's next fire comes from its
generation WAL. An occurrence awaiting ack means no new deadline. Uncovered past slots
mean a catch-up is owed. Only otherwise is it `next_due(covered)`.

## What to build

- **A read-only timer inspection, shared with the scheduler's own folds.** Add
  `timer_source::inspect(dir, source, now) -> TimerState`, built from the same pieces the
  scheduler uses: the one-off acceptance evidence by exact id, and for recurring timers
  the strict WAL fold plus `Recurrence::catch_up` / `next_due`. It never constructs or arms
  a `TimerSet`, never calls `accept_recurring`, never takes an ingest lease, and never
  repairs `Seen`, compacts, quarantines or creates a file. Where an existing helper mixes
  inspection with writing, the read half is split out and both callers use it. The states:
  - one-off: `Scheduled(due)` (future, not accepted); `Owed(due)` (past, not accepted);
    `Fired` (its exact occurrence durably accepted, which holds whatever the clock now
    says, so a clock rollback cannot revive it);
  - recurring: `Next(due)`; `CatchUpOwed(due)`; `AwaitingAck`;
  - either: `Unknown(why)` when the evidence cannot be read or is damaged. Missing evidence
    (no log yet) is NOT unknown: it means nothing has been accepted.
- **Active is every state but `Fired`.** The panel never hides an owed or unknown timer,
  and never says one fired without evidence.
- **One `⏰ timers` row when any timer is active,** after the other sources. It names the
  roster agents with active timers, each once with a count (`@assistent ×4`). It then says
  what is soonest among timers with a meaningful time: `next in 3h`, or `1 owed` when any
  is owed. When some timers are only awaiting ack or unknown, that is named too (`· 1
  awaiting ack`). No active timer, no row.
- **Enter lists the active timers,** owed and unknown first, then soonest due. Each line
  gives its name, agent, state in words (`in 3h`, `owed since 2h`, `awaiting ack`,
  `unknown: …`) and recurrence (`every 1d`). The list keeps its cursor on the same timer,
  by (agent, source key), across refreshes, and Back returns to it. Enter on a timer opens
  the existing source page with its configuration (content included), state and events,
  and no restart: a timer has no process.
- **Reads stay on the snapshot path.** The inspection runs where the snapshot is built and
  in the sources fingerprint, so state changes refresh the panel. Rendering is pure: the
  states carry absolute times, rendered against the frame's clock. No polling loop, no
  per-frame file reads.
- Timers are roster agents' only. Producer rows' wording and restart are unchanged.

## Acceptance

- life's shape as a fixture: one agent with two recurring timers, two future one-offs, and
  one past one-off whose occurrence IS accepted (evidence written by the real
  acceptance). The row reads `@assistent ×4`, and Enter lists exactly the four.
- Lifecycle, each against `inspect`:
  - a past one-off with no evidence (offline, no monitor) is `Owed`;
  - a past one-off whose acceptance failed is `Owed`;
  - an accepted one-off is `Fired`, also with the clock rolled back before its due time;
  - a recurring timer with uncovered past slots is `CatchUpOwed`;
  - one with an occurrence awaiting ack is `AwaitingAck`, and `Next(next_due)` after the
    ack;
  - compacted coverage still yields the right `Next`;
  - a damaged WAL or `Seen` is `Unknown` and still listed.
- `inspect` writes nothing: the events directory is byte-identical before and after, and no
  lease or lock file is created.
- Enter on a listed timer opens its page, with content and no restart. Back returns to the
  same timer after the list re-sorts.
- Mutation-checked: treating a past one-off as fired by the clock, inventing a next time
  for an awaiting-ack timer, hiding an unknown timer, and offering restart on a timer each
  fail a test.

## Out of scope

Creating, editing or cancelling timers from the TUI; what a timer delivers; the scheduler
itself.

## As built

- `timer_source::inspect(dir, source) -> TimerState` (`Due(at)`, `Fired`, `AwaitingAck`,
  `Unknown`). It reads the WAL's text and the `Seen` files only. It opens no `EventLog` (whose
  `open` makes the directory), takes no `SidecarLock` or lease, and writes nothing. The
  reads share the scheduler's own parsing: `EventLog::retained_timer_ids_in` is the text
  half split out of `retained_timer_ids`, and `github::log::recurring_progress` runs the
  strict recurring `fold` and returns its coverage and pending obligation. A one-off is
  `Fired` when its exact `timer/{due}` is in the WAL or durably in `Seen`; a damaged
  `Seen` entry is `Unknown`; otherwise it is `Due(due)`. A recurring timer is
  `AwaitingAck` while an occurrence is pending, else `Due(next_due(covered))`.
  `Due(at)` stays clock-free: whether it is "in 3h" or owed (a catch-up, for a
  recurring timer) is the frame clock's call, in `render::timer_words`.
- `timer_source::config_key` gives the scheduler's storage key, the generation's for a
  recurring timer. `ConfiguredSource::of` uses it, so a timer's page finds its events.
- `source_status::configured` lists the roster's timers again, except `Fired` ones, with
  `ConfiguredSource.timer` set. The sources fingerprint includes each timer's state.
- TUI: `source_groups` leaves timers out of the producer rows. `PanelRow::Timers` is the
  one `⏰ timers` row (`row_agents`, `timers_summary`). `Mode::TimersList` lists them in
  `input::active_timers` order: unknown, then by due time, then awaiting ack. That
  order is clock-free, so a due time passing moves no cursor. The cursor follows its
  timer across refreshes (`rebind_list_sel`, renamed from `rebind_member_sel` now that
  two lists use it). A timer's page is the source page, with its state in words; Back
  returns to the list on that timer. `restartable` is false for timers.
- `until_next_due` caps the loop's wait at the soonest scheduled timer's due time, so an
  idle pane turns it owed when it happens, with no file change and no polling.
- Mutation-checked, each caught: fired by the clock, a next time invented while awaiting
  ack, the `Seen` tier ignored, the WAL evidence ignored (the crash gap before the `Seen`
  write), the name key for a recurring timer, hiding an unknown timer, unknown sorted
  last, waking for an owed timer, dropping the owed count, and restart on a timer.
