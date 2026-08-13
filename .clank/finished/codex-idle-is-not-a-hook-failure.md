# codex-idle-is-not-a-hook-failure

## Problem

Observed:

```
• Stop hook (failed)
  error: hook timed out after 86400s
```

A normal idle repo surfaces to the user as a FAILED hook. Nothing was
wrong: the codex hook long-polls in-hook, the repo had no work for 24
hours, and codex's hook runner killed the process at the ceiling clank
itself wrote (`HOOK_TIMEOUT_SECS`, `setup.rs:212`).

## This is a regression, not a new bug

`4507b99 [codex-poll-expires-cleanly]` fixed exactly this symptom:

> An unset wait_timeout meant an indefinite poll that codex's 24h
> hook-runner ceiling killed, showing a failed banner for a normal
> idle; deriving a sub-ceiling default from the shared
> HOOK_TIMEOUT_SECS makes the expiry clean and drift-proof.

`7c6249c [remove-wait-timeout]` then deleted `wait_timeout` and every
consumer. `stop_hook.rs:604-606` now states it plainly: "There is no
timeout branch any more: the poll parks until a wake, the hook-runner
ceiling, or OUR death."

So the poll once again parks until the ceiling kills it. The removal
was right about the flag — `--timeout` promised a maximum it never
enforced — but it took the clean-expiry mechanism with it and left
the ceiling kill as the only terminator.

## The modeling error

**Raising the ceiling does not fix this; it moves the cliff.** Any
finite ceiling fires eventually on a legitimately idle repo, and when
it does the user sees a failure for a non-event. The defect is that a
hook whose lifetime is bounded by the runner's ceiling is being used
to wait for an unbounded event.

**Correction (2026-08-13): claude parks too, so this is not
codex-only.** An earlier draft of this section claimed claude had
escaped the trap, quoting `stop_hook.rs`'s "CODEX-ONLY: claude's hook
never waits in-hook". That comment was STALE — written for the
pre-asyncrewake model and left behind by
`3972e60 [claude-asyncrewake-work-loop]`. In the asyncrewake loop the
hook itself IS the long-poll: `asyncrewake_park` calls the same
`compute_wait_outcome`, under the same 86400 ceiling.

What asyncRewake actually changed is the COST of parking, not its
absence: the hook is untracked (so Claude Code's task reaper cannot
kill it) and non-blocking (so the UI is free while it waits). It did
not remove the ceiling, and claude was equally exposed to being killed
at it.

Legacy claude — versions without the asyncRewake capability — still
uses the background-arm model and does not park in-hook. That branch
is unaffected by this plan.

## Goal

A codex idle never surfaces as a failed hook. The fix removes the
failure mode rather than deferring it.

## Approach

1. **Research first — does codex still need to park at all?** This
   decides which of the remaining steps is even correct, so it is not
   optional groundwork.

   Determine whether codex now offers a non-parking trigger: an
   async/rewake equivalent, a `notify` mechanism, a server-side event
   channel, or anything that lets clank be woken instead of waiting.
   Clank currently drives codex through a single command hook in
   `~/.codex/hooks.json` (`setup.rs:348`).

   If such a mechanism exists, it is the clean fix and the rest of
   this plan collapses: codex stops parking, exactly as claude did,
   and no ceiling can fire. Capability-gate it the way claude's is
   (`claude_asyncrewake_capable`, `setup.rs:1602`, a parsed version
   floor) so a machine on an older codex keeps working.

   Record the finding either way — including a negative — so the next
   attempt does not redo the search.

   **FINDING (measured 2026-08-13, codex-cli 0.147.0): async hooks
   exist in the schema, are explicitly UNIMPLEMENTED, and enabling one
   makes codex SKIP the hook. No non-parking trigger. Step 2 applies.**

   An earlier version of this section claimed the string table had zero
   occurrences of `async`. That was false — an artifact of grepping
   with a `\b` word boundary against binary strings, which are
   concatenated without delimiters, so the pattern could essentially
   never match. codex caught it. The corrected model:

   - **The `async` field is real.** `HookHandlerConfig::Command`
     carries it, and the parser also knows `asyncRewake` (it appears in
     a field list with `shell` / `timeout` / `statusMessage` / `match`,
     consistent with importing claude-format external hooks).
   - **It is unimplemented, and the failure is SILENT-ish.** The binary
     carries `skipping async hook in ` + `async hooks are not supported
     yet`. An async hook is not run — it is skipped with a diagnostic.
   - **One exception:** `running async SessionEnd hook synchronously in`
     — async `SessionEnd` hooks are downgraded to synchronous rather
     than skipped. Nothing equivalent exists for `stop`.

   **Hazard to record, not just a negative result:** setting `async` on
   clank's codex Stop hook would not make it non-blocking — it would
   stop the hook running at all, silently disabling codex's entire work
   loop for a log line. The field's presence in the schema is an
   invitation to exactly that mistake. Anything clank writes for codex
   must leave `async` unset, and a test should pin that.

   Revisit if codex implements the async family; the `asyncRewake` name
   already being parsed suggests the shape they would adopt.

   **The other two channels, checked and also negative:**

   - **`notify` is outbound.** It is a `~/.codex/config.toml` key
     sitting beside `sandbox_mode` / `permissions` / `instructions`:
     codex runs a program to say it wants attention. That is codex →
     external. Useful for telling something else that codex is idle;
     useless for waking a parked codex session, which is the direction
     this plan needs.
   - **`app-server` / `remote-control` are real but wrong-shaped.**
     0.147.0 ships an app-server daemon with a JSON-RPC control socket
     (`app-server proxy`), generated JSON Schema and TypeScript
     bindings, plus `remote-control pair` for short-lived pairing
     codes. It is a client-DRIVES-codex protocol. Clank's codex agents
     are interactive TUI sessions in zellij panes, not app-server
     clients, so adopting it means replacing the agent-launch and
     pane-ownership architecture — a different plan, not this fix.
     It is the one path that would delete the ceiling problem outright
     rather than living under it, so it is worth revisiting if agents
     ever stop being TUI sessions.

   All three channels are therefore negative for this plan's purpose.

2. **If codex must still park: expire cleanly under the ceiling.**
   Restore the property `codex-poll-expires-cleanly` had, WITHOUT
   restoring what `remove-wait-timeout` correctly deleted. The
   distinction matters and must be kept:
   - Not a user-facing `--timeout` flag, and not per-agent
     `wait_timeout` config. That flag was removed because it promised
     a maximum it did not enforce (armed only after unbounded setup;
     a 2s timeout measured at 9.5s).
   - Instead an internal deadline derived from `HOOK_TIMEOUT_SECS`, so
     the two cannot drift, that ends the poll with a SUCCESS status
     and no items — an ordinary "nothing to do" — comfortably before
     the runner's kill.

   Honesty requirement from the removal: an enforced deadline must be
   armed around the whole operation including setup, or it repeats the
   dishonesty that justified deleting the flag. If that cannot be done
   within this plan's scope, say so rather than shipping an
   approximate one.

3. **Raise the ceiling to the maximum the runner accepts** — as
   MITIGATION, explicitly labelled as such. Establish what each tool's
   hook runner actually accepts and what it defaults to when the field
   is absent; the value is written per-tool from one shared constant,
   and the tools differ. Claude's documented default is 600s for a
   `command` hook, so the constant is doing real work there and must
   not simply be dropped.

   State in the constant's doc comment that it bounds a poll that is
   expected to end on its own, so a future reader does not mistake it
   for the terminator.

   **FINDING (2026-08-13): there is no maximum to raise it to, and
   raising it is no longer the point.**

   - No documented MAXIMUM hook timeout was found for either runner.
     Claude documents defaults (600s for a `command` hook, lower for
     some events); codex's `HookMetadata` carries `timeoutSec` with no
     discoverable cap or default. So "set it to the absolute max" has
     no target value.
   - More importantly it is now moot. Once the poll expires below the
     ceiling on its own, the ceiling stops being the terminator and
     becomes a backstop: reaching it means something went wrong. A
     larger number would only move a cliff that is no longer reached.
   - It must NOT be dropped either. Absent the field each runner
     applies its own default — 600s on claude — which would cut a
     legitimate park short. The constant is doing real work; its
     doc comment now says which work.

   So `HOOK_TIMEOUT_SECS` stays at 86400 and is re-documented as a
   backstop. The user's instinct ("set it to the max") was right for
   the world where the ceiling ended the poll; it does not any more.

## Required tests

In-process library tests (no binary spawning):

- **Idle expiry is not a failure**: a codex poll that reaches its
  internal deadline yields the no-work success outcome, never a
  Diagnostic. This is the regression under test — assert on the
  outcome variant, since the user-visible symptom is the banner.
- **The deadline sits under the ceiling**: derived from
  `HOOK_TIMEOUT_SECS` with margin, asserted as a relationship rather
  than a literal so the two cannot drift apart.
- **A wake still wins**: work arriving before the deadline returns
  Continue with items, unchanged.
- **Owner death still reaps**: the stdin-pipe sentinel
  (`stop_hook.rs:607-613`) keeps working — a deadline must not become
  a second, competing terminator that masks owner death.
- If step 1 finds a non-parking mechanism, the codex path gets the
  claude-shaped coverage instead, plus a capability-gate test for the
  version floor.

## Acceptance

- A 24h idle produces no failed-hook banner.
- Whatever bounds the poll is enforced around the whole operation, or
  its limits are stated.
- The ceiling constant is documented as a backstop, not the
  terminator.
- The codex trigger research is recorded in this plan, including a
  negative result.

## Out of scope

- Removing `HOOK_TIMEOUT_SECS` entirely. It is the only bound on a
  parked poll and the per-tool runner defaults differ; dropping it
  hands each runner its own default (600s on claude), which is worse.
- Legacy claude (`LoopPolicy::BackgroundArm`, versions predating the
  asyncRewake capability gate). That branch nudges and never waits
  in-hook, so no ceiling applies to it.

## Explicitly IN scope

- **Claude's asyncrewake park.** It calls the same
  `compute_wait_outcome` under the same ceiling, so the deadline
  protects it by construction rather than by a second mechanism. The
  earlier draft excluded claude on the strength of a stale comment;
  that exclusion was wrong and is withdrawn.
