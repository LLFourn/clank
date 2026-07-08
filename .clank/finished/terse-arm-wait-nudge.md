# terse-arm-wait-nudge

The stop hook's arm-the-wait nudge re-teaches the whole HOW every time
it fires:

> Nothing is watching for clank work. Start `clank wait` as its OWN
> background task now — call the Bash tool with command `clank wait`
> and run_in_background set to true — then end your turn. It will wake
> you the moment there is clank work for you.

That wording predates the skill docs carrying the work loop
(claude-stop-hook-minimal-hint taught arm/act/re-arm in
`WORK_LOOP_CLAUDE`). The nudge is now the safety net, not the teacher —
apply the minimal-hint rule to it: state the trigger, lean on the skill
for the rest.

## Change

`nudge_reason` in `crates/cli/src/cli/stop_hook.rs`, both variants:

- idle: "Nothing is watching for clank work — run `clank wait` as a
  background task (run_in_background: true), then end your turn."
- live background task(s): "You ended your turn with {a background
  task,N background tasks} still running but nothing watching for
  clank work — also run `clank wait` as a background task
  (run_in_background: true), then end your turn."

Keep: the count-only preamble (dark-skippy: never echo command lines),
the `run_in_background: true` mention (the one detail models get
wrong), and the single shared instruction core so the two variants
can't drift. Drop: "call the Bash tool with command", "its OWN", the
wake-motivation tails.

Update the wording asserts in `nudge_states_the_count_and_never_echoes_commands`
and `idle_nudge_keeps_the_instruction_without_a_background_preamble`.

## Risk

The verbose wording was spike-validated for compliance. If dogfooding
shows sessions ignoring the terse form (stalled reviewers, repeated
nudges in stop-hook.json traces), restore specificity — the decision
trace's `continue_kind: arm_wait_nudge` makes non-compliance visible.

## Acceptance

- Both nudge variants ≤ ~2 lines in a terminal; no "Bash tool"
  phrasing; still name `clank wait` + run_in_background + end turn.
- No command text ever echoed (existing dark-skippy asserts stay).
