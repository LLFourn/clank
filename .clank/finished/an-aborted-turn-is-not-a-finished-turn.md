# an-aborted-turn-is-not-a-finished-turn

## Why

Pressing Esc in opencode stops the agent, and the plugin immediately
prods it to continue. The one gesture whose entire meaning is "stop"
triggers the machinery whose entire purpose is "keep going".

The plugin's work loop fires on `session.idle`
(`setup_assets/opencode_plugin.js`). It treats that event as "the turn
finished". It actually means "the session stopped being busy", which
conflates two opposite outcomes: a turn that ran to completion, and a
turn a human killed. Automatic continuation is correct for the first
and is exactly backwards for the second.

The existing staleness guard cannot catch it, and the reason matters.
It counts FIRST SIGHTINGS of user message ids:

    if (event.type?.startsWith("message.") && info?.role === "user" ...)
      activity.set(id, (activity.get(id) ?? 0) + 1)

and injects only when the count is unchanged. That guard answers "did
the session move on?", which is a different question from "did the user
stop this?". An Esc produces no new user message, so the count is
unchanged, the continuation looks fresh, and it injects.

## The invariant

An explicit human interrupt must dominate the loop. Only a turn that
ended by COMPLETING may arm a continuation; a turn that ended because
someone stopped it must not be continued, and must stay stopped until
that person does something.

## Mechanism

The abort signal is confirmed in opencode 1.18.21's shipped source, not
inferred. An interrupted turn's ASSISTANT MESSAGE carries the error:

    s.role === "assistant" && s.error?.name === "MessageAbortedError"

The TUI keys on exactly this to render " · interrupted", and its
`session.error` handler deliberately suppresses the error toast for that
name — it is the signal for "a human stopped this", not for a failure.

**Use the message, not `session.error`.** Both carry the fact, but
`session.error`'s schema is:

    { sessionID: optional, error }

with `sessionID` OPTIONAL, so it cannot be relied on to attribute an
abort to a session — the plugin's `sessionOf` would return undefined and
bail. The assistant message always carries `info.sessionID`, which
`sessionOf` already reads. It also lands in the `message.*` branch the
handler already has for counting user messages, so this is an added
condition in an existing path rather than a new subscription.

**Decide at injection time, not at idle time.** This plugin has already
been bitten by assuming ordering around `session.idle` — its own comment
records that opencode emits housekeeping AFTER the idle, which is what
broke counting events instead of ids. So the abort must be readable
whether it is observed before or after the triggering idle. The handler
already defers its staleness decision to injection time; the abort check
belongs in that same decision, not in a second one.

**Flag lifecycle.** Set on observing the abort; cleared on the next
first-sighting user message. While set, an idle injects nothing. This
reuses the `seenUserMessages` machinery rather than adding a parallel
one — the user typing is already the signal that the session moved on,
and it is the same signal that should re-arm the loop.

Discarding is safe and needs no clank-side change: clank never
auto-acks, so the work re-presents on the next genuine idle. This plan
touches the plugin only.

## Required tests

The plugin has two harnesses and both are needed — the in-process state
model in `setup.rs` (cargo-runnable) and the deterministic lifecycle
driver `tests/opencode_plugin_lifecycle.mjs` (fake client/`$`, run by
hand).

- An abort observed BEFORE the idle injects nothing.
- An abort observed AFTER the idle but before injection injects nothing.
  This is the ordering hazard the plugin has already paid for once; a
  test that only covers the tidy order would pass while the real
  sequence fails.
- A turn that completes normally still injects — the fix must not buy
  quiet by breaking the loop.
- A user message after an abort re-arms the loop.
- Two consecutive delivered turns still re-arm (the existing d7c8908
  regression stays pinned).
- A source invariant that the plugin keys on the abort fact, not on a
  heuristic such as elapsed time or empty output.
- No test spawns opencode, an agent binary, or a model.

## Out of scope

- clank's stop-hook, wait, and auto-mode. The bug is that the plugin
  asks for work at a moment it should not; what clank returns is right.
- Whether the other tools' hooks have the same defect. Claude Code and
  codex reach the loop by different paths and are not examined here; if
  Esc misbehaves there too it deserves its own plan rather than a
  guessed-at shared fix.
- Cancelling an in-flight wait on abort. The guard already releases in
  `finally`, and discarding at injection is sufficient.
