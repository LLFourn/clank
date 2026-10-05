# a-failed-turn-is-reported-and-retried
# A failed turn is reported and retried

## Why

An agent whose turn ends on a provider error is left with pending work and no
waker. Claude, Codex and Grok skip their Stop hook on an API-error turn, and Stop
is what re-arms clank's waker. The clankstack reviewer sat idle for 2 h 14 min
after a transient `server_overloaded`. Clank's OpenCode plugin has the opposite
defect: it re-injects immediately after every failure, with no backoff.

The finished research plan `research-agent-stalls-and-provider-capacity-recovery`
is this plan's specification, especially its "contract chain" §1–§5, "State
machine traces", "Change map" and "Fixtures". This plan restates only the
contract it must keep.

## The contract

- **State comes from each harness's ordered record, never hook timing (§1).**
  - Sources: the Codex rollout, the Claude transcript, Grok's `events.jsonl`,
    and the OpenCode plugin's event stream.
  - The monitor watches the record only while the agent is obligated, reading
    appended bytes with `transcript::Tail` after a tail read at arm time that
    widens until the latest turn's start is in view.
  - A Grok `StopFailure` hook supplies the error class only when unambiguous;
    otherwise the class is `unknown`.
- **Current means the latest turn in that record ended in error, with nothing
  started since (§2).** Re-read at every decision, so a delayed report or a
  missed change event cannot act on a newer turn.
- **Eligibility (§3).** Obligated, bound, `drive_gate` auto, no pending clank
  question or block, no *deliverer* registered, and no `prove_attendance`
  evidence.
- **One durable episode per (session, obligation) (§4).**
  - Stored in `.clank/agents/<label>/recovery.json`.
  - Budget: retryable classes 1, 5 and 15 min; max_output_tokens and unknown, one
    retry; everything else, escalate at once.
  - Claim before any side effect.
  - Attribute a start to an attempt only by its identity: Codex's returned turn
    id, OpenCode's supplied `messageID`, or Claude's wake text. An unattributed start is a person taking over and closes
    the episode, except after an uncertain attempt, whose turn may be it:
    then only a finished turn closes it.
  - An attributed turn that fails spends budget in the same episode.
  - A native retry holds the episode without spending budget.
  - `Escalated` is latched until a reset: a person's turn, a successful
    completion, the obligation clearing or the session changing. Duplicate
    reports, helper reconnects and restarts are not resets.
  - On restart: a `Claimed` attempt becomes `Indeterminate`, then `Escalated`. A
    `Submitted` attempt is confirmed within 2 min or escalates. A past-due
    `next_at` fires once.
- **Never interrupt; start conditionally or post a notice (§5).**
  - No `pane_input`, `nudge` or `try_wake`.
  - OpenCode: a recovery helper that supplies a native `messageID` in the
    `promptAsync` body and persists it before the request. A 204 means
    `Submitted`; `message.updated` with that id means `Started`; an error after
    send means `Indeterminate`, then `Escalated`. Its local activity check only
    filters stale continuations. Enabled only once `prompt_async` on a busy
    session is shown to be rejected or queued without cancelling. Until then,
    notice-only.
  - Codex: `codex_interrupt::start_if_failed`, enabled only once `turn/start` on
    an active thread is shown to be rejected or queued without cancelling, and
    D1 is fixed. Until then, notice-only.
  - Claude: an `asyncRewake` `StopFailure` recovery helper, enabled only once its
    wake, and its behaviour during a running turn, are shown. Until then,
    notice-only.
  - Grok: notice-only, because typed input cancels a running turn.
- **Two client kinds.**
  - A *deliverer* is today's stop-hook wait. It ends eligibility and closes an
    episode as newer progress.
  - A *recovery helper* never receives inbox delivery. It receives exactly one
    dispatch per authorized attempt and answers `Submitted` or `Refused`. Its
    registration neither disqualifies nor closes its episode.
  - The OpenCode plugin starts a helper, not a deliverer, after a failed
    execution (fixes D2).
- **Escalation and no-transport cases (§6)** post one timeline row and status
  state, then nothing until a reset.

## First, record capability checks

Each one gates its harness's action, which stays notice-only until the check
passes:

- Codex: `turn/start` on an active thread.
- Claude: `asyncRewake` on `StopFailure`, and its effect during a running turn.
- Grok: a real `StopFailure` payload (class only).
- OpenCode: `prompt_async` on a busy session, and supplied-`messageID`
  attribution.

## Acceptance

The research plan's full fixture list, including:
- its four state-machine traces;
- a delayed report with a missed start;
- a helper held across a backoff, receiving exactly one dispatch;
- a deliverer registering mid-episode;
- OpenCode server activity beginning before the plugin hears of it;
- a `prompt_async` 204 with no body, which is never a started or reset signal;
- an action racing a new turn (`Refused`, no interrupt of any kind);
- a restart with `Claimed` and with `Submitted` attempts;
- latched escalation surviving duplicate reports and restart;
- offline catch-up with no burst;
- the OpenCode hot loop replaced by backoff;
- manual mode, questions and interruptions;
- missing prerequisites giving notice-only.

## Depends on

`codex-control-socket-may-be-a-link` (D1), for the Codex action.

## As built

- **The capability gate is closed for every harness.**
  `recovery::transport_for` returns `None` until each live check below
  passes. So the shipped behaviour is that a classified failure in an
  obligated agent with no deliverer becomes one latched notice: a handled
  timeline row plus `waiting for you: <agent> → …` in `clank status`. It
  is never re-injected. All transports are implemented and exercised
  against fakes.
- **Decisions are made from a snapshot of the session's recent turns,
  never from a single event.** `recovery::Snapshot` holds each turn's
  harness id and outcome, and `recovery::decide` takes the persisted
  episode plus that snapshot. As a result:
  - a batch holding a failure and a person's next turn decides once, on
    the person's turn;
  - a retry targets the turn that failed last, under that turn's class;
  - a restarted monitor follows whatever the record gained while it was
    down.

  A deadline re-reads the record, and asks the monitor about the agent as
  it is then (binding, deliverer, auto mode, block, attendance), before
  anything is dispatched.
- **Records are watched off the monitor's loop.** `recovery::RecordWatch`
  owns one OS watcher on a thread of its own. On macOS, FSEvents costs
  about 5 s to set up a watch, then about 12 ms per change.
  - It watches the directory a record lives in: the nearest one that
    exists, non-recursively. Its creation, its replacement and its
    deletion are therefore all seen, as well as appends.
  - A Codex rollout that has not been written yet is the one exception:
    its sessions root is watched recursively, and the rollout is found by
    its session id in the changed path's name. The only lookup is a
    single one when that watch comes up, which finds a rollout written
    during setup that has been quiet since.
  - Once a watch is in place it reports itself once, so changes made
    during setup are read.
  - The driver alone owns the watches: one per directory, however many
    agents share it. A watch the backend loses is let go on both sides,
    and the next fold installs it again once (never sooner), with the
    same setup report.
  - A fold looks at a record again only when its source changed (a new
    binding, or a new recorded path for the same session) or its watch
    must be registered again. Otherwise only watch events and deadlines
    read it.
- **A read is required before a deadline acts.** A record that cannot be
  read just then dispatches nothing from what was read before. Three such
  deadlines escalate.

  At arm time the record is read back over windows that widen (256 KiB
  up to the whole file) until the latest turn's start is in view; a
  replaced or truncated record is read back the same way.
- **An uncertain attempt is still the attempt.** An attempt that may have
  reached the harness keeps its escalation latched, and its budget spent,
  if its turn shows up later and fails. Only that turn finishing closes
  it, whether it is recognised by id or tag, or is an unnamed newer turn.
- **Storage failures hold, not spin.** An episode that cannot be stored
  dispatches nothing, and no path decides again for `RETRY_LOOK`: not a
  deadline, not a record event, not an answer. Answers that arrive during
  the hold are kept. The decision is owed with a deadline of its own, so
  a first notice that could not be stored is still made once storage
  works.
- **Claim before effect, and every claim has a deadline.** A dispatch is
  returned only after its claim is on disk.
  - Its transport has `ANSWER_WITHIN` to answer. An unanswered claim
    becomes `Indeterminate` and escalates; it is never replayed.
  - Codex's transport bounds its own phases inside that deadline. Silence
    before `turn/start` is `Unavailable`; silence after it is
    `Indeterminate`.
  - Answers name their attempt (session, opening turn, n), and an answer
    for any other attempt is dropped.
- **OpenCode reports its turns in order.** The plugin reports each turn's
  start and end with its user message id, chaining each session's reports
  so they arrive in the order seen.
  - An assistant error is attributed to its own `parentID` turn, and only
    the current turn's end moves the session's latches.
  - A `session.error` belongs to the running turn only while that turn
    has not ended, and only when it names its session.
  - An idle reports "completed", which a later stop or failure of the same
    turn supersedes.
  - The reader takes only the bound session's lines, and only ends that
    name their turn.
- **Grok's class is always `unknown`.** Its `events.jsonl` `turn_ended`
  carries none, and Grok is notice-only, so its `StopFailure` hook is not
  installed. A hook report that might belong to another turn is not worth
  borrowing a class from for a notice's wording.
- **Attendance at the monitor** is `stop_hook::attendance_is_live`: the
  marker's recorded process still holds its identity token. That is
  `prove_attendance` minus the hook's own task list, which the monitor
  does not have.

## Capability checks still to record (each enables one transport)

- **Codex** — `turn/start` on an active thread: rejected, queued or
  cancelling? Also `codex-control-socket-may-be-a-link` (D1). Enables
  `Transport::CodexStart`.
- **OpenCode** — `prompt_async` on a busy session; whether a supplied
  `messageID` of clank's tag form is accepted; and whether it is the id
  `message.updated` later reports. Enables `Transport::Helper` for
  OpenCode.
- **Claude** — `asyncRewake` on `StopFailure`, and its effect during a
  running turn. The helper mode is built: for Claude,
  `clank stop-hook --tool claude --recovery-helper` records the attempt as
  submitted, then exits 2 with the wake on stderr. Setup does not install
  it as a `StopFailure` hook until this check passes and enables
  `Transport::Helper` for Claude.
