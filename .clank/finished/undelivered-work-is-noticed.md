# undelivered-work-is-noticed
# The monitor keeps a ledger of what each agent is owed, and owed work is always being delivered, worked on, recovered, or shown with the reason it waits

Second of four plans from one diagnosis (LLFourn, 2026-11-10: "What is the problem with our architecture … No refactor is too ambitious"). It follows `the-monitor-alone-decides-a-wake`; `no-process-outlives-its-build` and `reachability-is-evidence-not-inference` come after.

## Why

On 2026-11-10 a message to `artist4` (Codex, standby, `~/src/safepay/.clank/worktrees/story-short`) sat undelivered for two hours and nothing noticed. The waiter fault is the first plan's. This plan is about why no safety net caught it:

- **A repo-wide flag silenced the net.** The master's block `stork-retakes` (pending since 9 Nov 13:30) set `suppress_all`. `present_with_blocks` uses that correctly, withholding only review-cycle items. But the same fact also reaches recovery as `attending` (`recovery.rs:2569`, `:2618`, shown as `· attended`), which suppressed the stall nudge for an agent the block has nothing to do with. artist4's published state was `working · attended`, nudge `nothing`.
- **Recovery reasons from mechanism, not outcome.** It opens episodes from how the last turn ended (`Failed`, `Completed` and obligated, a silent running turn) and skips the rest (`recovery.rs`, `_ => {}`: an interrupted turn opens nothing). Each skipped case is a hole. Nothing states the outcome: this agent is owed work that isn't withheld, and it isn't getting it.
- **Clocks restart with the monitor.** artist4's `state.json` had `since` 12:23:50, five seconds after monitor pid 62625 started, although its turn had been parked since the previous afternoon. The monitor was replaced by reinstalls at about 10:50, 11:55 and 12:23, so a 45-minute silence was never reached.
- **The TUI showed a plausible lie:** `working 42m · attended`.

## Build

### 1. A ledger of owed items: obligation and delivery are separate facts

The monitor derives, per agent at each fold, every item it owes that agent, from the durable records it already reads: the event logs' `inbox`, `presentation` and `ack` records, and the review obligations with their commit and gate times. Each item carries two independent facts.

**Obligation.** `owed_for` / `present_with_blocks` stays the one authority; the ledger is its output with times attached, not a second policy.

- **Withheld:** by a named block, because it's review-cycle work.
- **Owed:** not withheld and not discharged. It carries `owed_since`, the moment it became eligible: its arrival, or its release from a block. Time spent withheld is never charged. A record with no usable time has `owed_since` unknown, shown as unknown, never replaced by the monitor's start.
- **Discharged:** acked, or its gate moved.

**Delivery, to the current binding only.** A receipt from a session the label is no longer bound to counts for nothing.

- **Not delivered.**
- **In flight:** a presentation handed to a live carrier, with no result yet. It has an expiry: the delivery result's existing deadline in turn custody (name the constant). Past it with no positive result, it is not delivered.
- **Delivered:** the durable `Presentation` record, which `github/log.rs` defines as the first successful model delivery. A backend receipt callback that is still pending doesn't undo it. A failed or missing local delivery result never establishes it.

**Recovery eligibility is the obligation, not the delivery.** Every `Owed` item keeps its agent recovery-eligible, delivered or not: a review request that reached the session whose turn then failed on provider capacity is still owed, and the existing `Failed` and `Idle` recovery must still act on it. Delivery evidence decides only what is shown (`UNDELIVERED`) and which recovery case applies.

### 2. The invariant, as one state machine

An agent with an `Owed` item is always in exactly one of these cases, decided in this order. "None of them" must be unrepresentable in the type recovery works on.

1. **Hands-off, with the reason shown.** An explicit reason clank must not nudge, checked first:
   - auto mode off;
   - the agent's own pending question to a person;
   - the agent's own pending block, or work it is attending;
   - an interrupted turn (LLFourn chose a loud display for it, not a nudge).

   Judged after cases 2 and 3, because none of them calls for a nudge while a delivery or a live turn is under way, and a harness clank can't resume (Grok) must still read as working when it is:
   - a harness clank can't resume;
   - a spent budget or a refused attempt, latched as waiting for you;
   - a nudge policy configured off (`stall_warn`, `stall_nudge`, `blind_nudge`).

   Each is a named reason and is shown; none is a silent skip, and there is no reason for a gap in the mechanism.
2. **Being delivered.** An item is in flight and unexpired. When it expires without a result, this case ends.
3. **Provably active.** The harness record shows a current turn that has not ended, with record activity inside the stall window, or the harness is retrying the turn itself (the existing native-retry ownership). A turn that just failed or completed is not active, however recent its last activity: it goes to case 4 at once, as `Failed` and `Idle` do today.
4. **Recovering.** A recovery episode is live, with a nudge scheduled or in flight.

When case 2 or 3 stops holding, the agent moves to case 4, or to case 1 with a reason. It never stays suppressed.

Recast recovery's episode opening (`recovery.rs`, where `Failed`, `Idle`, `Fallback` and `Stall` are opened) so that it starts from the ledger's `Owed` set, and the existing kinds are how case 4 is carried out. Keep the nudge owner as it is, scoped to binding, role and launch, with its settlement guards, budgets and transports. Specifically:

- **Suppression reads the ledger.** An agent is exempt from recovery only when nothing is `Owed` to it. A pending block exempts an agent only when everything it's owed is withheld by that block. The repo-wide `attending` flag stops gating recovery.
- **Clocks are durable.** Silence and age are measured from durable evidence: `owed_since`, the last delivery to the current binding, and the last activity in the harness record. A monitor restart changes none of them. Establish whether the stall episode's own clock restarts with the monitor today, and fix it if it does.

### 3. Shown

- **TUI row and `clank status`:** an agent whose oldest `Owed` item is not delivered to the current binding, and has been so past the warn threshold, reads `UNDELIVERED <age> · <what>` in bright red in its own row, for example `UNDELIVERED 2h · message from claude`. `<what>` comes from the same `owes` text the interrupted row uses. In case 1 the row also says the hands-off reason.
- **Delivered but still owed** is not `UNDELIVERED`: it reads as the recovery state it's in (failed, idle, nudged), as today.
- **`status -j`:** each agent carries its oldest owed item's age, kind and delivery state.
- **No "working" over owed work without evidence.** A row may say `working` only in case 3.

Reuse the existing stall thresholds (`warn`, `nudge`) for silence; add no new settings unless a threshold has no existing home, and say so if one is added.

## Tests

Paused clock and fakes only; no monitor, harness or pane is started.

- **The artist4 case:** an owed message, another agent's pending block, no waiter, an open silent turn. After the stall window it's nudged, and the row reads `UNDELIVERED …` red before that.
- **Withheld is not owed:** review work withheld by a block, with nothing else owed, shows no red and gets no nudge. Add a message and it does. `owed_since` for work released from a block starts at the release.
- **Delivered but still owed:** a review request presented to the session whose turn then fails on provider capacity is recovered by the existing failed-turn path, and never reads `UNDELIVERED`.
- **Delivery evidence:**
  - a failed or missing delivery result leaves the item not delivered;
  - a pending backend receipt callback doesn't undo a recorded delivery;
  - after a rebind, the old session's receipt counts for nothing and the item is not delivered to the new binding.
- **A stuck carrier:** a live waiter whose presentation never settles stops being "being delivered" at the expiry, and the agent moves to recovery.
- **A recent terminal turn is not activity:** a turn that failed or completed a second ago goes to recovery at once, not to "provably active".
- **Native retry** keeps its ownership: clank doesn't nudge while the harness retries.
- **A monitor restart doesn't reset the clock:** restart the monitor at 40 minutes of silence with a 45-minute threshold; the nudge still fires at 45.
- **Unknown times stay unknown:** an item with no usable timestamp shows its age as unknown and is not aged from the monitor's start.
- **Each hands-off reason** (auto off, own question, interrupted, a harness that can't be resumed, a spent budget) is case 1 with its reason shown, and is not nudged.
- **One owner:** a live recovery episode and the ledger never produce two nudges for one silence.
- **Exhaustiveness:** a test, or a type that makes it impossible, shows no agent with an owed item falls outside the four cases. Enumerate last-turn outcome × delivery state × waiter present or not × block present or not × each hands-off reason.
- **Mutation:** restoring the repo-wide `attending` gate fails the artist4 test. Measuring from monitor start fails the restart test. Treating in-flight as unbounded fails the stuck-carrier test. Opening recovery only for undelivered items fails the delivered-but-owed test.

## Done when

No agent can hold owed work in silence: it's shown hands-off with the reason, being delivered within a bound, active in a turn that hasn't ended, or being recovered. A block withholds only the review items it's about, and work that was delivered but is still owed keeps its recovery.

## As landed

- **Ledger** (`cli/ledger.rs`): per agent, each admitted item's `Obligation` (`Withheld{by: [agent/name]}`, `Owed{owed_since: At | Unknown{held}}`, `Discharged`) and its `Delivery` to the current binding (`NotDelivered`, `InFlight(expires)`, `Delivered`). Obligations persist in `agents/<label>/ledger.json`; delivery is read afresh.
- **One admission authority** (`wait.rs`): `admit()` gives every item a `Standing` (`Owed`, `Withheld`, `Context`); `owed_for` is its flattening.
- **Clocks from durable records:** an event's log row, a timer's due time, review work's stage handover, and a block's release from its answer file (others' blocks) or receipt marker (own block). An undatable item or release is `Unknown` and shows "age unknown"; it is never aged from the monitor's start.
- **Delivery proof:** `agents/<label>/delivered.json` names the session. It is written only under the label's `BindingGuard`, with the binding compared in the same transaction. A legacy presentation record proves nothing.
- **Delivery bound:** `turn_policy::DELIVERY_RESULT_WITHIN` (60s). `Event::ResultOverdue` fails the presentation; a late result is refused. The carrier gives up at 58s.
- **Recovery's input** (`recovery.rs`): one `Standing` per look (`HandsOff`, `BeingDelivered`, `Retrying`, `Recover(Trigger)`). Episode opening, the send veto and the published `Case` all read it. Any `Recover` without a live episode opens one; a record with no turn is idle at `AT_PROMPT`. `HandsOff` is a closed set: `AutoOff`, `Asking`, `Blocked`, `Attending`, `Interrupted`, `CannotResume`, `WaitingForYou`, `StallWarnOff`, `StallNudgeOff`, `BlindNudgeOff`.
- **Deleted:** `Facts.attending`, `Facts.delivering`, `Facts.obligated`, `Suppressor`, `Gate::Deliverer`, `Quiet::Blocked`, `suppressed()`, `Episode::owns`. A registered waiter no longer exempts an agent from recovery.
- **Behaviour changes:** a refused attempt latches as waiting for you; a retried turn that completes with work still owed goes on as idle in the same episode, on what is left of its budget; no episode opens under a hold.
- **Display:** `UNDELIVERED <age> · <what>` in bold red in the TUI row and `clank status` once the oldest undelivered owed item passes the warn window (40m when `stall_warn` is off); `status -j` carries `agents[].state.owed`. An open turn without activity evidence reads `turn open <age>`.
- **A turn clank started is watched:** a turn clank's own retry or nudge started is watched by the episode that started it. Inside the stall window the agent reads as working; past it the episode warns, nudges once on its own attempt count, and then waits for you. `Recovering` is only shown for an agent with a look due, and `next_due` is set whenever an agent is recovering or working.
- **Episode ownership:** an episode owns the turn it opened for and every turn its attempts started (`step(episode, Seen::{Mine, Stranger, Owned}, ctx)`). It ends only on a turn it does not own, on an owned turn ending with nothing owed, or on the session or obligation changing. Any other outcome of an owned turn is a transition inside the episode, so spent attempts and latches are never refunded.
- **Left open:** `record_delivered` still asks "is this session the label's" two ways (guard for receipts, `current` for events and answers). An older binary cannot read the new `state.json`.
