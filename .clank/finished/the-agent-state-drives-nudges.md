# the-agent-state-drives-nudges
# Clank knows each agent's state, and nudges an idle agent whose turn it is

## Why

In `split-backup-formal` the master sat stopped for hours with work pending. Clank's only
response was a stall notice written into that agent's own event log, which it could not
receive because it was not waiting: two `⚡ @claude recovery ?` rows saying "claude has
had pending work for 40m and is not waiting for it". The user: "I don't think we need a
notice at all. We should nudge it."

`research-agent-stalls-and-provider-capacity-recovery` made the stall watchdog notice-only.
It rejected pane input, `nudge` and `try_wake` because they "can cancel a turn that
started a moment before the key write", and because a 40-minute timer "cannot tell a slow
tool from a hang". Both objections are about acting without knowing the agent's state. The
answer is to know it.

What is already known: harnesses run no Stop hook after an API-error turn, so hooks cannot
see "model too busy". But Claude's transcript and Codex's rollout record it, and clank
already reads them for provider-error retries (`recovery.rs`, from
`a-failed-turn-is-reported-and-retried`). The pieces exist but are not gathered into one
view, and nothing acts on "idle, with work, and not waiting".

Codex on 29f7385 corrected the premise above. A record says what HAS happened, not what
the harness does next. A person can type, or a turn can start, between any check and a key
write, so knowing the state does not by itself make typed input safe. This plan therefore
defines the observation, the eligibility, and the action separately, and makes the action
one that is safe without an exclusive claim over the terminal, or no action at all.
Production `recovery::transport_for` currently returns `None` for every harness: provider
retries are designed but never dispatched. This plan gives them the same action as idle
nudges, under the same evidence.

## What to build

### 1. The action: submit without interrupting, or nothing

An automatic nudge never sends an interrupt key or RPC. It types its text and submits it,
as `pane_input::say_to_pane_for_tool` does, never `interrupt_and_submit_*`. Each harness
gets a `Resume` capability in one table in code. Every row is a PROPOSED candidate, and
every row is disabled (status-only) until its evidence is committed beside the table:

| Harness | Candidate | What its evidence must show |
| --- | --- | --- |
| Claude Code | typed submit | input typed during a turn is queued for after it, not run over it |
| Codex | typed submit | Enter during a turn steers or queues it, and does not abort |
| OpenCode | the plugin's `prompt_async` | a prompt to a busy session is queued, not run over it (its 204 is acceptance only) |
| Grok | none | (typed input cancels the running turn, D-grok:418: never a candidate) |

Evidence is a committed record: the harness and its version or version range, the adapter
it applies to, the observed behaviour, and how it was observed. A capability applies only
to an installed harness within its evidence's range; a different version is
status-only. With no evidence committed, every harness is status-only, and this plan is
complete and correct in that state. Live verification is a recommendation, not a gate.

**The final guard, and the race after it.** Immediately before the send, clank revalidates
eligibility (§4) and reads the pane's screen once (`zellij action dump-screen`). A newer
turn in the record, or a non-empty, unreadable or ambiguous input line, vetoes the send,
and the agent's state says why. That guard is the last check, and it is not atomic.
Anything a person types after it is in the acknowledged race. Because submit-only types
AND presses Enter, the race can submit a person's unfinished draft with the hint appended
to it. Nothing is cancelled and no turn is interrupted, but the draft is sent early. The
plan does not claim the terminal is held. No input-preserving transport is known for the
typed-submit harnesses, and the harness capability evidence must also state this
limitation.

### 2. Unknown is status-only by default

`unknown` (the record cannot be read) withholds terminal input and publishes the reason
(`state: unknown: transcript unreadable`). An unreadable record is distinct from a
readable failed turn whose error class is unknown, which follows the class policy below.

The user's blind fallback is an opt-in, `recovery.blind_nudge = "<interval>"` (off by
default). Its stuck interval is identified without a turn, by `(session, the obligation's
item identities)`. It resets when the obligation's items change, the record becomes
readable, or the binding changes.

Within one stuck interval, the fallback SENDS at most once. With the record unreadable, a
sent nudge cannot be confirmed, so a submitted or uncertain send is `Indeterminate` and
latches; it is never sent again. Only outcomes that definitely sent nothing are retried:
no pane, a draft veto, the capability unavailable. Each retry waits the interval, doubling
to 4 hours, and there are at most 5 checks per stuck interval. Its documented limits: it
can submit a draft in the §1 race, or queue behind a turn clank could not see.

### 3. One reconciled state, as a projection

Each agent gets one `AgentState` in the monitor:

- **Identity:** the repo agent, its roster role, and its bound session and tool, from the
  roster and the binding; and the monitor that observed it, by its published pid and birth
  token (`monitor::published`, `ProcToken`).
- **Activity:** `working`, `idle`, `failed(class)`, `asking` (a permission prompt or user
  question, from the record), `interrupted`, `unknown(reason)`, or `unobserved` (clank is
  not reading this agent's record, because it owes nothing). It comes from the record's
  latest turn, as `recovery` already folds it. `unobserved` is never idle. When
  observation resumes, the record's tail is read (`Tail::open`'s bounded window) before
  any decision is armed.
- **Suppressors,** independent of activity: a pending clank question or block, live
  attendance (`prove_attendance`), a parked deliverer, and a native retry in progress.
- **Since:** when the activity last changed, which is semantic time, not file time.

Precedence:
- The record's order decides activity, and a delayed hook never overwrites a newer turn.
- A recovery helper is not a deliverer.
- A session replacement starts a new state.
- A record truncated or replaced restarts the tail from a bounded window.
- A read failure is `unknown`.
- `interrupted` says the turn was stopped, not by whom. It suppresses any nudge until a
  newer turn or a reset.
- An agent without zellij is not `gone`. `gone` is reserved for a binding whose process
  is dead.

The monitor publishes it, per agent, to `.clank/agents/<label>/state.json`. It is
rewritten only when it changes semantically, and stamped with the agent's role, session,
and the observing monitor's pid and birth token. It is a projection for the TUI and status
line, never read back as authority. A reader shows it as stale when any stamp no longer
matches: the binding's session, the roster's role, or a monitor that is no longer the
published and living one. A failed publication changes nothing else.

### 4. Eligibility, checked at the decision and again at the send

The gate is recovery's existing eligibility (stage 3 of the research: obligated, bound,
auto mode effective through `drive_gate`, no question or block, no deliverer, no
attendance). Added to it:
- actionable work for THIS agent's role, re-derived from the fold at the send;
- no newer turn in the record since the decision;
- no draft (the pane veto);
- a supported `Resume` capability.

Any change that ends eligibility cancels the armed decision and leaves no deadline: work
clearing, a role or session change, auto off, a person's turn, or new progress. Another
agent's turn is never a reason to nudge this one.

### 5. One durable owner: recovery's episode, extended

Idle nudges, failed-turn retries and the opt-in fallback are one arbitration: the recovery
`Driver`, its episodes and the nudge history just finished. Nothing runs a second
schedule.
- **A stuck interval** is an episode opened by `(session, obligation, the record's latest
  turn)`: a failed turn, as now, or an idle turn that ended with work owed. Its trigger
  says which (`Trigger::Retry`, and new `Trigger::Idle` and `Trigger::Fallback` in
  `nudge_log`).
- **Claim before effect,** one attempt in flight, and a confirmation deadline for the
  turn to appear, as recovery has today.
- **A submitted attempt whose turn never appears** is `Indeterminate`, then escalated. It
  is never sent again on the next fold.
- **Budgets:** an idle episode gets 2 attempts, at +2 min and +15 min after its settle
  period (60 s of unchanged state). A failed turn keeps its class budget. Non-retryable
  classes (authentication, billing, policy) and a native retry never become an idle
  nudge.
- **One episode across the switch.** A turn an idle nudge started that fails on a
  provider error stays in the same episode. Its spent attempts stay spent: the remaining
  budget is the smaller of the idle budget left and the class budget left. Nothing is
  refunded.
- **Outcomes:** `Refused` before any send (no pane, draft veto, newer turn, capability
  absent) spends nothing and may be retried at the next decision. `Submitted` waits for
  its turn, and confirmation fails as `Indeterminate`. `Indeterminate` latches. Nothing
  that may have reached the harness is resent.
- **Escalation latches** until a reset (a person's turn, completion, the obligation
  clearing, a binding change), as recovery's does.
- **Restart and offline** follow recovery's reconciliation: a claimed attempt is never
  replayed, and an overdue one is settled. The nudge history's pending lifecycle records
  every attempt.

### 6. Provider retries use this action

`transport_for` gains the `Resume` capability as its transport where the harness has one:
typed submit for Claude and Codex, the plugin for OpenCode, none for Grok. Codex's
`turn/start` stays as it is, behind its own capability. Each harness's retry is therefore
enabled exactly when its evidence is committed, and unsupported otherwise, with the
reason in its state.

### 7. Resources and delivery

- **Observation** uses `RecordWatch` and `Tail` as recovery does: one watch per
  directory, appended bytes only, the existing next-deadline scheduler. It runs only for
  obligated agents. An agent owing nothing is not tailed, and its state is `unobserved`.
- **The pane is read** only for a send candidate, once per attempt.
- **No periodic work:** no dump, rescan or elapsed-age timer. Ages are derived in the UI
  from `since`.
- **The nudge text** is a hint to read pending work (`clank status`), like the recovery
  wake text, never a presentation of the work itself. It earns no wake receipt, ack,
  reply acceptance or attendance. Normal delivery and required replies stay
  authoritative.
- **The stall watchdog's agent-addressed notice is removed.** Provider escalation keeps
  its explanation, now carried in the agent's state and status line (`failed:
  authentication_failed · waiting for you`) instead of an event written to the stuck
  agent.

## Acceptance

- **Harness capability.** Per harness: with no evidence committed, Claude, Codex and
  OpenCode are all status-only, and Grok is always status-only. With evidence for a
  harness's version, that harness is nudged by its candidate, and another version stays
  status-only. No path ever writes an interrupt key.
- **Unobserved.** An agent owing nothing shows `unobserved`, never `idle`. When work
  arrives, its record is read before any decision is armed.
- **Freshness.** A state stamped by a monitor that has stopped, or with an old role or
  session, is shown stale.
- **The fallback's transitions.** A draft veto and no pane each retry within the cap. A
  submitted fallback attempt, which cannot be confirmed, latches without a resend. An
  obligation change resets the interval.
- **One budget.** An idle-started turn that fails on a provider error spends from the
  same episode, and nothing is refunded.
- **Unknown.** Status-only by default, nudged only with `recovery.blind_nudge` set, within
  its cap and backoff.
- **Suppressors.** A permission prompt, a user question, a clank block, attendance, auto
  off, and `interrupted` each suppress a nudge.
- **Stale state.** A publication stamped with an old session or role is shown stale, and
  is never used to decide.
- **The final guard.** A newer turn, or a draft, seen by the final revalidation vetoes the
  send. The test makes the change after the decision and before the guard. A change after
  the guard is the acknowledged race. A test documents it: the typed hint and Enter go to
  the pane as they are, and no interrupt key is written.
- **Concurrency.** An idle nudge and a provider retry for the same agent produce one
  attempt.
- **Outcomes.** A refusal that definitely sent nothing spends nothing and is retried
  within its bound; an uncertain send latches without a resend. A submitted nudge whose
  turn never appears becomes indeterminate, once.
- **Lifecycle.** Retirement, monitor restart, offline catch-up and record corruption each
  follow §5.
- **Retries.** A native retry defers the nudge, and a non-retryable class never becomes a
  nudge.
- **No side effects on delivery.** No wake receipt, ack or attendance is ever written by
  a nudge.
- **No idle work.** An idle repo has no deadline, no tail and no pane read.
- **History.** Every attempt is in the nudge history with its trigger.
- **The `split-backup-formal` case.** The master stopped with continue pending, Claude's
  capability evidence is present for its version, and its input line is empty. After the settle period
  it is nudged once by submit-only, and the nudge is in the timeline.
- **Mutation-checked:** an interrupt key on the automatic path, a nudge over a draft, a
  nudge to an `asking` or `interrupted` agent, unknown nudging without the opt-in, a
  resend after a submitted attempt, and an unbounded fallback each fail a test.

## Out of scope

Live verification of the harness capabilities (recommended; the code is correct without
it). What the nudge says beyond the hint. Agents clank does not manage.

## As built

- **Capability.** `resume.rs` holds the candidate table and `EVIDENCE`, which is empty:
  every harness is status-only, and the published state says why (`cannot_resume`).
  `recovery::transport_for` returns the capability or its reason, and adds
  `Transport::Typed` for the typed-submit candidate.
- **The typed send.** The send is split across the monitor's loop:
  1. A task off the loop finds the pane (`pane_target::discover`), reads its screen
     (`open_zellij::dump_pane_screen`, bounded at 3 s) and folds the repo afresh, then
     hands all three back.
  2. The loop makes the last look (`monitor::may_type`), from the same live state a
     deadline is checked against:
     - `recovery_gate`: the binding, a parked deliverer, auto mode, and any question,
       block or attendance;
     - the input read as empty (`composer.rs`);
     - the agent's role, from the roster on disk, and its owed work, derived by
       `owed_items` from that fresh fold with a timer backlog read afresh (never the
       loop's cache, which keeps an acknowledged recurring row until it is
       invalidated): a change already on disk vetoes the send whether or not the loop
       has folded it yet;
     - then `Driver::may_send`, which requires that this is still the agent's claimed
       attempt, unsuppressed, under the same role, still owing work (a fallback's work
       still its interval's), and that, with the record re-read, the target is still
       its latest, ended turn.
  3. Only then a bounded, submit-only write (`pane_input::submit_async`) runs. Its
     `PaneWrite` says whether a failure wrote nothing (`Unavailable`) or may have typed
     something (`Indeterminate`).
- **Originating role.** An episode records the roster role it was opened under, and
  `decide` keeps an episode only while that is the agent's role. A role change therefore
  retires the old interval and arms the new role's afresh, whenever it is applied: at
  the fold that sees it, at the end of a storage or issued-number hold that deferred
  it, or after a restart. `may_send` requires the live role, the observed role and the
  episode's role to agree, so an obsolete claim is fenced at once, even while its
  retirement cannot yet be stored.
- **Attempt identity.** Attempt numbers are control state.
  - The highest one issued to an agent is kept in `recovery-issued.json`. It is stored
    before the episode that uses it, and neither is acted on until both are stored.
  - A reset (a role, work or obligation change) restarts the budget but never the
    numbers, and the nudge history's availability has no bearing on them.
  - A file that is there but cannot be read holds new claims; it is never read as
    zero. It is read only past any storage hold, and after an incoming answer is kept,
    so a fault neither drops a known claim's result nor moves the hold. When a hold
    ends, the decision owed across it is made from the agent as the folds since have
    left it: work cleared forgets the episode, and a suppressor holds it with no
    deadline.
  - Answers, history rows and tags therefore never alias between intervals or across
    restarts. A tag is matched only inside its parentheses, so attempt 1's tag is not
    found in attempt 12's.
  - History records each attempt's place in its own budget (`Pending::ordinal`),
    separately from its number. A blind nudge's place is its check, since its cap is on
    checks.
- **The input.** `composer.rs` reads the whole input region, not just one line:
  Claude's between its two rules, and Codex's over the run of rows on its background
  band, blank rows included. A band must be drawn above the prompt and must end before
  the screen does.
  Continuation lines count. Dim text is a placeholder only when it matches a known
  one; any other dim text, and any input it cannot place or bound, reads as
  unreadable and vetoes the send. Colour SGR parameters (`38;2;r;g;b`) are not read
  as attributes. Its fixtures include the lines captured from Claude Code 2.1.280
  and Codex.
- **One owner.** Recovery's `Episode` gains a `Kind`: `Failed`, `Idle` or `Fallback`.
  - An idle episode opens on an anchored, completed latest turn while the agent is
    obligated. It waits 180 s (a 60 s settle, then 2 min) and then 900 s, from
    `IDLE_WAITS`.
  - A nudged turn that completes with work still owed stays idle, on the same budget.
  - A nudged turn that fails takes its class, and `next_wait` allows only what both
    budgets still have.
  - A fallback opens only when the record cannot be read, `recovery.blind_nudge` is set,
    and no live episode exists. Its identity is an FNV-1a hash of the owed items'
    `WaitItem::identity`s. It is forgotten when the record becomes readable or the work
    changes.
  - The fallback's no-send answers back off from the interval, doubling up to 4 h, for 5
    checks. A submitted fallback latches.
  - History records `Trigger::Idle` and `Trigger::Fallback`.
- **Claude turn ends.** Claude's transcript reader now reads `system/turn_duration` as a
  turn's end and `[Request interrupted by user` as an interrupt. The first end of a turn
  wins, because Claude also writes `turn_duration` after a failed turn. Before this,
  Claude turns never read as completed.
- **Eligibility drives the deadlines.** Every fold now decides for every obligated
  agent, passing what suppresses it (`suppressed`: auto off, or attending or
  asking). A held episode has no deadline: nothing looks again until a fold with no
  hold gives it back, a minute out. Auto off is a hold, not an escalation, so turning
  auto back on resumes. A role change forgets the old role's episode. A pending
  dispatch keeps its accounting deadline whatever happens.
- **Evidence.**
  - A failed read clears the reading and its snapshot, so the state says unknown.
  - The shared `Tail` records the file's device and inode, so a record renamed over
    the path is read afresh even at the same length.
  - A new binding resets the activity's age.
- **State.** `agent_state.rs` defines `AgentState`. `Driver::publish` writes it to
  `state.json` when it changes in meaning, and `since` moves only when the activity
  changes. Readers get a `View`: the state, plus why it is stale. A stale view
  claims no activity.
  - The TUI roster row draws the plan-duty spinner only while a current state says
    working (or none is known); otherwise it shows the state's brief.
  - The roster row says `gone` when there is no pane and no running process for the
    binding (the TUI's own presence and `lost` evidence; a headless agent is never
    gone).
  - The agent page and plain `clank status` give the full line: why the record is
    unreadable, why clank waits for a person, and why it cannot resume.
  - `clank status --json` carries the structured state.
- **Removed:** the stall watchdog (`stall.json`, `Stall*`), which was a second,
  elapsed-age schedule, and every agent-addressed recovery event (`Act::Notice`,
  `Effect::Notice`, `write_recovery_notice`). An escalation's reason stays on the episode
  (`waiting_for_you`) and in the state. `recovery.stall_after` is replaced by
  `recovery.blind_nudge`, which is off by default. Old configs still load, because
  unknown keys are ignored.
- **Pre-existing:** `git_boundary` fails on the base commit too, at
  `stop_hook.rs:4065` and `grok_hook_tests.rs:12` (test code). The
  `source_group::…inflight_retirement_defers` test is flaky under load and passes alone.
- **Mutation-checked:** 16 mutants, all caught. Mutations to the code: an interrupt on the
  typed path, a guard that skips the composer, one that skips the record, a draft read as
  empty, an interrupted agent nudged, a held (asking) agent nudged, unknown nudged
  without the opt-in, a resend after a submitted blind nudge, an unbounded fallback, the
  idle budget refunded on failure, unobserved shown idle, a Claude failure overwritten
  by `turn_duration`, a record recheck that always passes, an idle episode opened while
  owing nothing, no settle period, and a stale check that ignores the monitor.
- **Revision mutation-checked:** 18 more mutants, all caught:
  - a fold that decides only when armed;
  - a hold that keeps its deadline;
  - a failed read that keeps the old evidence;
  - a tail blind to replacement;
  - a new session that inherits the old age;
  - an ignored role change;
  - `may_send` ignoring suppressors, or trusting the old read;
  - the last look ignoring the live gate, or the roster;
  - the composer ignoring continuation lines, taking any dim text as a placeholder,
    or reading colour numbers as dim;
  - a late write failure classed as no-send;
  - the line omitting why clank cannot resume;
  - the roster row always showing the duty spinner;
  - a lost process read as gone;
  - plain status repeating the reason.
- **Second revision mutation-checked:** 8 more mutants, all caught:
  - attempt numbers restarting each interval;
  - a tag read inside a longer one;
  - `may_send` ignoring the live role, or the live work;
  - the last look trusting the fold for work;
  - a blank row ending Codex's input;
  - no band required above the prompt;
  - the send path ignoring the composer.
- **Third revision mutation-checked:** 3 more mutants, all caught: the issued number
  not stored, an unreadable one read as zero, and history showing the global number
  as the budget's place. The cached backlog cannot be mutated back in: the last look
  no longer takes it.
- **Fourth revision mutation-checked:** 5 more mutants, all caught: answers dropped
  across an issued-number fault, the hold bypassed by that read, a hold's end ignoring
  cleared work or a suppressor, and fallback history counting sends rather than
  checks.
- **Fifth revision mutation-checked:** an episode outliving a role change, and an
  episode recording no role, are both caught. Comparing the live role with the observed
  role instead of the episode's is equivalent, given that `may_send` also requires the
  observed role to be the episode's.
