# research-agent-stalls-and-provider-capacity-recovery
# Research agent stalls and provider capacity recovery

## Incident and research outcome

The Codex reviewer in `/Users/llfourn/src/clankstack` stopped progressing after
`Selected model is at capacity. Please try a different model.` The pending Clank
review remained, and a subsequent continuation was interrupted. This can leave a
team waiting indefinitely for an agent that appears launched but is doing no work.
Research a reliable, inexpensive recovery design and recommend a concrete way
forward for every supported harness: Codex, Claude Code, Grok and OpenCode.

This is a research plan. Produce evidence and an implementation recommendation,
not a partial watchdog implementation. Complete the research in one commit without
internal named milestones, and queue any resulting implementation plan(s).

## Options to investigate

1. A general inactivity watchdog: when an agent has made no meaningful progress
   for a configurable interval, nudge it. Evaluate 40 minutes as the default.
   Define progress and eligibility precisely; an idle agent, native blocking wait,
   pending human answer, intentional interruption, long-running tool, manual mode
   or no current obligation must not automatically be mistaken for a stall.
2. Detect provider failures in the agent timeline, including the reported Codex
   capacity message, and schedule a recovery nudge. Determine whether this error is
   recorded in the transcript, exposed as a structured event, or only visible in
   terminal output. Distinguish an actual harness failure from quoted text in a
   user message, tool output or an old replay. Cover repeated errors and reconnects.
3. Use supported native hooks, lifecycle events, app-server/plugin APIs or other
   fine-grained state notifications to detect and recover stalled/error states.
   Prefer authoritative event-driven signals where available; investigate what
   each current installed harness and its latest supported integration actually
   exposes. Compare these signals with the two fallback options above.

## Research and end-to-end contracts

- Inspect the reported clankstack session and current Clank launch, hook, monitor,
  nudge and interruption paths. Preserve its conversation and pending review work;
  avoid interfering with active sessions merely to collect evidence.
- Consult current primary documentation and source for each harness. Record tested
  versions, exact event/error shapes, available operations and relevant sources.
  Separate observed behavior, documented guarantees and unverified assumptions.
- Establish the states Clank can know: working/progressing, executing a tool,
  awaiting work, awaiting a person, interrupted, provider refusal/capacity error,
  disconnected or unknown. Explain the authority for each state per harness.
  Reuse existing agent/repo/session ownership and monitor contracts rather than
  inventing an additional registration handshake or agent-supplied role.
- Recommend the state transition that schedules recovery, the action to perform,
  and the evidence that cancels or completes it. Reuse provider-aware nudge and
  interruption code; evaluate whether a native resume/retry operation is safer
  and more reliable than terminal key injection. Account for both Zellij and
  deployments without it. State honestly when automatic recovery is unavailable.
- Determine whether a nudge can recover the reported capacity failure at all.
  Distinguish transient retry from a failure that needs a person or a configured
  model fallback. Recommend bounded retries/backoff, deduplication, cancellation
  after fresh progress and clear escalation instead of an endless nudge loop.
  Keep pending events/reviews intact and never manufacture read receipts or acks.
- Keep Clank light: share existing monitor/transcript subscriptions, use one-shot
  deadlines armed only when relevant, and cancel them on state changes. Avoid
  periodic pane dumps, repeated whole-transcript scans, one watchdog per waiter,
  or model turns merely to check liveness. Quantify idle and active resource costs.
- Define restart/offline behavior, stale-session fencing, manual disable/override,
  and the user-visible explanation for any recovery action. An obsolete timer or
  old error replay must not nudge an agent's replacement session.

## Deliverable and verification proposal

Produce a comparison for all four harnesses covering detection signals, confidence,
native recovery capabilities, Zellij/headless behavior, limitations and costs. Give
one coherent recommended design, including how the 40-minute timeout should be
configured, fallback rules, the exact monitor-to-harness contracts and the modules
to change. Explain why alternatives were rejected and how this avoids another
symptom-by-symptom hook patch sequence.

Propose regression fixtures and live acceptance checks for capacity errors,
legitimate parked waits, slow tools, intentional interruption, blocking/background
questions, replay/reconnect, repeated errors, progress just before a deadline,
session replacement and monitor restart. Queue concrete implementation work from
the findings, retaining uncertainty where native capabilities remain unverified.

---

# Findings

Researched 2026-10-05 against the harnesses installed on this machine: Codex CLI
0.160.0, Claude Code 2.1.280, Grok 1.0.46 and OpenCode 1.18.30. Every claim is
marked with its basis:

- **[observed]**: seen in a real record or binary here, reproducible from the
  appendix.
- **[documented]**: the vendor's own reference, linked.
- **[unverified]**: an inference. The live check that settles it is named, and
  the design states its safe outcome until then.

## The incident, reconstructed [observed]

The clankstack reviewer is Codex thread `01a0d79c-16a4-75b3-b2ef-f007aae0a511`
(full path in appendix A1).

| UTC | Record | Meaning |
| --- | --- | --- |
| 10-04 23:06:59 | assistant final answer | an ordinary turn ends |
| 10-04 23:11:05 | `task_complete`, no error | written 4 min later: clank's Stop hook (a parked wait) ran first |
| 10-04 23:11:13 | user `clankstack · codex — Session resumed.` | clank's relaunch prompt opens a turn |
| 10-04 23:11:16 | `task_complete` with `error: {message: "Selected model is at capacity…", codex_error_info: "server_overloaded"}` | the turn fails 3 s after it started |
| (2 h 14 min of nothing) | | |
| 10-05 01:25:39 | user `continue` | a person types; the same model works |

1. **The failure is structured.** It is an `event_msg` of type `task_complete`
   carrying `error.codex_error_info`. Every Codex rollout on this machine was
   checked (A3). The codes seen are `server_overloaded`, `usage_limit_exceeded`,
   `unauthorized`, `cyber_policy`, `misalignment_policy_violation` and `other`.
2. **Codex runs no Stop hook on a failed turn.** A normal turn's `task_complete`
   waits for clank's Stop hook, which explains the 4-minute gap. The failed turn
   completed in 3 s. Codex 0.160's hook events contain no failure event (A4).
3. **The capacity failure was transient.** A plain `continue` on the same model
   succeeded later. A Codex guardian subagent in anxiety-attack hit the same
   error and succeeded on Codex's own retry one second later (A2).
4. **Quoted text is not a failure.** Two rollouts carry the capacity sentence only
   inside a guardian's approval prompt (A2). Matching the `task_complete.error`
   record, never message text, is what tells them apart.

## The root cause: one missing transition

Clank re-arms an agent's waker only when a turn **ends successfully**:

- Claude and Codex: the `Stop` hook.
- Grok: its `Stop` gate, which asks the agent to start a background `clank wait`.
- OpenCode: `session.idle`.

Claude, Codex and Grok skip `Stop` when an API error ends the turn. An errored
turn therefore leaves an agent with pending work and no waker. Nothing delivers
that work again until a person types. The fix belongs at that transition.

## Two existing defects found

- **D1. Clank's native Codex wake is unavailable on this machine [observed].**
  Codex 0.160 publishes `~/.codex/app-server-control/app-server-control.sock`
  as a symlink into `/private/tmp/codex-daemon-501/` (A5).
  `codex_interrupt::target()` checks the path with `symlink_metadata`, which
  sees a link, not a socket, and refuses. Every native Codex wake, including
  today's event interruptions, falls back to the terminal. Queued separately as
  `codex-control-socket-may-be-a-link`.
- **D2. OpenCode can hot-loop on a persistent provider error [observed in
  `~/.config/opencode/plugin/clank.js`].** The plugin suppresses re-arming only
  for `MessageAbortedError`. A turn that fails on an API error goes idle, pending
  work is returned at once, the continuation is injected, the call fails again,
  and so on, with no backoff, for as long as the failure lasts.

## Harness comparison

| | Codex 0.160 | Claude Code 2.1.280 | Grok 1.0.46 | OpenCode 1.18.30 |
| --- | --- | --- | --- | --- |
| **Ordered turn record** (the authority for state) | rollout JSONL: `task_started{turn_id}`, `task_complete{turn_id, error?}`, `turn_aborted{turn_id, reason}` [observed, A1] | transcript JSONL: user records carry `promptId` and `promptSource`; a failed call is an assistant record with `isApiErrorMessage: true`, `error` (class) and `apiErrorStatus` [observed, A8] | `events.jsonl` per session: `turn_started{turn_number}`, `turn_ended{outcome: completed\|error\|cancelled\|interrupted}`, `permission_requested`/`permission_resolved` [observed, A9] | the plugin's in-process event stream: `session.status` busy/idle/retry, `message.updated` (with `MessageAbortedError`), `session.error` [observed, A7 and clank.js] |
| **Error class** | in the record: `codex_error_info` [observed] | in the record: `error` [observed]. Also the `StopFailure` hook: `error`, `error_details` [observed in binary A6; documented D-claude] | not in `turn_ended` [observed]. Only the `StopFailure` hook: `error`, `errorDetails`, `promptId`; capacity classifies as `rate_limit` [documented, D-grok:376] | `session.error` and the retry `message` [observed] |
| **Per-turn id** | `turn_id` [observed] | `promptId` on user records [observed]. `prompt_id` on hooks, v2.1.196+ [documented] | `turn_number` in `events.jsonl` [observed]. `promptId` on hooks [documented]. Mapping between them: unknown | message ids [unverified] |
| **Interrupted on purpose** | `turn_aborted{reason:"interrupted"}` [observed] | no record and no failure hook; `Stop` simply does not fire [documented] | `turn_ended{outcome:"cancelled"\|"interrupted"}` [observed] | `MessageAbortedError` [observed] |
| **Stop runs after a failure?** | no [observed] | no; `StopFailure` instead [documented] | no; `StopFailure` instead [documented, D-grok:374] | `session.idle` fires [observed] |
| **Conditional (idle-only) native start** | `turn/start` exists; no expected-turn guard documented (only `turn/steer` has `expectedTurnId`) [documented, D-codex]. Behaviour on an active thread: **unknown** | none documented | none known | `promptAsync` is an HTTP `POST /session/{id}/prompt_async` with an optional native `messageID` in the body, answering `204 "Prompt accepted"` with no body [observed in the installed SDK 1.18.11, A10]. No expected-state argument exists. Server behaviour on a busy session: **unknown**. The plugin's local activity check is only a stale-continuation filter, because the server accepts later |
| **Submit-only typed input while busy** | not used | Claude Code queues a message typed during a turn: **unverified** | **cancels the running turn** ("cancel-and-send") [documented, D-grok:418] | not used |
| **Without Zellij** | app-server, once D1 is fixed and the capability is verified | a hook helper, only if `asyncRewake` on `StopFailure` is verified | notice only | plugin |

## The contract chain

Each stage has one owner and its own evidence. Nothing downstream infers from
upstream silence.

### 1. Observation: the harness's own ordered record is the authority

Hook timing cannot establish order: Grok documents turn-end reports arriving
after the next turn's start (D-grok:384). So state is read from each harness's
**ordered on-disk record**:

- Codex: the rollout.
- Claude: the transcript.
- Grok: `events.jsonl`.
- OpenCode: the plugin's ordered event stream, already in process.

These records hold every fact recovery needs, in the order it happened: turn
starts (with their native ids), turn ends with outcome and, where present,
class, interruptions on purpose, and permission prompts. A turn awaiting
permission has started and not ended, so it is never "ended with error".

- **The watch (Codex, Claude, Grok).** While an agent is obligated (stage 3), the
  monitor holds one file-system watch on that session's record, through its
  existing `notify` watcher. On each change it reads only the appended bytes,
  using the existing `transcript::Tail` (Codex and Claude adapters exist; Grok's
  `events.jsonl` needs a small line parser). Paths come from
  `transcript::transcript_path` and its derivation rules. Grok's `events.jsonl`
  sits beside the `chat_history.jsonl` that `grok_derived` already locates.
- **Bootstrap and restart.** When the watch is armed — the agent becomes
  obligated, or the monitor starts — the monitor reads a bounded tail window
  (`Tail::open` with its existing window) to recover the latest turn's state
  before trusting any later change.
- **Hooks only add a class.** A Grok `StopFailure` report (`error`, `promptId`)
  is consulted only for the error class, because Grok's `turn_ended` has none.
  It is used only when it is unambiguous: exactly one `turn_ended{error}` exists
  since the last class consumed. Otherwise the class is `unknown`. Claude and
  Codex need no hook, since their records carry the class.
- **OpenCode.** The plugin already sees its session's events in order. It
  reports a failed execution to the monitor through its recovery helper
  (stage 5), together with its activity counter.

No polling and no whole-file rescan happen: `Tail` keeps its offset, and the
watch exists only while the agent is obligated.

If a harness's record cannot be found or parsed, nothing is observed. Stages 2–5
then do nothing, and only the stage-6 notice applies.

### 2. Freshness: current means "the latest turn in the record"

A failure is current only if, at the moment of every decision, the **latest turn
in the ordered record is the failed turn and nothing has started since**:

- Codex: no `task_started` after the failing `task_complete`.
- Claude: no user record after the `isApiErrorMessage` record.
- Grok: no `turn_started` after the `turn_ended{error}`.

Because this compares positions in one ordered file, a delayed report cannot
mislead it. Trace: A fails, B starts, A's report arrives late. The record
already shows B's start after A, so A is not current, whatever the report says.
A missed start cannot hide either: the decision reads the record, not a cache of
past reports.

### 3. Eligibility: the monitor's decision to consider recovery

An agent is eligible while all of the following hold. The monitor re-evaluates
on every fold invalidation and every record change:

- **Obligated:** the fold has pending work for the agent.
- **Bound:** its session binding is current and the record is that session's.
- **Auto mode effective:** the same `drive_gate` result the Stop hook uses. Auto
  off means notice-only.
- **No pending clank question or block** for the agent, background questions
  included.
- **No delivery-capable waiter:** no parked client of the *deliverer* kind
  (stage 5) registered for the agent, and `stop_hook::prove_attendance` yields
  no evidence, called at each decision point. A marker file alone is not
  attendance.
- **The record's latest turn ended in error** (stage 2), or it is the stage-6
  fallback case.

Eligibility permits a decision. It is never itself evidence of failure.

### 4. Decision: one durable episode per (session, obligation)

The monitor alone decides. It persists the episode in
`.clank/agents/<label>/recovery.json` (temp file, then rename):

```text
episode {
  session_id, opened_by: native turn id of the first failed turn,
  class, attempts: [ attempt ], next_at: Option<time>, state
}
attempt {
  n, claimed_at, transport, before: native turn id latest at claim,
  started: Option<native turn id>, disposition
}
state:       Waiting | Dispatched(n) | Running(n) | Escalated | Closed(reason)
disposition: Claimed | Submitted | Started | Refused | Indeterminate
```

- **Budget by class.**

  | Class | Retries |
  | --- | --- |
  | Retryable: overloaded, server_overloaded, server_error, rate_limit | 3, at +1 min, +5 min, +15 min |
  | max_output_tokens | 1 |
  | unknown / other | 1, at +5 min |
  | usage_limit_exceeded, authentication_failed, unauthorized, billing_error, account_on_hold, oauth_org_not_allowed, cloud_credential_error, model_not_found, invalid_request, cyber_policy, misalignment_policy_violation | 0: escalate at once |

  A model switch happens only if `recovery.fallback_model` is configured for the
  agent, and it spends one attempt of the same budget.
- **Claim before any side effect.** An attempt is written as `Claimed`, recording
  `before` (the record's latest turn id), before a transport runs.
- **Attribution.** The first turn start in the record after `before` belongs to
  the attempt only if it carries the attempt's identity:
  - Codex: the turn id `turn/start` returns.
  - OpenCode: the native `messageID` the helper **supplies** in the
    `promptAsync` body, persisted on the attempt before the request, and
    confirmed when that id appears in the plugin's ordered `message.updated`
    events. The 204 response carries no identity.
  - Claude: the wake text the attempt submitted, together with `promptSource` on
    Claude's user record. A start that
  cannot be attributed is a **person or another process taking over**: the
  episode closes as `newer-progress` and the pending work is left to normal
  delivery.
- **While the attributed attempt runs** (`Running`), nothing is dispatched.
  - If that turn ends **completed**, the episode closes as `recovered`.
  - If it ends in **error**, the failure is the same episode. It goes back to
    `Waiting` with that attempt's budget spent; no new episode opens.
  - If it ends **cancelled or interrupted**, the episode closes as `interrupted`.
    A person stopped it, so no further attempt is made and the work stays
    pending.
- **A native retry in progress** — an OpenCode `retry` status, or the record
  showing the harness re-running the failed turn — keeps the episode `Waiting`
  and pushes `next_at` past it. Budget is not touched.
- **Escalated is latched.** It is reached when the budget is spent, or an
  `Indeterminate` attempt occurs, or the class is non-retryable. It then stays
  `Escalated` for the same session and obligation. These are **not** resets:
  duplicate or replayed failure reports, recovery helpers reconnecting, monitor
  restarts, and further errors in the record.

  These **are** resets, and each closes the escalated episode:
  - a person's turn start (an unattributed start, as above);
  - a successful turn completion;
  - the obligation clearing;
  - the session binding changing.

  Only after a reset can a later failure open a new episode.
- **Restart reconciliation** reads the record from the attempt's `before`:
  - A `Claimed` attempt whose transport never answered becomes `Indeterminate`,
    then `Escalated`. It is never replayed.
  - A `Submitted` attempt is reconciled against the record. An attributed start
    after `before` makes it `Running`. If none appears within the confirmation
    bound (2 min after `claimed_at`, a constant), it becomes `Indeterminate`,
    then `Escalated`.
  - A `next_at` in the past fires once, after re-checking stages 2 and 3, with no
    burst for missed intervals.

### 5. Action: never interrupt, and use a conditional start or nothing

Recovery may only start a turn. It never sends an interrupt key or RPC, so
`pane_input::interrupt_and_submit_*`, `nudge` and `codex_interrupt::try_wake`
(which interrupts an in-progress turn first) are excluded. Each harness's action
is allowed only where its safety is established. Until then, the conservative
outcome is a **notice**.

- **Codex** uses a new `codex_interrupt::start_if_failed`. It reuses `target()`,
  the RPC framing and the binding check. In order:
  1. read `thread/turns/list`;
  2. require the latest turn to be the episode's failed turn, with status
     `failed`;
  3. re-check the binding;
  4. call `turn/start`;
  5. return the started turn's id.

  **Capability prerequisite:** what `turn/start` does on an active thread is
  unknown (no expected-turn guard is documented). This operation is enabled only
  after it is shown that `turn/start` on an active thread is rejected, or queued
  without cancelling the active turn. Until then, Codex recovery is notice-only.
  D1 must also be fixed.
- **OpenCode** uses a *recovery helper*, defined below. Before calling
  `promptAsync`, the helper discards the continuation (`Refused`) if its
  activity counter moved since the failure. That check only filters stale
  continuations. It is **not** a conditional start: `promptAsync` is an HTTP
  request the server accepts later, and a person's prompt or a native retry can
  begin before the plugin hears of it.

  **Capability prerequisite:** what the server does with `prompt_async` while
  the session is busy must be established for the installed version — rejected,
  or queued without cancelling the running turn. Until then, OpenCode recovery
  is notice-only.

  Once enabled, responses map onto the attempt as follows:
  - A 204 means `Submitted` (accepted, not started).
  - The supplied `messageID` appearing in `message.updated` means `Started`.
  - An error returned before the request was sent (no connection) means the
    transport was unavailable, which is not an attempt.
  - Any error or timeout after the request may have reached the server means
    `Indeterminate`, then `Escalated`.

  None of these reopens or refunds the budget.
- **Claude** uses a recovery helper on the `StopFailure` hook, with
  `asyncRewake: true`. **Capability prerequisite:** the hook's exit 2 must wake
  Claude, and must queue without cancelling if a turn is already running. Until
  both are shown, Claude recovery is notice-only. Typed pane input is not used;
  whether Claude queues a message typed during a turn is unverified.
- **Grok** gets notice-only. Typed input during a turn cancels it (D-grok:418),
  and no conditional start is known.

**Two kinds of parked monitor client.**
- A **deliverer** is today's stop-hook wait. It can receive inbox work, so its
  registration proves the agent is waiting. It ends eligibility, and if it
  registers during an episode, the episode closes as `newer-progress`: normal
  delivery has taken over.
- A **recovery helper** is a new client kind that names its session and the
  failed native turn id. It can **never** receive inbox delivery. It receives at
  most the monitor's dispatch for its episode: one wake text, sent exactly once
  per attempt, carried on the same monitor socket the stop-hook wait uses.
  - Its registration does not count as a waiter and does not close the episode.
  - It answers back with `Submitted` (injected or exited) or `Refused`
    (discarded).
  - If it dies before a dispatch, the transport is unavailable. That is not an
    attempt, and the episode waits for the next helper or escalates at the
    budget deadline.
  - If it dies after a dispatch without answering, the attempt is
    `Indeterminate`.

  The OpenCode plugin starts a helper instead of a deliverer after a failed
  execution, which removes the immediate re-arm (fixes D2). After a recovered
  turn ends normally, the plugin's ordinary deliverer resumes.

The wake text is clank's ordinary wake for the pending work. It never acks,
reads or marks anything.

### 6. Notices and the unknown-state fallback

- **Escalation, or a harness without a safe action,** writes one timeline row and
  a status-line state, for example "codex: turn failed — authentication_failed;
  waiting for you". Nothing further happens until a reset.
- **The fallback watchdog** covers an eligible agent with no failed turn in its
  record: the record is unreadable, or the harness is hung mid-turn. The monitor
  arms one deadline of `recovery.stall_after` (default 40 min; `off` disables
  it). When it fires, the action is **notice-only for every harness**:
  - The fallback never starts a turn. Where the record is readable, recovery is
    driven by stages 1–5 instead.
  - It posts nothing if the record shows the latest turn **cancelled or
    interrupted**. A person stopped it, and the work stays pending until they
    act.
  - It posts nothing if the record grew since the deadline was armed. The
    deadline is re-armed.

  With the fallback `off`, a recorded failure still recovers (stages 1–5); only
  unreadable or hung agents go unnoticed.

## State machine traces

The pure machine `(episode, event) → (episode, effects)`, for four cases.

**Fail, then helper, then retry fails, then the next attempt (OpenCode,
`server_error`; assumes its busy-session prerequisite has been established)**

| Event | Episode after | Effect |
| --- | --- | --- |
| record: execution failed, class server_error | Waiting, next_at = +1 min | none |
| helper registers for this failure | Waiting (helper is not a waiter) | none |
| next_at reached; stages 2–3 re-checked | Dispatched(1); attempt 1 Claimed with `before` | dispatch the wake to the helper |
| helper persists messageID m1 on attempt 1, posts `prompt_async`, gets 204 | Dispatched(1); attempt 1 Submitted (accepted, not started) | none |
| plugin event: `message.updated` with id m1 | Running(1); attempt 1 Started | none |
| record: that turn failed, server_error | Waiting, next_at = +5 min, 1 of 3 spent | none |
| a new helper registers | Waiting | none |
| next_at reached | Dispatched(2) | dispatch to the new helper |

**Exhausted, then a duplicate report and a restart (Codex, `server_overloaded`)**

| Event | Episode after | Effect |
| --- | --- | --- |
| attempt 3's turn fails | Escalated | notice |
| a duplicate `task_complete` line is replayed by a reread | Escalated (not a reset) | none |
| monitor restarts and reloads the episode | Escalated | none: the notice is already posted |
| a person types; an unattributed `task_started` appears | Closed(newer-progress) | none |

**Delayed failure, then a newer human turn (Grok)**

| Event | Episode after | Effect |
| --- | --- | --- |
| `events.jsonl`: turn 7 ended error | Waiting, next_at = +5 min (class unknown, no hook yet) | none |
| `events.jsonl`: turn 8 started (a person typed) | Closed(newer-progress) | none |
| turn 7's `StopFailure` report arrives late | Closed | none: not current, since turn 8 started after it |

**Interrupted turn, then fallback expiry (Codex)**

| Event | Episode after | Effect |
| --- | --- | --- |
| record: `turn_aborted{reason:"interrupted"}` | no episode (not an error) | none |
| the fallback deadline fires, still obligated | no episode | none: the latest turn was interrupted on purpose |

## Costs

- **Idle:** nothing beyond the monitor's existing fold.
- **Per obligated agent:** one file watch on its record, plus incremental reads
  of the bytes appended. A bounded tail read happens once, when the watch is
  armed.
- **Per attempt:**
  - Codex `start_if_failed`: a socket connect, `initialize`,
    `thread/loaded/list`, `thread/read`, `thread/turns/list` and `turn/start` —
    five RPCs.
  - Helper dispatch: one message on an already-open monitor connection.
- **Per deadline:** one tokio timer per eligible agent.
- **Never:** pane dumps, transcript rescans, model turns for liveness, or a
  watchdog per waiter.

## Rejected alternatives

- **Text matching on transcripts or terminal output.** Quoted errors in approval
  prompts and tool output would trigger it [observed, A2].
- **Hook timing as the source of order.** Reports can arrive after the next turn
  starts [documented, D-grok:384]. The ordered record is used instead.
- **The 40-minute watchdog as the main mechanism.** It is 40 minutes late, and it
  cannot tell a slow tool from a hang.
- **Typed or interrupt-first pane input, `nudge`, or `try_wake`.** Each can
  cancel a turn that started a moment before the key write. A preflight check
  cannot make the write atomic.
- **A hook or plugin that retries on its own.** That creates two decision owners
  with diverging budgets; D2 is exactly that.
- **An app-server subscription for observation.** It requires resuming the
  thread from a second connection.

## Change map

- **`a-failed-turn-is-reported-and-retried`**
  - New `cli/recovery.rs`: the episode type and its persistence, the
    class→budget table, attribution, and the pure machine above.
  - `cli/monitor.rs`: the eligibility fold, the record watches (armed while the
    agent is obligated) with bootstrap tail reads, one deadline per eligible
    agent, episode load and restart reconciliation, and the helper/deliverer
    distinction in client registration and dispatch.
  - `cli/web/transcript.rs`: turn-record extraction for Codex `event_msg` and the
    Claude `isApiErrorMessage` and user `promptId` records, plus a Grok
    `events.jsonl` line parser, shared with the watch.
  - `cli/stop_hook.rs`: the recovery-helper client mode. Claude's is installed
    as an `asyncRewake` `StopFailure` hook and stays disabled until its
    prerequisite is shown.
  - `cli/setup.rs` and `cli/grok_setup.rs`: Grok's `StopFailure`, used for class
    only.
  - The OpenCode plugin source written by setup: the failure report through a
    helper instead of an immediate re-arm.
  - `cli/codex_interrupt.rs`: `start_if_failed`, gated on its prerequisite.
  - `cli/pane_input.rs`: **not used** by recovery.
  - Config: `recovery.fallback_model` per agent.
  - Status and TUI: escalation and notice rows through the existing event log.
- **`pending-work-nobody-is-waiting-for-is-noticed`**
  - `recovery.stall_after`.
  - The notice-only fallback deadline in the same scheduler.
- **`codex-control-socket-may-be-a-link`**
  - `codex_interrupt::target()` follows a symlink to a socket owned by this user.

## Fixtures

All are driven through the real monitor fold, the record watches over fixture
files, the deadline and the `recovery.rs` machine. Transports are fakes that
record calls.

- **The four traces above,** step by step.
- **Codex `server_overloaded`,** with `start_if_failed` enabled behind a fake
  capability. It retries at +1 min, and the attributed turn's success closes the
  episode as recovered.
- **Quoted error text** inside a user message or guardian prompt produces no
  turn-end error.
- **Delayed report:** a Grok `StopFailure` for turn 7 arrives after turn 8's
  `turn_started`. The real `events.jsonl` path shows the episode closed and no
  dispatch.
- **Missed start:** the watch misses turn 8's change event, and the decision-time
  re-read of the record still shows turn 8. There is no dispatch.
- **Native retry in progress:** an OpenCode `retry` status holds the episode with
  its budget unchanged.
- **Server activity before the plugin hears of it:** a person's prompt reaches
  the OpenCode server before the plugin receives the event, and the helper's
  `prompt_async` is accepted with 204. The attempt is `Submitted`, not a proven
  idle start. The first start the plugin then sees is the person's (no `m1`), so
  the episode closes as newer progress. Attempt 1 stays recorded as spent, and
  if `m1` later appears it is attributed to attempt 1, not counted as a reset.
- **204 with no body:** a `prompt_async` 204 never yields an attributed start by
  itself. Only `message.updated` with the supplied `messageID` does. Without it
  within the 2-minute bound, the attempt becomes `Indeterminate`, then
  `Escalated`, never a budget reset.
- **Error after send:** a timeout or 5xx after the request was sent gives
  `Indeterminate`, then `Escalated`, with no retry on any transport.
- **Manual mode:** auto off gives notice-only and no transport call.
- **A pending question or block:** no dispatch.
- **Interrupted on purpose:** Codex `turn_aborted`, Grok `cancelled`, or an
  OpenCode abort produces no episode, and the work stays pending.
- **Action races a new turn:** the record changes between claim and
  `start_if_failed`'s re-read, giving `Refused`, no `turn/start` and no
  interrupt call of any kind.
- **Helper held across backoff:** the OpenCode helper stays connected through
  +1 min and receives exactly one dispatch. Its registration does not close the
  episode.
- **Deliverer registers mid-episode:** the episode closes as newer progress, and
  the helper's dispatch is withdrawn.
- **Restart with a `Claimed` attempt:** it becomes `Indeterminate`, then
  `Escalated`, with no replay.
- **Restart with a `Submitted` attempt:** an attributed start within 2 min makes
  it `Running`. Without one, it becomes `Indeterminate`, then `Escalated`.
- **Repeated failures:** a clank-attributed turn failing spends budget in the
  same episode, and `Escalated` survives duplicate reports and restart.
- **Offline catch-up:** a monitor down across two retry times fires one attempt.
- **Session replacement** closes the episode.
- **Persistent auth failure:** one notice, zero attempts.
- **OpenCode repeated API error:** backoff through the helper, not a hot loop.
- **Fallback off:** a recorded failure still recovers, and an unreadable record
  gets no notice.
- **Fallback with an interrupted latest turn:** no notice.
- **Missing prerequisites:** an unverified harness capability gives notice-only.

Live acceptance checks: each gates its harness's action and does not block this
research.

- OpenCode: `prompt_async` on a busy session, and that a supplied `messageID` is
  the id of the user message that `message.updated` later reports.
- Codex: `turn/start` on an active thread.
- Claude: `asyncRewake` on `StopFailure`, and its behaviour during a running
  turn.
- Grok: one real `StopFailure` payload, to confirm it is only advisory.

## Appendix: evidence

Paths are on this machine; no message content beyond the quoted error strings.

- **A1. Incident rollout:**
  `~/.codex/sessions/2026/09/25/rollout-2026-09-25T18-08-46-01a0d79c-16a4-75b3-b2ef-f007aae0a511.jsonl`,
  lines 18770–18795. Reproduce with
  `sed -n 18780,18795p <file> | jq -c '{timestamp,type,t:.payload.type,error:.payload.error}'`.
  Failure record:
  `{"type":"event_msg","payload":{"type":"task_complete","turn_id":"01a1092f-81dd-75d2-9fac-5ab2734f4753","last_agent_message":null,"error":{"message":"Selected model is at capacity. Please try a different model.","codex_error_info":"server_overloaded"}}}`.
- **A2. Guardian retry:**
  `~/.codex/sessions/2026/10/05/rollout-2026-10-05T11-02-05-01a1095e-1cd6-7921-b60c-25032a3cc999.jsonl`
  shows the failed turn at line 12 and the successful turn at lines 14–23.
  Quoted-only matches are in `rollout-2026-10-05T18-13-33-01a10ae9-…jsonl` and
  `rollout-2026-10-05T20-38-29-01a10b6d-…jsonl` (line 10, inside `input_text`).
- **A3. All structured error codes:**
  `grep -rho '"codex_error_info":"[^"]*"' ~/.codex/sessions | sort | uniq -c`.
- **A4. Codex hook events:** `strings -n 6 <codex binary> | grep -o 'UserPromptSubmitSubagentStartSubagentStopStopInterrupt'`
  plus the hook-name run near `PreToolUsePermissionRequestPostToolUsePreCompactPostCompactSessionStartSessionEnd`.
  The binary is `~/.codex/packages/standalone/releases/0.160.0-aarch64-apple-darwin/bin/codex`.
- **A5. Control socket:** `ls -la ~/.codex/app-server-control/` shows
  `app-server-control.sock -> /private/tmp/codex-daemon-501/<hash>`. Python's
  `os.lstat` reports it is not a socket; `os.stat` reports it is.
- **A6. Claude `StopFailure` construction:**
  `strings -n 6 ~/.local/share/claude/versions/2.1.280 | grep 'hook_event_name:"StopFailure"'`
  shows `{…hook_event_name:"StopFailure",error:g,error_details:e.errorDetails,last_assistant_message:s}`.
- **A7. OpenCode retry status:**
  `strings -n 8 /opt/homebrew/Cellar/opencode/1.18.30_2/bin/opencode | grep -o 'session.retry.scheduled.\{0,120\}'`
  shows `{type:"retry",attempt:W.data.attempt,message:W.data.error.message,next:W.data.at}`.

- **A8. Claude transcript turn records:** user records carry `promptId` and
  `promptSource` (e.g.
  `~/.claude/projects/-Users-llfourn-src-clank/8e702960-295f-4755-be95-2f491e53a293.jsonl`).
  An API failure is recorded as `{"type":"assistant","isApiErrorMessage":true,"error":"<class>","apiErrorStatus":<http>,…}`.
  Seen here: `oauth_org_not_allowed` (403), `rate_limit` (429), and `unknown` for
  a 529 "API Error: Overloaded". Reproduce with
  `grep -h '"isApiErrorMessage":true' ~/.claude/projects/*/*.jsonl | jq -c '{error,apiErrorStatus,timestamp}'`.
- **A9. Grok ordered events:**
  `~/.grok/sessions/<url-encoded cwd>/<session>/events.jsonl`, with
  `{"type":"turn_started","turn_number":N,…}` and
  `{"type":"turn_ended","outcome":"completed|error|cancelled|interrupted"}`
  (`cancelled` adds `cancellation_category` and `cancellation_context.trigger`).
  Counts across all sessions here: completed 1193, cancelled 36, error 31,
  interrupted 1. Reproduce with
  `cat ~/.grok/sessions/*/*/events.jsonl | jq -r 'select(.type=="turn_ended").outcome' | sort | uniq -c`.

- **A10. OpenCode `promptAsync` contract:**
  `~/.config/opencode/node_modules/@opencode-ai/sdk/dist/gen/types.gen.d.ts`
  (SDK 1.18.11, used by clank's plugin). `SessionPromptAsyncData` is
  `{ url: "/session/{id}/prompt_async", body?: { messageID?, model?, agent?, noReply?, system?, tools?, parts } }`,
  and `SessionPromptAsyncResponses` is `{ 204: void }` ("Prompt accepted").
  Current upstream source agrees there is no response body:
  <https://github.com/anomalyco/opencode/blob/dev/packages/opencode/src/server/routes/instance/httpapi/groups/session.ts#L1745>.

Documented sources:

- **D-claude:** <https://code.claude.com/docs/en/hooks#stopfailure>, with
  `prompt_id` among the common input fields.
- **D-codex:** <https://learn.chatgpt.com/docs/app-server#turn-events>, covering
  turn events, `thread/status/changed`, the subscription model and the
  `turn/steer` `expectedTurnId` guard.
- **D-grok:** `~/.grok/docs/user-guide/10-hooks.md` as installed with 1.0.46. The
  line numbers cited above are into that file.
- **D-opencode:** <https://opencode.ai/docs/plugins/>, which names the event
  list but not its payloads; those come from A7.

## Queued implementation work

- `a-failed-turn-is-reported-and-retried`: stages 1–6 except the fallback.
- `pending-work-nobody-is-waiting-for-is-noticed`: the stage-6 fallback.
- `codex-control-socket-may-be-a-link`: D1.
