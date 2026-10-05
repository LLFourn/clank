# interrupt-before-you-speak
# Interrupt before you speak

## Why

`clank nudge` submits its message and THEN interrupts. That order is only right
when the agent is mid-turn. When the agent is idle — which is exactly when a
nudge is most often needed, since the usual cause is a stop hook that failed to
park — the submitted message starts a new turn and the interrupt that follows
aborts that very turn.

Observed on 2026-10-05 in `~/src/life`: the `codex` reviewer's binding was
stale (live thread `01a1094f…`, bound `01a10ae7…`), so its Stop hook failed
identity resolution and never parked. A nudge telling it to run `clank as codex`
arrived in its transcript followed immediately by `<turn_aborted>` — the
instruction was delivered and then cancelled before the agent could act on it.
The escape hatch failed in the one situation it exists for.

The order is decided in ONE place, `pane_input::submit_then_interrupt`, and it
has two production callers:

- `nudge.rs` via `submit_and_interrupt_async`;
- `event_interrupt.rs` via `wake_for_events_async`, which sends `EVENT_WAKE`.

The second matters more than the first. An idle agent that receives an event
wake gets the wake prompt submitted and its new turn aborted, so the wake can be
lost entirely rather than merely delayed.

## What to do

Reverse the order in the shared helper: interrupt first, then submit. Interrupting
an idle agent should be a no-op; interrupting a busy one clears the way; the
message then always starts a turn that nothing cancels.

Rename it to say what it does (`interrupt_then_submit`), and sweep everything
that describes the old order: `nudge.rs`'s module doc ("submit a message, then
interrupt"), the `clank nudge --help` text, the `submit_and_interrupt_async` doc
and name, the error string "interruption was not attempted", and any README or
skill text. Sweep the whole repo for the phrasing, not just `src/`.

## Acceptance

- Both callers interrupt before they submit. Asserted through the existing
  closure seam on the helper — the order of the two injected operations is
  observable without a terminal — and through each caller, so a caller that
  bypasses the helper with its own sequence fails a test.
- The mutant is the previous body: paste the submit-then-interrupt order back
  and a test goes red.
- Error semantics are decided and stated, not inherited. Today a failed submit
  means "interruption was not attempted". After the swap the question is what a
  FAILED INTERRUPT means: on an idle agent there may be nothing to interrupt, and
  refusing to deliver the message because of that would recreate the bug in a
  new form. Pick a rule, write down why, and test it.
- An empty message is still refused before anything is sent — interrupting an
  agent to tell it nothing is the one case worse than the current bug.

## Watch out

- The interrupt must SETTLE before typing starts. Characters sent while a tool's
  cancellation UI is still tearing down can be eaten or land in the wrong place,
  and the codex path already carries a fixed 200ms sleep between its own submit
  steps. Do not add another fixed sleep as the fix; that is the arithmetic of one
  machine. Find what each tool exposes for "ready for input" and use that, or say
  plainly what is being assumed and why it is bounded.
- What Esc does on an IDLE prompt differs per tool. For some it is a no-op; for
  others it may clear a half-typed input line or open something. Check claude,
  codex, grok and opencode individually and record the answer for each — this
  plan's whole premise is that interrupting an idle agent is harmless, so it has
  to be verified per tool rather than assumed once.
- `stop_argv` / `interrupt_pane_async` already know the per-tool interrupt; reuse
  them rather than inventing a second spelling.

## Out of scope

- Fixing stale session bindings themselves. The `life` incident's root cause is
  a resume that picked up a different thread from the one clank had bound; that
  deserves its own diagnosis. This plan only makes the escape hatch work when a
  binding is wrong.
