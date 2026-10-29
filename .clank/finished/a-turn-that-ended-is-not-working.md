# a-turn-that-ended-is-not-working
# A turn that ended is not "working"

## What happened (2026-10-28, clank repo, reviewer `codex`)

- 23:17:59 on 27 Oct: Codex wrote its final answer (FINISHED on a29596bf). Its Stop
  hook ran and parked `clank wait`. Nothing else was written to the rollout:
  `~/.codex/sessions/2026/10/01/rollout-…-01a0f4e2….jsonl` ends at that moment,
  with no turn-complete event after the final answer.
- 08:26 on 28 Oct: the Codex app-server daemon restarted, onto release 0.165.0.
  `~/.codex/logs_2.sqlite` shows `thread/resume` for this thread at 08:26:14. The
  restart killed the parked hook and its wait. No clank wait for this repo's codex
  has existed since then (`ps`).
- 10:34: `ci-runs-on-linux` was promoted. With no wait parked, nothing delivered
  the review item.
- clank still reported `codex → working` for more than an hour.
  `recovery.rs::activity` reads the rollout's latest turn. That turn never got an
  end marker, so it reads as `Outcome::Running`, and recovery refuses to nudge
  ("its turn is running", `recovery.rs:1807`). Lloyd saw "reviewing for 1h". In
  fact the agent was idle, and nothing was watching it.

## The model that's wrong

clank decided whether a turn was running from the harness's record alone. When
clank's own parked wait died without delivering anything, clank had the one fact
that mattered (nothing is watching this agent) and ignored it.

## Change: a lost park is a fact, not an inference

(Revised 2026-10-29 at Lloyd's direction, after two review rounds. An earlier
design inferred where in each harness's record the turn stopped, and what counted
as execution after that. Every harness needed its own exceptions, so it was
dropped. That code is removed, not patched.)

- **The fact:** turn custody already sees the parked deliverer's connection end,
  and whether it delivered. When the park for the current binding ends **without
  delivering**, custody records `unwatched { since, record cursor }`. It is stored
  with custody, so a monitor restart keeps it. A park from an older binding, or
  one replaced by a newer park, records nothing.
- **The rule:** an unwatched agent that owes work, has auto on, has nothing
  marking it attending, and whose record has **not grown** past the stored cursor
  for the idle settle time is nudged through the existing idle episode (same
  settle, same budget, then a person). Any record growth after the park was lost
  clears it, because the agent is doing something. A new park clears it too. If
  the agent then goes quiet, the watchdog below covers it.
- **Same gates as the rest of recovery:** an explicit interruption or
  cancellation in the record (already there when the park is lost, or arriving
  afterwards) and a native retry in progress both suppress the nudge, through the
  record readers and gates recovery already has.
- **The cursor belongs to one record:** a replaced or truncated record (the
  reader's reset, the record identity changing) clears the unwatched
  observation, and the existing record and watchdog policy takes over from
  there. A record that can't be found or read can't prove nothing happened, so
  the existing unknown-record policy applies. The cleared observation must not
  come back after a monitor restart.
- **Shown as:** `unwatched` in the status line, not "working".
- **Nothing harness-specific:** no Stop positions, no execution evidence, no
  per-adapter mapping. It works the same for every harness whose wait clank owns.

What this deliberately doesn't do: a delivered hand-off that never starts isn't
caught quickly. It shows as whatever the record says, and the watchdog catches it
after `stall_warn` + `stall_nudge`.

## The general watchdog: running but silent

A turn the record shows as running can still be stuck: the harness is hung, or
its record has stopped being written. This **deliberately goes further** than the
research plan's fallback watchdog
(`research-agent-stalls-and-provider-capacity-recovery`, "The fallback
watchdog"). That one was notice-only. Lloyd chose a warning followed by a nudge.

- **Eligible:** the agent owes work, auto is on, its record reads the turn as running, nothing marks it attending (a pending question or block, or proven
  attendance), and there's no parked deliverer and no native retry in progress.
- **Quiet period:** identified by (binding, record cursor) when it is armed. Any
  record growth, a replaced or truncated record, a rebind, or losing eligibility
  (owed work gone, suppressed, auto off) ends that period. Growth or replacement
  starts a new period; losing eligibility suspends it until the agent is eligible
  again, when a new period is armed.
- **Warning**, `recovery.stall_warn` (default `40m`) into the period: one timeline
  row and a status-line state, for example `codex: silent 40m — nudging in 5m`.
- **Nudge**, `recovery.stall_nudge` after the warning (default `5m`). It goes
  through the existing nudge path, with the existing transport safeguards.
  Silence doesn't prove a foreground tool is hung, and the nudge's interrupt key
  will stop a tool that really is still running. That is the intended trade, and
  the warning row is the five-minute chance to stop it. Exactly one nudge per
  period. If the period carries on after the nudge, escalate to a person (status
  line).
- **Durable and claimed before sending:** the period, the warning and the send are
  stored in the existing recovery episode for that agent, so a monitor restart or
  an unrelated fold can't repeat them. The send is claimed before it is
  dispatched, as recovery's retries already are. A send whose outcome is unknown
  never authorises a second one after a restart.
- **Recheck right before acting:** before the warning and before the nudge,
  re-read record progress, eligibility and the suppressors.
- **Config:** both keys take `duration|off` in clank's duration grammar, and live
  in `recovery` beside `blind_nudge`. They're read through the existing layered
  loader, so `~/.clank/config.json` sets the default and a repo's
  `.clank/config.json` overrides it field by field. Add both to the `KeyDef`
  table. `stall_warn: off` disables the watchdog; `stall_nudge: off` warns but
  never nudges.
- Inside the monitor's existing recovery fold, reusing its record watches and
  timer. No new loop.

## Out of scope

- Detecting a harness restart directly (watching app-server logs, `thread/resume`).
  The death of the hook is the signal; the reason it died doesn't matter.
- New sockets, polling loops or log watchers.

## Acceptance

- A test with this incident's shape: the record's latest turn has a final answer
  but no end marker, the Stop hook ran, and the wait died without delivering. The
  agent reads as unwatched, and recovery nudges it when work is pending. Build the
  fixture from the mechanism that failed, not a hand-set flag.
- Custody traces through the real deliverer registration and close:
  - a park lost without delivery, then no growth: nudged once the idle settle has
    passed
  - the same with growth after the loss: no nudge
  - a park that delivered: no unwatched state
  - an old park closing after a newer one registered: nothing changes
  - a monitor restart after the loss: still unwatched, still nudged
  - an interruption already present at the loss, and one arriving afterwards: no
    nudge; a native retry: no nudge
  - a replacement, and a truncation in place to below the old cursor: the
    observation is cleared and doesn't return after a restart
- Mutation checks: drop the "lost without delivery" rule, and the incident test
  fails; ignore growth, and the growth test fails.
- Watchdog tests drive time and record growth through the fold, not real clocks:
  a warning at `stall_warn`; a nudge at `stall_warn + stall_nudge`; growth before
  either re-arms it; no second nudge for the same quiet period; an interrupted
  turn, an attending agent, or auto off never warns; `off` for each key behaves
  as specified; and a repo value overrides the global one, field by field.
- Watchdog durability and suppression:
  - a monitor restart between the warning and the nudge, and after the nudge,
    produces neither a duplicate warning nor a duplicate nudge
  - an indeterminate send is not repeated
  - a parked deliverer or a native retry suppresses both stages
  - a truncated or replaced record starts a new period
- Mutation checks: measure `stall_nudge` from quiet start instead of from the
  warning, and the timing test fails; ignore record growth, and the re-arm test
  fails.
- No WHAT comments.
