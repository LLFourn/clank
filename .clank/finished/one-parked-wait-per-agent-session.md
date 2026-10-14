# one-parked-wait-per-agent-session
# The monitor keeps one parked wait per agent session: a newer one supersedes the older, so a wake is delivered once

## Why

In `~/src/penlock-experiment`, claude's turn at 2026-10-14 21:42:49 ended with six
"Stop hook feedback" wakes at the same instant. All six were identical: the same
`promote: scan-shows-the-card` item, the same text.

The hook runs as `clank stop-hook --tool claude --loop asyncrewake`. Every Stop with
nothing owed parks a `clank wait` in the background and attaches it to the repo's
monitor. Claude Code does not cancel that background hook when the next prompt
arrives. So over a run of user-driven turns, one session accumulates several parked
waits. When work appears, the monitor serves every one of them, and each rewakes the
agent with the same items.

The monitor is the one place that sees every parked wait of an agent. But
`admit_into` admits every client, and refuses a duplicate only for an automatic Grok
wait (`automatic_grok_client`). For Claude and Codex, nothing retires an older wait
when a newer one for the same session arrives.

## What to build

### 1. Who may be replaced, and by whom

- **The newcomer is checked first.** It is admitted by the existing rules
  (`client_refusal`: binding, roster, role) before it can displace anything. A session
  no longer bound to its author is refused exactly as today.
- **Same wait only.** An admitted client replaces an older one only when both are
  `Served::Agent` for the same author, with a holder (tool and session) that is
  present and equal.
  - Observers and recovery helpers (`Served::Helper`) neither displace a parked agent
    wait nor are displaced by one.
  - Clients without a holder are untouched.
  - Different sessions or authors are separate keys, each still subject to the
    binding checks.
- **"Newer" means admission order.** It is the order the monitor admits clients. No
  client-supplied generation, token or registration key is added.
- **Grok keeps its rule, in its direction.** The automatic Grok wait's existing
  refusal (keep the connected one, refuse the newer one) stays. That wait is a task
  Grok starts and manages explicitly, so the refusal tells the agent to keep or
  cancel it. Claude and Codex hook waits are background hooks the harness never
  cancels, so the newest is the live one and the older ones are what this plan
  retires. Both rules live in `admit_into`, with that reason stated beside them.

### 2. A terminal supersession the old wait cannot miss

- **A distinct note.** Supersession is a new note, `Note::Superseded`. It is
  delivered so that a full queue cannot drop it: a parked writer's queue is
  `CLIENT_QUEUE` (two) deep and may hold beats.
  - **The farewell frame.** Each writer task gets a farewell slot: a oneshot the
    retirement fills before closing the client's queue. On supersession the monitor
    fills it with the `Superseded` note and drops the sender.
  - **Bounded by the existing writer.** The writer drains what is queued, then writes
    the farewell, then ends. Each write is bounded by `WRITE_BUDGET`. Its handle goes
    into `leaving` like any retired writer.
  - No idle retry and no timer per waiter is added.
- **The client ends silently.** On `Note::Superseded`, `clank wait` ends with
  `WaitSuperseded`, the existing `superseded` envelope, with no local fallback. The
  stop hook treats that envelope as a silent exit: no wake, no wake receipt, no
  repeat-count increment.
- **Ordinary EOF is unchanged.** EOF, a malformed note, or a reader failure still
  returns `Shared::Alone` and the wait watches locally. That is the recovery path when
  a monitor crashes or wedges, and it stays.
  - Only the recognised terminal note ends a wait silently.
  - A wait whose `Superseded` frame was never written, because its peer stopped
    reading or the monitor died first, falls back like any other. That is the
    documented residual.

### 3. What one delivery means

- **What is covered:** eligible waits parked at the same time, and work not yet
  dispatched when a replacement is admitted.
- **Dispatched work is untouched.** `serve_to_cached` already retires a client once
  its first items are queued, moving its writer into `leaving`. A delivery already
  dispatched to a wait is not retracted by a later admission. Its queued items keep
  their custody, and its writer still drains them.
- **Owed work still follows its own rules.** Unacked durable events stay owed, and a
  later wait is handed them as it is today. This is not exactly-once processing; it
  stops one change waking the same session once per parked hook.

### 4. Without a monitor

A wait running alone (no monitor reachable) is not covered. `WaitBinding` checks the
session's binding, but it does not tell two waits in one session apart, and this plan
adds no generation or token to make it.

### 5. Compatibility

The new note needs a `PROTO` bump. A running older binary is not retrofitted:
- an older client attaches only to a monitor of its own protocol, and that monitor
  cannot send the note;
- when such a monitor is replaced, its parked waits see EOF and fall back to watching
  locally.

So waits parked by an older build can still deliver duplicates until they deliver or
their harness restarts. After install, a restarted session, or one whose parked hooks
have all delivered once, gets the new behaviour.

## Acceptance

- **Replacement:** two admitted agent clients with the same author and holder. The
  older is sent `Superseded` and retired; only the newer is served the next items.
- **The farewell survives a full queue:** an older client whose queue holds beats up
  to `CLIENT_QUEUE` still has its `Superseded` frame written after them, by the same
  writer.
- **Eligibility:**
  - observers, helpers and holderless clients neither displace nor are displaced;
  - a newcomer refused by `client_refusal` displaces nothing;
  - different sessions or authors coexist.
- **Grok:** an automatic Grok duplicate is still refused, and the connected one
  stays.
- **The client:** a decoded `Superseded` note ends `clank wait` with the superseded
  outcome and no local fallback. The stop hook emits nothing for it, records no
  receipt, and counts nothing.
- **Recovery is unchanged:** EOF and a malformed frame still give `Shared::Alone`.
- **The dispatch boundary:**
  - with replacement before dispatch, only the newer wait is served;
  - a wait already dispatched keeps its delivery;
  - the newer admission is then handed owed work by the normal rules.
- **The penlock case:** several parked waits for one session, then an item appears:
  exactly one delivery.
- **Older protocol:** admission and refusal tests for older clients stay separate
  from these.

## Out of scope

- Claude Code's own handling of background hooks.
- Uniqueness of waits when no monitor is running.

## As built

- **Replacement** (`monitor.rs`, `admit_into`): after the existing refusal checks (and
  the automatic Grok refusal, unchanged), any connected client that is `same_wait` as
  the newcomer is removed and `supersede`d.
  - `same_wait` means both are `Served::Agent` for one author, with a present and
    equal holder.
  - The order is admission order. No key or generation is added.
- **The farewell frame:**
  - `Client` has a `farewell` slot, a oneshot to its writer.
    `write_to_within(socket, queued, farewell, …)` writes everything queued, then
    the farewell once the queue closes, each write within `WRITE_BUDGET`.
  - `supersede` fills the slot with `Note::Superseded` and drops the sender; the
    writer goes into `leaving` like any retired writer.
  - Writers without a farewell (control clients, tests) are unchanged.
- **The client:** `shared_wait_within_on_idle` returns `WaitSuperseded` on
  `Note::Superseded`, the existing path to the `superseded` envelope and a silent stop
  hook. EOF and malformed frames still return `Shared::Alone`.
- **`PROTO` is 14.** Fixtures that hard-coded 13 now use `PROTO`.
- **Tests** (`monitor::superseding_tests`):
  - a newer wait of the same session supersedes the older, whose farewell is
    `Superseded` and whose queue closes;
  - only the same agent session is the same wait (observers, helpers, holderless
    clients, other sessions and other authors are not);
  - a refused newcomer displaces nothing;
  - the farewell survives a full queue, written after the queued beats by the same
    writer;
  - a dispatched delivery stands, and the next admission is owed work normally;
  - in `wait`, a superseded wait ends without watching for itself.
- **Covered by existing tests:**
  - Grok's duplicate refusal;
  - EOF and silence recovery (`silence_ends_the_sharing` and the died/wedged-monitor
    tests);
  - the hook's silent handling of the superseded envelope.
