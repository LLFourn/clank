# attendance-survives-a-subagent-waiting
# A declared subagent stays attended while work it started is still running

## Why

On 2026-11-10 and 11 the master was woken about ten times with nothing to do. Each time a declared subagent (`clank attend "<description>"`) had started a long test run as a background shell and ended its turn to wait for it. clank dropped the attendance and re-presented the gate item; the subagent resumed minutes later and handed back.

Measured on 2026-11-11 with a recording hook on `Stop` and `SubagentStop` (subagent A backgrounds `sleep 240` and ends its turn; monitor entry omitted):

| Time | Hook | `background_tasks` |
|---|---|---|
| 11:00:46 | SubagentStop, `agent_id` A | subagent A `running`, subagent B `running`, shell `sleep 240` `running` |
| 11:00:46 | Stop (main) | subagent B `running`, shell `running`. A is absent |
| 11:01:05 | Stop (main) | shell `running` only |
| 11:04:48 | SubagentStop, `agent_id` A | subagent A `running` only. The shell is gone |

So:

- A subagent that ends its turn to wait on its own background work leaves `background_tasks`. It returns under the same id when that work exits.
- In a Stop payload, parked and finished look the same: the subagent is absent.
- A shell entry is `{id, type: "shell", status, description, command}`. Nothing names the subagent that started it.
- `SubagentStop` fires at every end of a subagent's turn, parked or finished, with `agent_id`, `agent_type`, `agent_transcript_path` and the session's `background_tasks`. The hooks reference (https://code.claude.com/docs/en/hooks) says `SubagentStart` fires on spawn and again on resume. No documented hook or field says a subagent is finally done.

`declared-work-ends-by-the-harness-listing` (PR #27) ends an attendance "when the harness's listing no longer has its task". That rule is right for a finished subagent and wrong for a parked one.

LLFourn, 2026-11-11: "I don't think there's much else we can do [than] try and guess the sub-processes that are started by a sub-agent and keep the attending open against those processes. The main agent should be stopped. There's no way for it to spawn processes."

## Build

One model, held per binding in the turn state beside the attendance it serves. The adapters only report observations; the monitor alone applies them.

### Observations

Each is a report from a hook of the bound session. None of the child reports parks, becomes the agent's waiter, marks the agent's turn ended, or consumes a wake.

- **Agent stop** (the existing turn-end report): the listing `L` as the main agent stopped.
- **Child stop** (`SubagentStop`): `agent_id` and the listing at that moment.
- **Child start** (`SubagentStart`): `agent_id` and `agent_type`, nothing else. Fires on spawn and on resume. It carries no description and no listing.

Every report also carries **when its hook started**, read by the hook from the host clock. That is the only ordering evidence there is: the harness gives none, and a child's transcript need not have grown between a stop and the resume that follows it.

A listing says what was live when it was taken. It does not say when a task was born, and a child's stop listing still shows that child as `running`: the report itself is the evidence that it stopped.

### What is kept

For the binding, whether or not anything is attended:

- **Seen:** the id of every task that has appeared in any listing, so a later declaration has a baseline.

Per declared subagent X:

- **Baseline:** the ids seen before the listing that first showed X. A baseline task is never X's.
- **As of:** the hook time of the last report applied to X.
- **State:**
  - `Live`: X is running.
  - `Parked { on, sure }`: X has stopped and `on` (task ids) may be work it started. `sure` says whether the evidence excludes the main agent as their owner.
  - `Unsettled`: a report that could not be ordered says X may be running again. Nothing about X's tasks can end the attendance from here.
  - `Ended`: the attendance is over.

### Transitions

| From | Observation | To |
|---|---|---|
| declared, unbound | any listing, an agent's or a child's, has an entry whose description is the declaration's | bound to that entry's id, `Live`, baseline = what was seen before this listing. If the listing is that same child's stop, the next row is then applied to it |
| `Live` | child stop for X; candidates = tasks in its listing that are not baseline, not X, not another subagent | `Parked { on: candidates, sure }` if any, else `Ended` |
| `Live` | agent-stop listing without X, and no child stop for X was seen (hook lost, or a harness with none) | `Parked { on: non-baseline tasks listed, sure: false }` if any, else `Ended` |
| `Parked` or `Unsettled` | child start for X's id, or X listed in any listing but its own stop's | `Live` |
| `Parked` | any later listing in which X is absent and nothing of `on` is still live | `Ended` |
| `Parked` | an unordered child start for X's id | `Unsettled` |
| `Unsettled` | child stop for X | as from `Live` |
| `Unsettled` | agent-stop listing without X | as from `Live` (the third row) |
| `Unsettled` | anything else, or nothing | stays, until `clank attend --clear` or the `--expect` check-in |
| `Parked` | nothing arrives (the work ended and the child was never resumed) | stays, until the `--expect` check-in says so |

`Ended` releases whatever the attendance held, at once, to the agent's parked waiter if it has one. It needs no further stop from the main agent.

### `sure`, and what is not promised

A candidate is known not to be the main agent's only when the main agent could not have started it: its last stop still stands when the child's report is applied (its record is exactly what its Stop hook saw, as `stop_reported` already decides), and the candidate was not in that stop's listing. Then `sure` is true.

Otherwise the main agent's turn was open at some point since the baseline, or cannot be shown closed, and the candidate might be its own. It is held anyway with `sure: false`. The monitor's present view of the agent is never substituted for the interval a delayed report covers.

So the promise is narrower than "the main agent's own shell never holds":

- A task that existed before the subagent did never holds its attendance.
- A task the main agent started while the subagent was running, in an open turn, cannot be told from the subagent's and does hold it, until that task ends or the check-in falls due. The attendance's text says so.

The recorded sequence is the second kind: the main agent's turn was open when A started its shell (both stops are in the same second), so the shell is held `sure: false`.

### Order, staleness, restart

- A child report for a session other than the binding changes nothing. One for an id no attendance is bound to changes no attendance; its listing is still tried against pending declarations (the first row) and added to what has been seen. A child start for an unbound id is only remembered as a start, since it names no description to match.
- A report is applied to X only if its hook time is later than X's `as of`. That holds for every kind, an agent-stop listing included: arriving later is not being later.
- A report whose hook time is equal to `as of`, missing, or earlier is **unordered**. What it does depends on which way it could be wrong:
  - An unordered report that could only end or park X (a child stop, a listing without X) is not applied. A late stop can neither re-park nor end a child that may have resumed, and a late listing cannot undo a newer child transition.
  - An unordered child start for X might be a resume, so it cannot be ignored: a parked X becomes `Unsettled`, where the disappearance of its tasks proves nothing. Ignoring it would let a later listing with those tasks gone end the attendance while X runs.
  - The same holds at binding. If a start for an id was remembered and the listing that binds a declaration to that id is that child's stop, the stop is applied only when it is strictly later than the start. Otherwise X binds as `Unsettled`.

  Every unordered case therefore errs toward an attendance that outlasts its work, which the `--expect` check-in bounds. None releases held work early. A host clock that steps backwards produces unordered reports and nothing worse.
- `seen`, each baseline, state and `as of` are stored in the turn state and survive a monitor restart, so a report that was unordered before a restart is unordered after it. A stored `Live` with no report since is re-read from the next later listing by the third row.
- A rebind drops them with the attendance.

### The rest

- **Hooks:** `clank setup` registers `SubagentStop` and `SubagentStart` for Claude. Both go through the stop hook's reporting path as a new non-parking report and return at once.
- **Shown:** `clank status` and the TUI's attended row say what a parked attendance rests on, for example `attended · "Implement X" waiting on shell "Run the mutation checks"`, with `(may be yours)` when `sure` is false.
- **Other harnesses:** unchanged, except that a harness with a listing and no child hooks gets the third row.
- **Docs:** the README's attending section says a declared subagent is attended through work it may have started, how that is inferred, and what is not promised.

Undeclared work is still ignored. Nothing here creates an attendance.

## Tests

No harness is started. Fixture the four recorded payloads above (redacted) under `crates/core/tests/fixtures/stop_hook/`.

- **The recorded sequence:** declared A parks on a shell → `Parked`, `sure: false`, still attended at the agent stop where A is absent; child start for A → `Live`; A's last child stop, whose listing shows A `running` and no shell → `Ended`, and the held work is released with no further agent stop.
- **Finished at once:** a child stop with no candidates ends the attendance.
- **A task older than the subagent** never holds it.
- **The agent's own shell, first seen at or after its stop:** started in an open turn while the subagent ran, it holds with `sure: false` and the text says it may be the agent's.
- **`sure`:** the agent's stop stands, a shell not in its listing appears in the child's stop → `sure: true`. The agent resumes between the two snapshots → `sure: false`.
- **Two declared subagents, one shell:** it is a candidate of whichever child's stop listed it; the other is unaffected.
- **Parks twice:** attended throughout.
- **Binding from the recording alone:** a pending declaration, a child start carrying only `agent_id` and `agent_type`, then exactly the four recorded payloads and no earlier listing of A. A binds from its own first stop listing, the shell in that listing is a candidate and not baseline, and the sequence ends as above.
- **An unordered resume:** A is parked on shell S; a child start for A arrives with an equal, a missing and an earlier hook time in turn; then a later stop listing of another child shows A running and S absent. In each, the attendance is still held and nothing is delivered, across a restart too. The same listing with an ordered start also keeps it. A later ordered stop of A with nothing left then ends it.
- **Binding after an unordered start:** a remembered start that is not strictly earlier than the stop that binds the declaration binds `Unsettled`.
- **Order:** a child stop whose hook time is earlier than the start already applied changes nothing; so does one with the same time, and one with none. An agent-stop listing taken before a child's applied start does not make that child absent. Each holds after a restart.
- **A replaced or truncated child transcript** changes nothing, because no transition reads it.
- **Child reports are passive:** one arriving while the agent is parked does not end its turn evidence, replace its waiter, or deliver its wake.
- **No child hooks:** absence at an agent stop with a non-baseline task listed parks; with none, ends.
- **Restart** between the park and the resume keeps the attendance.
- **Hooks registered:** `clank setup` writes both, and neither parks.
- **Mutation:** ending on absence alone fails the recorded sequence. Reading the child's own `running` entry as presence fails its last step. Applying a report by arrival fails the order test. Ignoring an unordered start fails the unordered-resume test. Adding a child's first stop listing to the baseline before binding fails the binding test. Treating a baseline task as a candidate fails the older-task test. Setting `sure` from the monitor's present view fails the resume test.

## Done when

A master that declared a subagent is not woken while that subagent is waiting on work it started, and is woken as before when the subagent has finished.

## As landed

- **One table in core:** `turn_policy::child::transition(Phase, Order, Sight) -> Phase`, with `Phase::{Live, Parked { on, sure }, Unsettled { since }, Ended}`. `SessionState::observe` reduces each report to a `Sight` per bound declaration. `Ended` removes the entry and releases what it held, to the parked waiter at once.
- **Kept:** per binding, `seen` (task id → earliest hook time shown) and `starts`; per declaration, `baseline`, `as_of`, `phase` and the bound task's `kind`; on the Stop, its hook time and listed ids. Old `turn.json` loads unchanged.
- **Only subagents follow the table:** `Subject::{Subagent, Other, Unknown}`. Any other kind ends when its task leaves the listing. A record with no kind follows the table only if its state shows the table touched it.
- **Order:** by hook-start time, never arrival; baseline is what was shown strictly earlier than the binding listing. An unsettled subagent carries its own barrier. Only a report strictly later than `since` moves it. A start, or a listing showing it, with a missing or earlier time removes the barrier, and it then stands until `clank attend --clear` or the check-in. An unordered stop or a listing without it leaves the barrier.
- **`sure`** only when the agent's Stop started before the child's stop, the agent's record as the child's hook read it is still that Stop's, and the Stop did not list the task.
- **Hooks:** `SubagentStop` and `SubagentStart` are registered for Claude beside the asyncrewake Stop entry (timeout 15, not `asyncRewake`); `clank doctor` checks both. A child's report holds no session, is no waiter, records no Stop, spends no outage announcement and needs no fold. It is served before the park it may release.
- **Every report carries its hook's start time** (agent Stop, child stop and start, Grok handoff).
- **Shown:** `waiting on <kind> "<description>"`, with `(may be yours)` when unsure, and `may be running again` when unsettled.
- **Not promised:** a task the agent itself started while the subagent ran holds the declaration until that task ends or `--expect` passes. So does a session's own long-running task, such as Claude's artifact monitor, when the subagent is declared before the session's first Stop. The agent's own `clank wait` (program `clank` by any path, first argument `wait`) never holds one.
- **Left open:** an agent Stop replayed to a new monitor that was taken while the subagent ran shows it at an out-of-order time, which removes an unsettled barrier, so that attendance holds until a clear or its check-in. No `SubagentStart` payload has been recorded; fixtures use the documented fields. `serve_newcomer` takes a `Serving` context.
