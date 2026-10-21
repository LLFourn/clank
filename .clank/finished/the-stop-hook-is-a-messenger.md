# the-stop-hook-is-a-messenger
# The stop hook is a messenger

## Why

Lloyd: "Stop hooks are meant to fully delegate their decision to the
monitor … They're meant to be dumb." They are not.

The hook started as a thin adapter (b0a91b2c, 2026-05-23): resolve the
agent, spawn `clank wait`, map the result to an exit. Since then each
feature has put its decision where its evidence arrived, and the hook's
stdin is the only place claude's `background_tasks` list appears:
- yielding on background work (ca9d357c);
- attendance proof, the marker take and the `attended` projection
  (d239bbe1, 16fb97dc);
- the check-in timer (dcf1c649);
- repeat escalation, question detection and delivery receipts.

`stop_hook.rs` is now 7,925 lines. Meanwhile the monitor gained its own
decision about the same agent (idle recovery and nudges: 38b055be,
66c32335). That decision reads the hook's files to learn what the hook
decided.

Two deciders hold one agent's state, linked by files one writes and the
other infers from. The idle-recovery bug is that link failing: the hook
takes the `attending` marker, and the monitor reads its absence as "idle"
and types a recovery prompt into an agent that is correctly waiting. Fixing
links one at a time is the treadmill this plan stops.

## The model

The **monitor owns every decision about an agent's turn-ends**. The
hook is a messenger. One monitor state machine per (repo, agent,
session binding) decides, and the provider hooks carry evidence in and
render verdicts out.

### The hook

1. **Routing (transport, not policy).**
   - It resolves the repo, label and binding exactly as today.
   - An unbound or unmanaged session exits Silent without attaching to
     or spawning a monitor: clank does not own it, and learning that must
     not cost a per-repo monitor.
2. **Report.** On Stop it sends one `TurnEnd` report over the connection
   `clank wait` already opens. The report carries:
   - tool, session id, turn boundary and question evidence;
   - the hook's deadline;
   - the task snapshot, as one of:
     - `Known(tasks)`: the provider sent `background_tasks`, possibly `[]`.
       It holds the COMPLETE observed entries, every field, kept as
       observed JSON alongside the typed fields clank reads.
     - `Unavailable`: the provider omits the field (codex today). This is
       never read as "tasks completed".
3. **Verdict.** It holds that one owned connection until a terminal
   verdict. `Park` is not terminal: the monitor later sends the terminal
   verdict on the same connection, and the adapter never starts a second
   wait. The terminal verdicts are `Silent`, `Deliver(items, presentation)`
   and `Diagnostic`. The hook maps each to the tool's wire output.
4. **Delivery result.** After emitting a `Deliver` it reports
   `Delivered(presentation)` or `EmitFailed(presentation, why)` on the
   same connection, then closes.
5. **Nothing else.** The hook keeps no marker reads or writes, no
   projections, no timers, no auto-mode, role, repeat or question logic.
   The stdin ownership sentinel stays: it is transport, so a killed hook
   still reaps its wait. If the monitor is unreachable, the hook uses the
   existing attach-or-spawn.
6. **Alone mode never silences.** `monitor::Attached` deliberately has
   no error variant. `wait` degrades to `Attached::Alone`, a local fold,
   on decline, protocol skew, a wedged monitor or lease trouble, and every
   protocol bump exercises that path. That design stays: going quiet is
   the one outcome it may not produce. Alone mode runs no attendance,
   check-in or recovery policy, and it never delivers items. Its whole
   contract:
   - **No second policy.** It reuses the existing generic owed-work fold
     to learn only whether work is owed. Nothing is presented from it.
   - **Safety kept:**
     - stale binding: Silent, as routing;
     - auto-off from config: today's NotOurs(AutoOff);
     - a pending question or block: no wake, read only from the monitor's
       persisted projection, never written.
   - **Never quiet on attendance.** Attendance is never honoured, and no
     check-in or recovery runs.
   - **Owed work wakes the agent with a Diagnostic**, never a Deliver:
     "clank's monitor is unavailable; work is owed — run `clank status`".
     A Diagnostic is not a presentation, so no receipt is created and
     nothing is committed. The owed items stay owed in the monitor's
     custody, whether the emit succeeds or fails.
   - **When the monitor is available again,** it presents those items at
     the next TurnEnd with its own presentation identity, and commits
     the receipt on `Delivered`, exactly once. The fallback holds no
     state, so there is nothing to reconcile.
   - **It touches no forbidden file.** The source gate covers the
     fallback too, except for the read-only projection read.

   Selecting work in Alone mode is never treated as delivering it.

### Monitor state

The state is per (repo, agent label, session binding), bounded per live
session:

- **binding:** the bound session identity, using the existing
  session/turn/presentation identities. No new handoff keys.
- **auto mode, role, question/block:** read through the existing
  watchers.
- **attendance**, one of:
  - `None`;
  - `Intent { selector, since, expect, shell_proof? }`;
  - `Standing { task_id, kind, shell_proof?, since, expect, check_in: Armed | Fired }`.
- **repeats, outstanding presentation, recovery episode.**

### Qualified attendance: the one rule

Live work and a guaranteed completion wake are different facts. One
function, `qualifies(attendance, latest_snapshot, facts, now)`, decides
whether an attendance may quiet anything. TurnEnd, CheckInDue and
Recovery all call it, and none of them re-derives any part of it. An
attendance QUALIFIES only when every one of these holds:

1. **Completion-wake capability.** The provider delivers a completion
   wake for background work. Today's `wakes_on_background_completion`
   (`LoopPolicy::TaskCompletionWake`, i.e. claude) moves to the monitor
   unchanged.
   - Codex and OpenCode (`InHookWait`) never qualify. Their attendance
     is recorded and shown, but their turn-ends Park on the hook's owned
     wait, exactly as today's `Attendance::Declined`.
   - Grok keeps its native handoff.
   - A live pid never stands in for this capability.
2. **Fresh task correlation.** The latest `TurnEnd`'s `Known` snapshot
   lists the attended task id, not `completed`.
   - A `Known` snapshot that lacks the task, or marks it completed, ENDS
     the attendance, even if a recorded pid is still alive.
   - An `Unavailable` snapshot establishes no correlation, so the
     attendance does not qualify. It is not ended either: the attendance
     stays recorded, and the turn-end Parks on the hook's wait. It is
     evidence of neither completion nor liveness.
3. **Process identity, for a shell.** For a shell task (`clank run`, or
   `--pid`), the pid + birth token still hold, in addition to rule 2.
   - A listed shell with no recorded process proof does not qualify. The
     resolution diagnostic says to use `clank run`, and the turn-end
     Parks.
   - Subagents need no pid.
   - Unknown task kinds never qualify, pid or not.
4. **Before the deadline.** `now < since + expect`, from the original
   `since`. It is never renewed.

After the deadline the attendance stays RECORDED, so status shows it
overdue, but it no longer qualifies:
- TurnEnds Park or Deliver normally;
- Recovery treats the agent as it would any other;
- CheckInDue has fired once.

To be quiet again, the agent must `clank attend` again, which starts a
new `since`.

### Events

Each event is a transition, pure in (state, event, facts) → (state,
effects). Facts the monitor collects at evaluation time are inputs, never
cached booleans:
- now;
- pid + birth-token liveness for a shell proof;
- the current binding and config.

1. **`TurnEnd(report)`.** Binding is validated first. A report whose
   session is not the current binding mutates nothing and is answered
   Silent(stale). After that, the existing precedence order is preserved.
   The inventory records the exact order of today's `decide`: stale
   binding, auto-off, pending question/block, then attendance, then
   items/park. Attendance gets no unconditional priority. Within the
   attendance step:
   - **`Intent`:** resolve the selector against THIS report's `Known`
     snapshot. An `Unavailable` snapshot resolves nothing: the intent
     stays and the turn-end Parks.
     - **Exactly one live match** becomes `Standing`, bound to that task's
       id. From then on the identity is the task id, never the name
       re-matched. Whether it silences is `qualifies`, evaluated on this
       same report.
     - **Zero or ambiguous matches** produce `Deliver` of a diagnostic
       listing the live tasks (noise, never silence). The intent is
       dropped.
   - **`Standing`:**
     - silenced iff `qualifies`;
     - ended by rule 2's missing/completed case;
     - otherwise (not capable, `Unavailable`, or past the deadline) it
       stays recorded and the turn-end proceeds normally to items or
       Park.
2. **`Attend(intent)` / `Clear`**, sent by `clank attend` /
   `clank attend --clear` as a client request on the existing protocol:
   - The binding is validated first, and a request from a non-current
     binding is refused.
   - `Attend` replaces any existing intent or standing attendance and
     cancels its check-in.
   - The command reports *intent accepted*, for example "attending
     `<selector>` from your next turn-end". It never claims proof.
   - `--expect` starts at acceptance, matching today's "clock starts at
     the marker".
3. **Task completion or disappearance** is observed only through a later
   `TurnEnd` whose `Known` snapshot lacks the task or marks it completed.
   The harness's completion notification ends a turn, which produces that
   report. Nothing polls for it.
4. **Binding change, auto-mode change, question/block change**, from the
   existing watchers:
   - a new binding drops the old binding's attendance and outstanding
     presentation;
   - auto-off and a question keep their current precedence.
5. **`CheckInDue`:** the nearest deadline in the monitor's existing event
   loop, at `since + expect`. If the attendance is still `Standing`, not
   ended and `check_in` is `Armed`, the check-in is delivered once through the existing nudge/recovery
   transport. That route already reaches an agent whose hook exited
   Silent. Then `check_in` becomes `Fired`.
   - No hook-owned timer remains.
   - Repeated `TurnEnd` reports and unrelated wakes neither renew
     `since`/`expect`, consume a standing attendance, nor arm a second
     check-in.
6. **`Delivered` / `EmitFailed(presentation)`:** the receipt commit point.
   - Choosing items, or writing a verdict to the socket, commits nothing.
   - On `Delivered` the monitor commits the receipt and repeat accounting,
     with today's `ReceiptCapture` meaning unchanged.
   - **A result is connection-scoped.** Only the connection that carried
     a presentation can answer for it, and only while it is in flight.
   - **`EmitFailed` is terminal.** That covers an emit error the hook
     reports, and a connection that closes (the hook died, or its
     deadline closed the wait) without a result. The record is
     discarded, and no later success for it can exist.
   - **The items remain owed**, and the next TurnEnd presents them
     FRESH: a new identity, counted from the last committed repeat
     record.
   - **At-least-once and exactly-once.** Delivery to the agent is
     at-least-once. Receipts are exactly-once, because only a
     `Delivered` on the live connection commits.
   - **Stray results.** A result for anything not in flight is a
     protocol violation and commits nothing. A repeat of the result
     that just settled a presentation is a no-op duplicate.
   - **A break while in flight.** If an owed-work change is observed
     while a presentation is in flight, its episode has ended, even if
     the same work is owed again later. Its `Delivered` still commits
     its receipts exactly once, but it does not restore that episode's
     repeat count. The next presentation counts from 1.
7. **Monitor restart or retirement.**
   - Intents and standing attendances persist with their original `since`
     and `expect`, through the persistence the monitor uses today. The
     monitor is the only reader and writer.
   - On load, a standing attendance is `Standing` but unproven until the
     next `TurnEnd` revalidates it. A cached task list never buys quiet
     past the original deadline.
8. **Recovery** asks `qualifies` against the latest snapshot, now's pid
   liveness and the deadline. It reads no files and has no rule of its
   own. A qualifying attendance means the agent is not idle, and anything
   else lets recovery proceed as it would for any agent. This is the
   idle-recovery bug's fix by construction. The turn-end evaluator and
   the recovery loop cannot disagree, because they are one function.

### Projection

Status and the TUI read the monitor's published projection. The monitor
is its only writer, through the existing agent-state publication. Nothing
reconstructs a decision from a file's absence.

### `clank run`

`clank run` submits an `Attend` intent with a run-description selector
and its own pid + token before exec. If exec fails, it sends `Clear`
before exiting non-zero, so a failed launch leaves no intent.

## Hard constraints

- **No new sockets, no new listening ports, no new IPC/admin channel.**
  - Reports, verdicts, delivery results and attend/clear requests ride
    the existing `clank wait` ↔ monitor protocol.
  - Bump the protocol and use the existing build-retirement path
    (f24b59d2).
- **Resources:**
  - the monitor's existing event loop and nearest-deadline timer;
  - bounded state per live session;
  - no periodic task-list polling, no new permanent helper, no per-hook
    timer task, no repeated process scans. A pid check happens only when
    a transition evaluates a shell proof.
- **Don't restart live agents, TUI, tunnel or monitor** to land this.
  Install only after `clank finish`, and the newer monitor takes over
  through retirement.
- **Codex and grok hooks** get the same messenger shape. Codex reports
  `Unavailable`. Grok's native handoff route is preserved.
- **Existing invocations keep working.** `clank attending <task> --desc …
  --pid …` keeps working through the hidden `attending` alias.

## Order of work: two reviewed commits

A single pass was attempted and stalled at the size of the whole change.
It splits at exactly one boundary, the one with its own safety rationale.
There is no intermediate state in which two components own policy for the
same agent.

**Done before M1 (committed with this revision):**
- **Step 0 evidence:** three real Claude Stop payloads in
  `crates/core/tests/fixtures/stop_hook/`, with session ids, paths, prompt
  id, last message and artifact URL replaced, and otherwise faithful:
  - `stop_subagent_running.json`: `{"type":"subagent","status":"running",
    "description":<name>,"agent_type":…}`;
  - `stop_subagent_finished.json`: `background_tasks: []`. Completion is
    the task's disappearance, never `status: "completed"`;
  - `stop_monitor_task_running.json`: a live `type: "monitor"` task from
    another session. This is an unknown kind that never completes on its
    own, and it must never qualify.
- **Step 1 inventory:** below.

### M1: the core model, inert

**Why it is its own boundary:** every later behaviour is decided by this
function, so its safety model gets reviewed alone before any transport
depends on it:
- `qualifies`;
- precedence;
- the original deadline;
- first-use intent resolution;
- receipt commit points;
- restart revalidation.

A wrong transition found after the hook is gutted costs a rewrite of both.

**It contains:**
- `hook_io::BackgroundTask` gains `type`, `description` and
  `agent_type`, plus the raw entry;
- the report, the snapshot (`Known` / `Unavailable`), the verdict, the
  delivery result, the attend/clear request, and the per-session state;
- the pure transition, covering the drive gate (config, auto-off, role),
  questions/blocks, attendance, check-in, repeats, presentation/receipts,
  binding change and restart;
- fixture parse tests and transition tests 1–16, with their mutation
  checks.

Nothing calls it yet. Being inert is the point: the existing behaviour is
untouched, so this commit is safe on its own.

### M2: the monitor decides, the hook carries (everything else, one commit)

All policy moves together. The drive gate, questions, loop policy,
attendance, check-in, repeats, presentation, receipts, recovery and
SessionStart's peek all move in this one commit. That is why auto-off,
question/block precedence, config changes after a hook exits, binding
changes and deadline delivery never have two deciders: there is no
commit in which some of them are hook-owned. M2 contains:
- the protocol bump;
- the monitor's state, persistence, restart revalidation, the check-in on
  the nudge route and the receipt commit on `Delivered`;
- recovery and the status/TUI projection read state;
- the hook reduced to the messenger, with inventory groups 2–5 and 7–12
  deleted from it;
- the hook-file reads deleted from monitor, status and the TUI;
- the Alone-mode contract (rule 6);
- `clank attend <selector>`, renamed with a hidden `attending` alias, its
  resolution diagnostics, the nudge/repeat/check-in recipes, the skills
  line, and the whole-repo sweep of the old word (keeping only the alias
  and its parse test). The behaviour and the words that teach it land
  together;
- the full source gate.

#### M2 transport, built from existing frames (no new sockets)

Today the protocol is one-way after the hello: the client sends `Hello`,
the monitor pushes `Note`s, and `Who::Control` is the only request. M2
turns it into a request/verdict exchange on the same connection:

1. **Report:** `Who::Agent` in `Hello` gains `turn_end:
   Option<TurnEndReport>`. The hook's spawned `clank wait` child carries
   the report in the hello it already sends. `None` keeps today's plain
   waits, such as `clank wait` run by hand.
2. **Verdict, with its payload:** a new `Note::Verdict { verdict,
   rendered }`. It replaces `Note::Items` on connections that carry a
   report.
   - For `Deliver`, `rendered` is the continuation TEXT the monitor
     rendered from its selected items, using today's presentation code
     (`render_wait_items*`, `duty_line`, `discharge_*`, the recipe
     paragraph). That code moves to the monitor side.
   - The monitor keeps the selected `WaitItem`s and their receipt
     preparation keyed by the presentation id, so the hook never refetches
     work or reads its own files to render.
   - The hook's only mapping is per-tool wire framing: exit code,
     stdout/stderr, JSON shape.
   - `Park` is not terminal. The connection stays open and the terminal
     verdict arrives later as another frame.
3. **The live-child handshake and the connection-scoped result.**
   `OwnedWait::run_until` (which calls `wait_with_output`) cannot be
   reused: a child that must stay alive to forward the result would
   deadlock waiting to exit. The hook instead:
   1. spawns the child with piped stdin and stdout, keeping stdin as the
      ownership sentinel throughout;
   2. reads verdict frames line by line from the child's stdout while the
      child and its monitor connection stay alive, until one is terminal;
   3. emits a `Deliver` to the harness;
   4. writes exactly one result line on the child's stdin: `Delivered(id)`
      or `EmitFailed(id)`;
   5. closes stdin and collects the child.

   The child forwards the result on the SAME socket that carried the
   presentation. It is the first frame the monitor reads after a hello,
   so a result cannot be credited across connections. A stdin EOF before
   a result line is `EmitFailed`, and if the hook is killed at any point,
   EOF cancels the wait and the child exits.
4. **Attend / Clear:** two new variants on the existing one-shot
   `Who::Control`, alongside `Control::Restart`.
5. **Check-in, on the agent's established resume route.** A
   `RecoveryHelper` is not guaranteed to exist: claude setup installs no
   persistent helper, and `act_on_recovery` reports Unavailable without
   a connected `Served::Helper`. So the check-in uses what recovery
   already resolves for the agent, `recovery::transport_for(tool, …)`.
   For Claude that is `Resume::TypedSubmit` → `Transport::Typed` (the
   route that types "Clank sees you stopped…" today), under the same
   `resume::resume_for` version grant and dispatcher authorization.
   - **Silent needs a route.** Whether the resume route is available is a
     monitor-collected fact, `check_in_route: Available |
     Unavailable(why)`, added to `qualifies`'s inputs. A terminal Silent
     is only allowed while it is `Available`. Without a route, the
     attendance is recorded but does not qualify, and the turn-end Parks
     (noise, never an unwakeable silence).
   - **The route is lost after Silent.** At the deadline the check-in
     attempt fails. It is recorded as `CheckInUndeliverable(why)`, never
     as delivered, and status/TUI show it. The attendance no longer
     qualifies (its deadline has passed), so recovery's own idle path
     takes over with its existing escalation, and the harness's
     completion wake still arrives when the task ends.
   - **No permanent helper, no polling.** The attempt is one dispatcher
     submit at the nearest deadline.
6. **Protocol 16:** a mismatch already declines into `Attached::Alone`,
   and Alone mode answers with `turn_policy::alone_verdict` (rule 6).

M2 stays one commit, as approved. A split into transport, then
repeats/questions/receipts, then recovery and the surface was considered
and rejected. Its middle commits would leave repeats, questions and
receipts hook-owned while the monitor decides the verdict, which is the
two-owner state this plan forbids.

### Inventory (step 1)

The decisions in `stop_hook.rs`, each assigned to a milestone:

1. **Routing (stays, as transport):**
   - GROK_HOOK_EVENT suppression and grok dispatch;
   - `resolve_hook_repo`;
   - `resolve_identity_for_hook_in`;
   - `binding_matches`, checked at entry, after the park, and in
     `emit_delivery`.
2. **`drive_gate`** (M2, done): an unreadable config gives Failed, auto
   Off gives NotOurs(AutoOff), and an unresolvable role gives Failed. That
   order is the precedence the monitor keeps, with attendance after it.
   Now `turn_policy::drive_gate`, shared by TurnEnd, Parked, SessionStart
   and `alone_verdict`.
3. **Attendance** (M2, done): deleted from the hook. The marker, its
   proof and the `attended` projection are the monitor's
   `SessionState::attendance` (`turn_custody.rs`), persisted in the
   agent's `turn.json` and published in `state.json`'s `attending`.
4. **Check-in timer** (M2, done): deleted. The check-in is the monitor's
   `CheckInDue` at `since + expect` on its own nearest deadline, typed
   through the resume route (`deliver_check_in`).
5. **Loop policy** (M2, done): deleted from the hook. Completion wakes
   are `turn_policy::completion_wake`; the deadline verdict is
   `turn_policy::deadline_verdict`, framed by the monitor at a parked
   report's deadline (`Turns::deadline_frame`), by the child alone, and
   by the hook only when no verdict at all came by its own deadline.
6. **Park** (stays, as transport): `wait_command`, `OwnedWait`,
   `await_verdict_under_deadline` and the stdin sentinel. `OwnedWait`
   now reads verdict frames from a live child and writes one result back.
7. **Presentation** (M2, done): moved to `wake_render.rs`, rendered by the
   monitor (`Turns::answer`) from the items it presents.
8. **Repeats** (M2, done): `native_boundary` stays as evidence and rides
   the report's `TurnPlace`. The count is `turn_policy::count_delivery` in
   the monitor's state; `repeat.json` / `repeat.lock` and their code are
   deleted. `wake_repeat.rs` keeps the escalation text and the record
   boundaries.
9. **Questions** (M2, done): `question_evidence` stays as evidence and
   rides the report; the monitor applies it (`Turns::observe_questions`).
10. **Receipts** (M2, done): the monitor prepares a presentation's
    receipts when it makes it and commits them on its `Delivered`
    (`Commit::run`: `presentation::record` + `wake_receipt::dispatch`).
11. **Recovery modes** (M2, done): moved out of the hook module to
    `recovery_hook.rs`, and off files onto the connection. An observed
    turn is `Control::Observe`; the monitor records it (it is now the
    only writer of `turns.jsonl`) and answers how a retry reaches the
    harness (`recovery::retry_announcement`), which the hook prints. A
    helper's answer is `Control::RecoveryReply`, applied by
    `Driver::answered`; `recovery-reply.json` and its reader are deleted.
12. **SessionStart** (M2, done): the bind stays (routing). The peek is
    deleted: a session start is a `Report::SessionStart` the monitor
    answers once (`Event::SessionStart`), presented uncounted, with
    receipts only for Claude, as before.

The monitor and status reads of hook-written files:
- **`monitor.rs`:**
  - 3424 `question_state::observe`;
  - 3434 `wake_repeat::load`;
  - 3456 `question_state::asking`;
  - 3506 `wake_repeat::forget_if_stale`;
  - 3557–3560 `recovery_gate_holds` (`question_state::asking`,
    `stop_hook::attendance_is_live`: the bug).
- **`recovery.rs`:** fold facts `attending` / `asking` (5649–5870).
- **`status.rs`:** 313 `question_state::asking`, 319 `wake_repeat::load`,
  and 337 / 1389 `stop_hook::read_attended`.
- **`status_tui/mod.rs`:** 247 `stop_hook::read_attended_now`.

All deleted in M2, with one read kept on purpose:
- the monitor still reads `question.json` for `asking`. The stop hook no
  longer writes it (the monitor applies the Stop's evidence); its other
  writers are the question hooks and the monitor's own record reader.
  Status shows `asking`, like `attending` and `repeated`, from what the
  monitor publishes, while it describes the current binding;
- `recovery.rs`'s fold facts are computed from the model: `attending` is
  `SessionState::recovery_quiet`, `asking` the projection.

**Grok** gets the messenger shape too, keeping its native handoff: its
Stop reports `Report::Handoff { session, wait_running, continued }`, the
evidence of a running `clank wait`. The monitor answers it with
`turn_policy::handoff_verdict` (the drive gate, then `Notice::ArmWait`
unless a wait already carries the wake, `SilentWhy::WaitArmed`), and the
adapter frames that as Grok's block-with-reason. The auto-mode, role and
arm decisions are deleted from `grok_hook.rs`. Its auto mode is read
under the agent's launch HOME (`grok_setup::launch_home`), as the
monitor's automatic-wait admission already did. A handoff's child holds
no session, so it can never supersede, or be refused as a duplicate of,
the wait it reports. With no monitor, the child answers it by the same
rule. Once the answer is in and the child collected, the adapter checks
the binding again before any native output (`still_bound`), as
`emit_turn` does for Claude and Codex: an answer authorized for a binding
replaced in transit reaches the old session as nothing.

### M1 model notes (`clank_core::turn_policy`)

The places where the model reads today's meaning, or deliberately
differs from it:

- **TurnEnd precedence** is today's `decide`:
  1. stale binding;
  2. unreadable config;
  3. auto-off;
  4. unresolvable role;
  5. attendance (`qualifies`);
  6. owed items (Deliver) or Park.

  A pending question does not change a turn-end today, and it still does
  not. Blocks still act through the owed-work fold, which withholds what
  they pause. "A question or block precedes attendance" is realised where
  today's code has it, in recovery: `recovery_quiet` returns Blocked,
  then Asking, then Attending (`recovery_gate_holds`' order). Test 8
  covers both places.
- **Proof differs from today on purpose:**
  - a subagent qualifies by its fresh listing with no pid;
  - a shell needs both the listing and its pid + token;
  - an unknown kind never qualifies.

  Today any listed task id plus a live pid suppressed.
- **Shell kind:** no captured payload carried a shell's `type`. So
  `type: "shell"`, or a task with a command line and no `type`, is a
  shell (today's records), and anything else unlisted is Unknown.
- **Check-in at the deadline** for a shell whose process has gone ends
  the attendance with no check-in. That is today's `Disarmed::WorkEnded`:
  the completion wake is the wake.
- **A check-in is owed only by an attendance that quieted a stop.** It
  is `Unarmed` until a turn-end is silenced on it. A never-qualifying
  attendance (unknown kind, no completion wake, no process proof) never
  arms one.
- **At the deadline the check-in passes `wake_gate`, the same gate as
  every clank wake:**
  - auto-off, a pending block or a pending question make it `Withheld`:
    spent, never rescheduled. What lifts those gates (auto turning on, the
    person's answer) is itself a wake, and past the deadline the
    attendance no longer quiets recovery;
  - an unreadable config or role cannot establish auto-off, so it fails
    toward noise and the check-in fires.
- **A resolution notice** (no match, ambiguous, shell needs `clank run`)
  rides on the Deliver when work is owed. Otherwise it is a Notify, and
  the next turn-end Parks.
- **Repeats:** `count_delivery` is today's `wake_repeat::decide`.
  - **Invalidation:** every step judges the record by the owed work in
    its own facts, before and after the event. Any observed change,
    including nothing owed, forgets it, exactly as today's monitor calls
    `forget_if_stale`.
  - **Late results:** facts are read when a step is evaluated, so a late
    result is judged by the work owed when it arrives. A delivery whose
    work is no longer owed commits its receipts and leaves no count.
- **Presentation custody:**
  - **One at a time.** A session has at most one presentation. While it
    is `InFlight`, a turn-end with owed work Parks, and the monitor steps
    that parked report again once the result arrives. That is today's
    serialised `send_counted`: each delivery counts against the record
    the previous one committed.
  - **Immutable.** An id names one payload, receipts and repeat decision
    fixed when it is made.
  - **Connection-scoped results.** `Delivered` / `EmitFailed` answer only
    the in-flight presentation, from the connection that carried it.
    - **`EmitFailed` is terminal.** The monitor also raises it when a
      hook's connection closes without a result. The record is discarded,
      and no late success can exist for it.
    - **A stray result**, for anything not in flight, is
      `ResultOutcome::ProtocolViolation` and commits nothing. A repeat of
      the result that last settled a presentation is
      `ResultOutcome::Duplicate`.
  - **No reuse.** After a failure, the next TurnEnd mints a fresh
    presentation from the current owed work. Its repeat decision comes
    from the last COMMITTED record, as invalidated by observed changes,
    so a failed count can never revive an episode that was seen to end.
  - **Restart** settles an in-flight presentation as terminally failed,
    because its connection died with the old monitor. Its work is
    presented fresh at the next stop.
- **Grok** turn-ends are Failed, as today's `TurnRoute::Undrivable`.
  `deadline_verdict` is today's `deadline_action` (ReArm for
  claude/codex, RenewWait for opencode, Failed for grok).
- **Restart:** `Persisted` omits the last listing, so a restored
  attendance cannot quiet recovery until a turn-end reports one.

### M2 implementation notes

**Where it lives.**
- `turn_custody.rs`: the monitor's `Turns` — one `SessionState` per
  agent, keyed by its binding, persisted in `.clank/agents/<label>/turn.json`
  and restored on first touch (`SessionState::restored`); fact gathering
  (`Gathered`, read afresh for every step); verdict rendering; receipt
  commits (`Commit`); the check-in text; the projection.
- `turn_wire.rs`: `Report` (`TurnEnd { stop, deadline, questions }` /
  `SessionStart` / `Handoff`), `WireVerdict`, `Frame`, `TurnResult`.
- `turn_child.rs`: `clank wait --turn-end`, the child: report on its first
  stdin line, verdict frames on stdout, one result forwarded on the
  connection that carried the presentation, EOF as owner death. Two
  attaches through the ordinary attach-or-spawn, then the Alone fold.
- `stop_hook.rs`: routing, evidence, the report, `OwnedWait` reading frames
  from the live child, per-tool framing, the one result, the sentinel.
- `wake_render.rs`: every wake's wording, moved from the hook.
- `recovery_hook.rs`: the recovery modes, moved out of the hook module
  and carried on the connection.
- `grok_hook.rs`: Grok's native routing and evidence, and its wire.
- `attend.rs` (was `attending.rs`): `clank attend`, hidden alias
  `attending`, submitting `Control::Attend` / `Control::Clear`.

**Protocol 16.** `Who::Agent { turn_end }`, `Note::Verdict(Frame)`, and
`Control::{Attend, Clear, Observe, RecoveryReply}`, and
`Report::Handoff`. A reporting connection is split at admission:
its writer writes, a reader (`hear`) forwards the lines after the hello
to the loop as `Heard::Line` / `Heard::Closed`, and each connection has an
id (`Client::conn`). A result is credited only by `Turns::result` when its
connection is the one recorded as carrying the presentation.

**Model extensions** (core, each with a test):
- `Facts::check_in_route` and `Unqualified::NoCheckInRoute`;
- `CheckIn::Undeliverable` and `Event::CheckInUndeliverable`;
- `Event::Parked`: a parked report stepped again resolves no intent and
  proves no attendance, since its listing is as old as the park. Without
  it, an intent made in a new turn while an older hook was still parked
  (asyncRewake) would be resolved, and dropped, against the stale listing;
- `Event::SessionStart`, presented with `RepeatDecision::Uncounted`;
- `Event::Observed`, so a fold forgets a stale repeat as
  `forget_if_stale` did;
- a Standing attendance takes its listing's description when the request
  named none;
- `CheckInRef` on `Effect::CheckIn` and `Event::CheckInUndeliverable`,
  and `SessionState::may_type_check_in`;
- `Handoff`, `handoff_verdict`, `Notice::ArmWait`, `SilentWhy::WaitArmed`
  (`SilentReason::WaitArmed` on the wire).

**The check-in route** is `recovery::Driver::check_in_routes`:
`transport_for(tool, Nudge, version)` must give `Typed`, from the harness
identity recovery already establishes once per binding. An agent with an
attendance is identified as an obligated one is (`Facts::identify`,
`Driver::wants_identity`), with no extra scans. At the deadline the
check-in is located, re-authorized on the loop against the very process
identified (`may_check_in_now`, `Driver::may_type_check_in`) and typed
once; any failure is `CheckInUndeliverable`.

**Custody ends with its binding.** Binding removal is a retirement
event: an agent the facts show unbound (at any step, or at a fold, via
`Turns::retire_unbound`) loses its attendance, check-in, repeat count and
any presentation in flight, in memory and on disk. Before this, an
unbound agent's armed deadline stayed in `next_deadline` while
`check_in` declined to evaluate it, so the loop woke on a passed
deadline again and again.

**A check-in names its attendance.** `Effect::CheckIn` carries a
`CheckInRef { task_id, since }`, and so do the request typed off the
loop and its outcome. The last look before typing
(`SessionState::may_type_check_in`, from `may_check_in_now`) refuses
unless the attendance is still that one with its check-in in flight, and
every wake gate is open by facts gathered then: auto off, a question or a
block opened, a clear or a replacement, each refuses. A lost check-in is
recorded only against the attendance it was fired for, so an overtaken
attempt cannot overwrite its replacement's outcome.

**Transient work stays held until it is delivered.** Selecting a Deliver
no longer removes the author's held items. A presentation records the
identities it carries, and only its `Delivered` discharges them
(`settle_result`); an emit failure, a closed connection, or a verdict
that could not even be queued (failed at once by `serve_reports`) leaves
them held, owed to the next presentation. `serve_reports` takes the held
items read-only, so it cannot drop them.

**Receipts commit after the hook exits.** The hook reports `Delivered`
and closes; the monitor then records the presentation and runs the
receipt callbacks, off its loop. Delivery evidence and receipts follow the
emit, as before, but are no longer done by the time the hook exits.

### M2 changed assertions

Tests of the deleted mechanisms were ported to the owner of the decision;
these are the ones whose meaning had to change, and why:
- **One quiet turn-end per marker** (`any_other_outcome_clears_the_decision`,
  `empty_wait_failure_still_consumes_the_marker`,
  `markers_of_any_older_shape_are_consumed_but_never_suppress`,
  `auto_off_still_consumes_the_marker`,
  `an_unreadable_marker_fails_toward_noise_and_is_taken`): there is no
  marker. An attendance stands until a listing ends it or its deadline
  passes (the model), so "consumed" has no meaning.
- **A live pid suppresses** (`only_a_provably_live_attendance_may_suppress`,
  `the_gate_reads_the_agent_as_it_is_now`,
  `facts_name_who_owes_work_and_who_is_waiting_for_it`): a subagent
  needs no pid, a shell needs its listing AND its pid, and a codex
  attendance never quiets anything (rule 1). The two monitor tests now
  prove attendance with a Claude listing.
- **The check-in timer** (`the_work_ending_before_the_bound_yields_without_a_check_in`,
  `a_re_attend_supersedes_the_armed_check_in`,
  `clear_cancels_the_armed_check_in`, `the_ceiling_asks_what_it_hit…`,
  `outside_the_async_loop_attendance_silences_at_once`, and the rest of
  that group): no hook holds a timer. Work ending is learned only from the
  next Stop's listing (t03, t15); the check-in is the monitor's, typed
  into the pane, so `--loop asyncrewake` no longer changes anything.
- **Serialised sends** (`continuations_sent_out_of_order_are_counted_conservatively`,
  `overlapping_sends_each_state_and_store_their_own_count`,
  `a_rebind_while_the_sender_waits_sends_nothing`): one presentation is
  in flight per session, so out-of-order or overlapping sends of one
  session cannot happen; the lock they exercised is gone.
- **Evidence newer than the count** (`a_count_is_forgotten_only_on_current_evidence`):
  facts are read when a step is evaluated, so there is no timestamp to
  compare.
- **Admission** (`a_description_is_required_unless_you_are_clearing`,
  `a_wait_clank_cannot_verify_is_refused_not_recorded`): `--desc` is
  required only with `--pid`; without one the name is a subagent's, which
  needs no pid. An unidentifiable pid is still refused.
- **The status repeat threshold** (`a_repeating_agent_is_reported_by_its_count`):
  the threshold is applied where the projection is made
  (`a_count_below_the_threshold_is_not_shown`); status shows what is
  published while it is current.
- **The skill line**: teaches `clank attend "<name>"` for subagents and
  `clank run` for shell work; the `--pid` form is in the help and README.
- **`wake_receipt_acceptance`**: the hook alone (lease held) now wakes
  with the monitor-unavailable notice and earns no receipt or delivery
  evidence (`wide_icon_hook_alone_wakes_without_zellij_presents_nothing_and_keeps_reply_owed`);
  `session_start_notifies_…` and `receipt_does_not_retire_work_…` run a
  monitor, since receipts are now earned only through one;
  `shared_monitor_delivery_…_commits_its_receipt_on_delivered` was
  `…_does_not_own_receipts`; and every receipt is waited for, since the
  monitor commits it after the hook exits.
- **Grok's native hook** (`grok_hook_tests`): each Stop is now decided by
  the monitor's rule on the evidence the hook routes (`route`, then
  `handoff_verdict`, framed by `wire`), with the same expected answers.
  The session-start roster check is `resolve_tool`, a roster lookup, not
  a role decision.
- **Recovery replies** (`a_helpers_reply_is_applied_to_its_own_attempt_only`,
  `a_reply_for_an_agent_off_the_roster_is_its_attempts_report`): driven by
  `Driver::answered`, the reply's arrival on the connection, not a file.
  `a_restart_keeps_the_attempt_for_a_reply_that_survived_it` is
  `…_that_comes_after_it`. `an_overdue_restart_reads_the_reply_before_deciding_none_came`
  is `an_overdue_restart_settles_the_attempt_once`: a reply no longer
  waits on disk while no monitor runs, so one that arrives only after the
  restarted monitor's timer settled the attempt is late and adds nothing,
  as any late answer does. `an_episode_survives_the_disk_and_a_reply_is_taken_once`
  keeps only its episode half; the reply's wire form is covered by
  `recovery_requests_are_carried_out_for_a_bound_session_only`.
- **Status asking** (`the_status_frame_says_a_recorded_question_is_waiting`,
  `a_repeating_agent_is_reported_by_its_count`): the recorded question
  reaches the status frame through a monitor's fold, as the projection
  it publishes, and only for the current binding.
- **The last look** (`a_check_in_its_last_look_refuses_is_not_typed`,
  `the_last_look_needs_an_identified_harness`): it answers with the reason
  it refused. Codex's `reviewer_last_look_must_recheck_auto_off_before_typing`
  is `the_last_look_rechecks_the_attendance_and_every_gate_before_typing`,
  which needs the custody and facts the look now takes; the other two
  probes are kept as `unbinding_an_attended_agent_removes_its_due_timer`
  and `a_failed_emit_keeps_unlogged_work_owed`.

### M2 mutation checks

Nineteen mutants, each caught by its covering suite: the route check
removed from `qualifies`; an undeliverable check-in not recorded; a parked
report stepped as a new Stop, and a first report stepped as a parked one;
an intent resolved against the latest prior report (rule 4); recovery
quiet on any recorded attendance (rule 12); attendance taken at the
turn-end that silences (rule 2); a result credited on any connection; EOF
before a result forwarded as delivered; a carrier closing without a result
settling nothing; Alone mode never waking on owed work; a failed `clank
run` exec clearing nothing; a parked verdict ending the hook; a due
check-in never typed; a failed emit committing receipts; a session start
committing receipts for any tool; status showing a stale projection; the
monitor dropping a Stop's question evidence; the hook reading an agent
directory (the source gate).

The corrections to M2 (codex on fbd5a0c3) add eighteen, each caught:
custody not retired when its binding goes; a fold not retiring unbound
custody; retirement leaving `turn.json` on disk; the last look ignoring
which attendance fired, ignoring the wake gates, and skipping custody and
gates at the monitor; a lost check-in overwriting whatever attendance
stands; a verdict never queued leaving its presentation in flight; a
delivery discharging nothing, and discharging all of its author's held
work; Grok ignoring a running wait; a handoff skipping the drive gate,
holding its session, and waiting for a fold; recovery requests skipping
the binding check; a hook resolving auto mode itself, and writing a
harness record (both the gate); status showing another session's
question. Not mutation-checked, being the owner loop's own wiring: the
loop passing fresh facts to the last look, and `Control::RecoveryReply`
reaching `Driver::answered` (its refusal and acceptance are tested).

One more for codex on 467e95df, caught: Grok's adapter framing an answer
without checking the binding again once it is in
(`an_answer_overtaken_by_a_rebind_reaches_the_old_session_as_nothing`).

## Acceptance

M1 carries transitions 1–16 and fixture parsing. M2 carries everything
else below, including:
- **Check-in end to end:** a qualifying Claude attendance goes through
  a terminal Silent to an actual observable typed check-in on a test
  dispatcher, not just `Effect::CheckIn`. With the route missing at
  turn-end, the turn-end Parks. With the route lost before the deadline,
  `CheckInUndeliverable` is recorded and recovery takes over.
- **The pipe/socket handshake:** a verdict frame is read before the
  child exits; the result is forwarded on the same owned connection;
  a result written before EOF is attributed, and EOF without one is
  EmitFailed; a killed hook's child exits and nothing is committed.

and the Alone-mode sequence:
1. owed work with the monitor unavailable gives a Diagnostic and no
   receipt, both when the emit succeeds and when it fails;
2. the monitor becomes available;
3. the next TurnEnd delivers the same items once and commits one receipt;
4. nothing is lost and nothing is duplicated.

The monitor reads a presentation's result only from the connection that
carried it. A transport test shows that a result arriving on another
connection (an older hook's, or a client's) cannot be attributed to the
in-flight presentation. That is the transport guarantee the model's
terminal-failure contract rests on.

Alone mode is also never Silent on attendance, and still honours stale
binding, auto-off and a pending question.


- **Source gate:** the hook module's non-test code is routing,
  evidence, one report, verdict mapping, the delivery result and the
  ownership sentinel. A gate in the `it` harness fails if the hook reads
  or writes an agent's `attending`, `attended`, `attending.holder`,
  `repeat*`, `recovery*` or question-state files.
- **Transition tests** (pure, in core), one per event above:
  1. Attend a subagent, then a TurnEnd snapshot containing it: Standing,
     silenced.
  2. An unrelated wake, then the next TurnEnd: still silenced, with no
     re-attend, and `since` unchanged.
  3. A TurnEnd without the task: attendance ends, normal park.
  4. Intent before the task exists, then a TurnEnd snapshot that has it:
     resolves. The first-use ordering is covered.
  5. Zero or ambiguous matches: a Deliver diagnostic listing the live
     tasks, and the intent dropped.
  6. An `Unavailable` snapshot never proves a subagent and never ends a
     shell-proven attendance.
  7. A stale-binding report or attend request mutates nothing. A new
     binding drops the old attendance.
  8. A question, block or auto-off still precedes attendance.
  9. CheckInDue fires once. Repeated reports don't re-arm it or move the
     deadline.
  10. Restart: the original deadline is kept, and the attendance is
      unproven until the next TurnEnd.
  11. Deliver, then no result (the hook died): the failure is terminal,
      the items are still owed and are presented fresh, and a late result
      for the failed presentation is a protocol violation that commits
      nothing. Deliver, then Delivered: receipt committed once, and a
      repeat of it is a no-op.
  12. Recovery is quiet for a qualifying attendance, and resumes after
      it ends or passes its deadline.
  13. Codex (`Unavailable`, `InHookWait`) with a live `clank run` pid:
      the attendance is recorded, the turn-end Parks on the hook's wait,
      and it is never Silent.
  14. A Known snapshot listing a shell task with no recorded process
      proof: the attendance does not qualify, the diagnostic names `clank
      run`, and the turn-end Parks.
  15. A Known snapshot missing the shell task (or marking it completed)
      while its pid is still alive: the attendance ends.
  16. One attendance, same original deadline: TurnEnd and Recovery both
      quiet before it, and both normal after it. CheckInDue fires
      exactly once at it. A TurnEnd after the deadline neither resets it
      nor re-arms the check-in.
- **Mutation checks:**
  - 4 with resolution against the latest prior report instead of the
    next TurnEnd;
  - 12 with the old `attendance_is_live` body;
  - 2 with the marker taken at turn-end.
- **Ported tests:** today's attendance, check-in, repeat, question,
  receipt and recovery tests drive the transition. Their meaning is
  unchanged, and the plan lists any assertion that had to change, with
  the reason.
- **Fixtures:** tests from the step-0 captured payloads parse them, and
  resolve a subagent by name with no pid.
- **`clank run`:** a failed exec leaves no intent.
- **Suites, with the session env cleared:**
  - `cargo test -p clank --lib`;
  - `cargo test -p clank-core`;
  - the `it` modules for wait, standby wakes, wake receipts and the new
    hook gate.
