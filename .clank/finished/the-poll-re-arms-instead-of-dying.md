# the-poll-re-arms-instead-of-dying

`poll_deadline` is terminal. When it fires the hook returns
`Silent { PollDeadline }`, which leaves the agent with no park and no
wake — permanently asleep on a repo that was merely quiet.

## This is the same failure as before, moved

The ceiling once silenced this repo's codex reviewer for 22 hours with
a review waiting. `codex-idle-is-not-a-hook-failure` responded by
making the poll expire CLEANLY just below the ceiling instead of being
killed at it. That removed the `Stop hook (failed)` banner and changed
nothing about the death: a clean expiry and a kill both end the turn
with nothing armed.

Fixing the ceiling's unit bug ([[the-hook-ceiling-is-in-the-wrong-unit]])
raises the cliff from 24 hours to 24.9 days — the largest the timer
can express — but does not remove it. `asyncRewake` is the only hook
mode that wakes on exit 2, and its timeout is always enforced (`async:
true` is exempt but has no wake), so the ceiling cannot be escaped. A
repo idle for a month still ends with an agent that never runs again.

So the deadline must stop being an ending.

## Change

On reaching its deadline the poll RE-ARMS rather than going silent: it
returns a continuation, the agent wakes, and its turn-end fires a
fresh hook that parks again under a new ceiling.

That is a heartbeat, not a nudge: once per ceiling period, so roughly
once a month per idle agent. The cost is one wasted turn; the
alternative is an agent that is gone.

The continuation must SAY it is a heartbeat. An agent woken with no
work and no explanation will invent some — the reason should state
that nothing is pending, that this is the wait re-arming, and that
ending the turn is the correct response.

## The decision has no seam to sit on yet — make one

`compute_wait_outcome` is TOOL-BLIND, and every adapter that can reach
a deadline reaches it through that one function:

- claude (asyncrewake) enters via `asyncrewake_park`, itself called
  from two dispatch sites;
- codex and opencode enter via the single `LoopPolicy::InHookWait`
  call — they SHARE both the policy variant and the call site.

So "codex re-arms, opencode does not" cannot be written where the
decision currently lives. Do not discover the tool inside
`compute_wait_outcome` — pass the re-arm choice IN, decided by the
caller that already knows which adapter it is serving. Splitting
`InHookWait` into two policy variants is the alternative; prefer
whichever leaves `compute_wait_outcome` ignorant of tools.

Whatever the shape, the constraint is that adding a fifth adapter
must force an explicit answer rather than silently inheriting one.

## Both deadline exits, not just the interesting one

`compute_wait_outcome` returns `Silent { PollDeadline }` in TWO
places, and they fail the same way:

1. the pre-check, before spawning — the deadline already passed while
   identity resolution and lease acquisition ran;
2. the `under_deadline` expiry, after the wait was armed.

A change that only re-arms the second leaves the first terminal. That
path is rarer but strictly worse: it dies having done no waiting at
all, so an agent whose setup is slow near the boundary goes to sleep
without ever having polled.

## Per-adapter, because one tool cannot take it

`codex-idle-is-not-a-hook-failure` records why the current code goes
silent on expiry: "a nudge relay would loop an opencode session
forever" (codex 8000d6e). opencode's plugin injects on `session.idle`,
so a continuation there re-idles immediately and spins. VERIFIED still
current: `opencode_plugin.js` runs its work loop on `session.idle`.

- **claude (asyncrewake)** — re-arm. Reached via `asyncrewake_park`;
  the turn-end that follows is a real event, and the next hook parks
  for another ceiling period.
- **codex (in-hook wait)** — re-arm. This is the adapter that actually
  suffered the 22-hour silence.
- **opencode** — NOT re-armed, and the plan says so rather than
  letting it look covered. Its idle loop makes a continuation unsafe,
  and it needs a different answer, not this one applied blindly.
- **grok** — `Passive`, reaches no deadline. Nothing to decide.
- **claude legacy `BackgroundArm`** — returns its hint without
  waiting in-hook, so it reaches no deadline either. Out of scope,
  named here so its absence is not read as an oversight.

## Tests

- The deadline yields a Continue, not `Silent { PollDeadline }` —
  driven through the real expiry seam (`under_deadline`), not by
  calling the branch directly.
- The PRE-CHECK deadline re-arms too: an already-passed deadline at
  entry returns a continuation, not silence.
- The reason identifies itself as a re-arm and names that no work is
  pending, so the wake cannot read as work.
- opencode's expiry is unchanged — asserted through the same seam the
  re-armed adapters use, so it is a real difference in behaviour and
  not just an untested path.
- A wake that arrives BEFORE the deadline still returns its items —
  the heartbeat must not preempt real work.
- The re-armed hook parks again: two consecutive expiries produce two
  continuations, not one continuation and then silence.

## Out of scope

- The ceiling's value, which is [[the-hook-ceiling-is-in-the-wrong-unit]].
- `SilentReason::PollDeadline` itself may become unreachable for the
  re-armed adapters; remove it only if nothing else can reach it.
