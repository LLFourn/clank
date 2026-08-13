# attending-suppresses-standing-wakes

## Problem

The claude loop wakes the agent with work it is already doing, once per
turn end, without bound.

Measured this session: one ~12 minute `cargo test` produced roughly a
dozen identical wakes, each carrying
`master: continue <plan> @ <sha> (gate_continue)` — the agent's OWN
active plan, which it was mid-way through implementing.

Nothing malfunctioned. `clank wait` answers a predicate over repo
state ("does work exist?"), the Stop hook fires at every turn end, and
`gate_continue` is a STANDING condition that stays true for the whole
hour an implementation takes. True predicate × every turn end = a wake
each time, with no memory and no ceiling.

The second-order cost is worse than the noise. Chasing progress on that
job, the agent span up ~15 redundant poll loops, one of which
(`until <cond>; do sleep 10; done`, watching a path that never existed)
was still running FIVE DAYS later. The wake loop trains exactly the
behaviour that feeds it.

## The modeling error

Claude Code's `Stop` event means "the model stopped producing output".
Clank reads it as "the agent is idle and should be given work". Those
diverge constantly: stopping to wait on a build is not idleness, but it
is indistinguishable from it at the hook boundary.

The distinction already has a name in this codebase.
`background_disposition` classifies a live background process as
`BgDisposition::NeedsWorkCheck`, and the legacy claude path answers it
correctly:

```rust
Ok(true) => HookOutcome::Silent { why: SilentReason::BusyOwnWork },
```

`stop_hook.rs:151` short-circuits past that in the asyncrewake loop —
the only loop claude runs:

```rust
BgDisposition::NeedsWorkCheck if async_loop => {
    return asyncrewake_park(&repo, &label, role).await;
}
```

Its reasoning ("the PARK is the watcher") holds only when there is no
work YET. When `clank wait` already has work, the park returns
immediately and exit-2 wakes the agent with the item it is mid-flight
on. So `BusyOwnWork` exists, is named, and is unreachable in production.

## Goal

While an agent is genuinely attending a live background task, it is not
woken with work it already knows about — and IS still woken by news.

## Approach

1. **`<agent-dir>/attending`, holding a background task id.** Sits
   beside `wait.lock` / `wait.holder` in `.clank/agents/<label>/`.
   Written by `clank attending <task-id>` (and `--clear`).

   **Ownership by LIVENESS, never by claim.** The hook validates the
   recorded id against `HookInput.background_tasks` — which already
   carries `id`, `status` and `command`. An id absent from that list is
   void: the marker is ignored and deleted.

   The agent therefore never has to remember to clear it, and CANNOT
   wedge the hook silent by forgetting. This is the same ownership
   pattern as the stdin-pipe owner sentinel and the generation file: a
   claim that survives exactly as long as the thing it points at.

   A declarative "I am busy" flag is the rejected alternative. Agents
   are unreliable narrators — the five-day orphan above is the
   evidence — and a stale busy-flag causes MISSED wakes, which is
   strictly worse than the noise being fixed.

2. **Suppress exactly one reason: `GateContinue` on the agent's OWN
   active plan.** Nothing else.

   An earlier draft said "continue / revise", which contradicted this
   plan's own news rule. `MasterNext::Revise` is emitted only for
   `WaitingOn::MasterToRevise` with `WaitingReason::AddressCommitChanges`
   (`wait.rs:1233-1239`), and that reason is defined as "REQUEST_CHANGES
   on the latest reviewable commit" (`vocab.rs:171-173`) — newly arrived
   reviewer feedback, i.e. precisely the news that MUST wake. Suppressing
   it would mean sitting through a REQUEST_CHANGES for the length of a
   build.

   `GateContinue` is the standing one: "latest reviewable commit is
   CONTINUE; master keeps working" (`vocab.rs:177-180`) — true
   continuously for the whole implementation, and information the agent
   already has.

   **Adding any other reason requires proving it is level-triggered**
   — true continuously while the agent works — rather than edge-
   triggered on arrival. `CommitPlanRevision` and `ReadyToFinalize` are
   plausible candidates and are deliberately NOT included without that
   proof. Everything else still wakes: an answered block, a verdict at
   a new sha, a queue promotion.

3. **Hint at the moment of the mistake.** When the hook is about to
   wake with a standing item AND `background_tasks` is non-empty AND
   there is no valid `attending` marker, the wake carries the hint:
   name the live task and tell the agent to record it if it is waiting
   on it. The protocol then needs no prior knowledge — it is taught
   exactly when it would have looped.

4. **Both failure modes must be safe.** Marker lost → the hook wakes
   as it does today (noisy, correct). Marker stale → id not live →
   ignored. Neither may produce permanent silence; a test pins each.

## Required tests

In-process library tests (no binary spawning):

- **Attending a live task suppresses a standing item**: marker id
  present in `background_tasks` + `gate_continue` for the agent's own
  active plan → Silent, not Continue.
- **Attending does NOT suppress news**: same marker, but the pending
  item is an answered block / a verdict at a new sha → still wakes.
- **A stale marker is void**: id absent from `background_tasks` → the
  item wakes normally and the marker is removed. Guards the
  missed-wake failure mode directly.
- **No marker + live task + standing item → wake CARRIES the hint**,
  naming the live task id.
- **No background task + marker present** → marker is void (nothing to
  attend), item wakes.

## Acceptance

- **Once a valid `attending` marker is recorded**, a long background
  job produces no further wakes for `GateContinue` on that agent's own
  plan, for as long as the marked task is live.

  Scoped deliberately. Before a marker exists the hook only emits an
  advisory hint, so an agent that ignores it is still woken on every
  Stop — the pre-marker behaviour is BEST-EFFORT and this plan does not
  claim otherwise. Enforcing at-most-once without a marker would need
  the hook to hold state about what it has already said, which is the
  latch that reintroduces lost wakes across restarts.
- No reachable state where a forgotten or stale marker silences the
  hook indefinitely.
- The hint appears exactly in the situation that used to loop.

## Out of scope

- Codex's hook ceiling (`codex-idle-is-not-a-hook-failure`, queued).
  Same family — the hook speaking when it should not — different
  mechanism.
- Any dedupe/latch on wake CONTENT. Level-triggering is what makes the
  loop restart-safe; the fix is a sharper predicate, not memory.
