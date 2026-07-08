# trim-nudge-background-task-phrase

Drop the redundant "as a background task" from the arm-the-wait nudge core.
`run_in_background: true` already says it; the phrase is the only remaining fat
after terse-arm-wait-nudge. Pure wording trim, no behavior change.

## Change

In `nudge_reason` (crates/cli/src/cli/stop_hook.rs), the shared `CORE` const:

    - "run `clank wait` as a background task (run_in_background: true), then end your turn."
    + "run `clank wait` (run_in_background: true), then end your turn."

Both variants (idle + background-task) inherit it, so the trim lands once.

## Keep (do NOT touch)

- The trigger preambles ("Nothing is watching for clank work" / "You ended
  your turn with N background tasks still running but nothing watching …") —
  a safety-net nudge must state WHY, for a model that has drifted from the
  skill-doc work loop.
- The bare command `clank wait` — it is what `has_background_clank_wait`
  recognizes; arming it as anything else defeats the YieldArmed detection and
  double-nudges (arm-clank-wait-bare).
- `run_in_background: true` — the one detail models get wrong.
- "then end your turn" — stops the model from continuing after arming.

## Tests

- The existing `run_in_background: true` assertions
  (`nudge_states_the_count_and_never_echoes_commands`,
  `idle_nudge_keeps_the_instruction_without_a_background_preamble`) still pass.
- Grep the stop_hook tests for any assertion on the literal phrase "as a
  background task" in the CORE and update/remove it. NOTE: the background-task
  VARIANT preamble legitimately still says "background task(s)" for the COUNT
  ("N background tasks still running") — that is different text, leave it.

## Acceptance

- The idle and background-task nudges no longer contain "as a background task"
  in the instruction, but still contain `clank wait`, `run_in_background: true`,
  the trigger preamble, and "then end your turn".
- clippy/fmt/suites green.
