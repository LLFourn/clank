# reachability-is-evidence-not-inference
# The monitor says an agent is working or reachable only on positive evidence, and an agent it can't reach is shown as unreachable

Fourth of four plans from one diagnosis (LLFourn, 2026-11-10: "What is the problem with our architecture … No refactor is too ambitious"). It follows `the-monitor-alone-decides-a-wake`, `undelivered-work-is-noticed` and `no-process-outlives-its-build`.

## Why

On 2026-11-10 the TUI showed `artist4 → working 42m · attended` for a Codex agent that had done nothing since the previous afternoon. Its Stop hook had reported the turn's end on 9 Nov 16:30 and was parked. Codex keeps a turn open in its own record while its Stop hook runs, so the record still said "running". The monitor that drew the row had started at 12:23 and held no connection from that agent's waiter. It had no evidence the agent was working and none that it could be reached, and it showed "working" because that is what remains when nothing else matches:

- **"Working" is the default.** The TUI draws the working spinner "only while a current state says the agent is working, or no state is known" (`status_tui/render.rs`), and the agent state's `Working` comes from an open turn in the harness record, not from activity.
- **Reachability isn't modelled.** The monitor knows which waiters are connected (PR #25 uses "a registered waiter" for Grok's verdict), but it publishes nothing about it. `Unobserved` covers both "parked in a wait that can take its work" and "no idea".
- **The Stop report isn't used as evidence.** For every harness, a Stop hook report means the model's turn is over, whatever the harness's own record says while the hook is still running.

`undelivered-work-is-noticed` makes undelivered work visible and nudged whatever the cause. This plan makes the state the monitor publishes true, so the picture is right before anything goes wrong.

## Build

1. **Reachability is published.** For each agent the monitor records, in the agent state it already publishes, how it could deliver right now:
   - **Waiter:** a live connection from a waiter for the bound session, with since-when and the waiter's build.
   - **None:** no waiter is connected.

   The value comes from the monitor's own connection table and its beats, never from a file a waiter wrote about itself.
2. **Activity needs evidence.**
   - `Working` requires a turn that is open in the harness record, no Stop report for that turn, and record activity inside the stall window. Otherwise it's the existing silent-turn state.
   - A turn whose Stop hook has reported is over: the agent is waiting, however the harness record reads. The report is durable in the turn state, so this survives a monitor restart. Name the field used.
   - Waiting with a live waiter is the resting state (drawn as nothing, as `idle-agent-rows-say-nothing` does). Waiting with no waiter is **unreachable**.
   - No state is ever chosen because nothing else matched. The `Unknown` and "no state is known" paths draw as unknown, not as working.
3. **Shown.**
   - **TUI row:** an unreachable agent reads `unreachable <age>`, bright red when it's owed deliverable work (the ledger from `undelivered-work-is-noticed`) and dim otherwise.
   - **Spinner:** an agent row spins only for `Working` as defined above.
   - **`clank status`** says the same words; `status -j` carries the reachability value.
   - **Old state files** load: a state with no reachability reads as unknown, not as reachable.
4. **Recovery uses it.** `undelivered-work-is-noticed` deleted the "a registered waiter exempts" rule (`Suppressor::Delivering`, `Gate::Deliverer`), and recovery now takes one `Standing` per look from the harness record and the ledger. Reachability and the Stop report join that standing's inputs:
   - a turn whose Stop hook has reported is not `Running` for the standing, whatever the harness record says, so it is never held as a stall inside its window;
   - an agent that is unreachable and owed deliverable work is `Recover` at once, as an idle agent is, without waiting out a stall window meant for running turns. It goes by the harness's existing resume route under every existing guard (auto off, own question or block, attendance, interruption, native retry, budget and latches, the last look before a send). A harness with no route is `CannotResume`;
   - the published `Case` and the row read the same standing, as they do now. No second classifier.

5. **A Grok agent with no wait armed is shown, not typed at.** `the-monitor-alone-decides-a-wake` left this gap, in its implementer's words: "a Grok agent told once [of a monitor outage] has no wait armed. Until a later Stop reaches a monitor … nothing wakes it." Under this plan such an agent is unreachable and says so: `unreachable <age>`, red when owed work, with the hands-off reason `CannotResume`. Recovery does not nudge it. `resume.rs` gives Grok no resume route on purpose: typed input cancels a running turn, and a missing waiter is evidence of no delivery connection, not permission to type. Even a durable Stop report for the last observed turn can't rule out a turn begun after that observation and before the nudge.

## Out of scope

- An automatic re-arm for Grok. It needs a primitive that queues a turn without cancelling one, admitted atomically against a turn starting meanwhile. None exists; finding one is research for a separate plan.

## Implementation notes (from review)

- Stop evidence is persisted against the exact binding and turn boundary, is invalidated by a genuinely newer turn, and comes only from a turn-end report, never from SessionStart or PostCompact.
- Only an admitted deliverer for the current tool and session counts as a waiter: not observers, helpers or a stale binding's connection. The waiter's build is carried on its own connection, not inferred from the monitor's.
- Unknown stays explicit: bytes from before this plan, and ages that can't be dated.

## Tests

Fakes and a paused clock; no monitor, harness or pane is started.

- **The artist4 case:** a Codex record with an open turn, a durable Stop report for it, no connected waiter. The state is unreachable, not working; the row is red once a message is owed; recovery nudges without waiting for the stall window.
- **Grok after an outage:** a Grok agent with no registered waiter, once a monitor is serving again, is unreachable with `CannotResume` shown, and nothing is typed at it.
- **The race that forbids typing:** an agent with a durable Stop report and no waiter starts a new turn between the look and the send. The Stop evidence no longer applies to the latest turn, and no nudge is typed into the running turn, for any harness.
- **Who counts as a waiter:** an observer, a helper, or a connection from a replaced binding does not make an agent reachable.
- **A real working turn:** an open turn, no Stop report, record activity inside the window → `Working`, with the spinner.
- **Resting:** Stop reported and a live waiter → the quiet resting row.
- **A monitor restart** with the Stop report on disk keeps the agent waiting, not working.
- **Unknown stays unknown:** no readable state draws as unknown, never as working.
- **Old bytes:** a state file from before this plan loads with reachability unknown.
- **Mutation:** treating an open harness turn alone as `Working` fails the artist4 test. Reading reachability from anything but the connection table fails a test where a stale file claims a waiter.

## Done when

Every state the monitor publishes about an agent is backed by evidence it holds: `working` means activity it saw, a quiet row means a waiter it's connected to, and anything else is shown as unreachable or unknown.

## As landed

- **Reachability** (`agent_state::Reachability`, published as `AgentState.reachability`; `status -j` at `agents[].state.state.reachability`): `Unknown` (default, legacy bytes, and a monitor that has served less than `WAITERS_SETTLE`, about 33s), `Waiter { since, build }`, `NoWaiter { since: Option<u64> }`. Built only from the monitor's connection table (`monitor::waiter()` / `waiters()` → `Driver::reach()`). A deliverer is an agent connection with a holder, an open socket, and no report or one that parks; it is an agent's waiter only while its tool and session are the binding recovery holds. The waiter's build travels on its own connection (`Hello.build`).
- **Activity carries its evidence:** `Activity::Working(Moving { moved, until })` needs an open turn, no standing Stop for it, and a record write inside `recovery.stall_warn`; a reader expires it at `until`. A silent open turn is `Activity::TurnOpen`.
- **The durable Stop:** `turn_policy::Persisted::stopped` in `agents/<label>/turn.json`, `Stopped { turn, position, read_to: Option<ReadTo { file, len }>, at }`. Set only by a turn-end report of the bound session, placed or not; cleared by a delivery, any notify, the deadline re-arm and a rebind. The hook (`wake_repeat::place()`) reports the boundary, else the turn's name, and always what it saw of the record, measured before it scanned.
- **One resolution** (`recovery::stop_reported`, held beside the record's turns in `Snapshot.stop`, read through `Snapshot::latest()` by the published activity, the `Standing` and the last look before a send). A placed Stop ends its turn while the boundary is unmoved. An unplaced one ends the latest turn only while the record is exactly the hook's `ReadTo`. A Stop naming another turn says nothing. Anything else is `Stop::Unplaced`: unknown, the unreadable-record standing, nothing typed. Receipt time is never evidence that a record was unwritten.
- **A lost connection dates unreachability and ends no turn.** `Turns::lost` records `Unwatched { since }` only.
- **Recovery:** `Ctx.no_waiter` is the one new input to `standing()`. A turn over by the record or by a standing Stop is `Recover(Idle)`. No waiter and no resume route is `HandsOff(CannotResume)`: Grok shows `unreachable <age> · not nudged` and is never typed at.
- **Shown:** the spinner draws only for `View::working(now)`. No state or a stale one draws `unknown`. `unreachable <age>` is said once a turn has been over with no waiter for `UNREACHABLE_AFTER` (20s); `View::alarm()` returns `Alarm::{Undelivered, Unreachable, Interrupted}`, red when work is owed and dim otherwise.
- **Deleted:** `proven_working`, `Stop::ParkLost`, `Activity::Unwatched`, the disconnect-time cursor, `Snapshot::set_unwatched` and any rewriting of the record's turns, `stop_hook::native_boundary`. Renamed in recovery: `Watched.unreachable` → `no_pane`, `Answer::Unreachable` → `Answer::NoPane`.
- **Behaviour changes:** records are read for an agent with no waiter even when it is owed nothing. A wait lost with no Stop over an open turn follows the stall path instead of an idle nudge at 180s. With no monitor running, rows with a pane read `unknown`.
- **Old bytes:** a state with no `reachability` reads `Unknown`; `{"is":"working"}` loads as `TurnOpen`; `{"is":"unwatched"}` as `Unknown`; a `stopped` with neither position nor `read_to` ends no turn.
- **Deviations:** the two settle periods are additions. "Unreachable and owed" is recovered through the idle episode's own 180s settle, not instantly.
- **Left open:** the plan bar still takes its verb from the plan state. A record with no turn yet publishes `Unknown` while the standing recovers it as idle at its prompt. A Stop report reaches recovery at the next fold, up to about 5s. Between a rebind and the next fold the old binding's connection can read as the old session's waiter. An OpenCode or Grok turn-end report names nothing and sees no record, so it only ever says nothing or unknown.
