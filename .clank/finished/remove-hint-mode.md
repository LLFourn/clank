# remove-hint-mode

## Summary

Collapse `AutoMode` from three variants (Off / Hint / Wait) to two
(Off / On). `clank auto on` always means wait-mode. The `--mode`
flag and `AutoModeArg` enum go away. Existing `Hint` values in
agent configs deserialize as `On` for backwards compatibility.

## Why

Hint mode was the conservative first step — fire only if work is
already pending, then exit. With the stop-hook loop working
(`drop-stop-hook-state`), wait mode subsumes hint entirely: the
hook fires, wfw blocks until work arrives, returns it, agent
processes it, hook fires again. There's no use case where "check
but don't wait" is better than "wait until something shows up."

Keeping both adds a config knob nobody should ever need to change
and code that won't be exercised.

## Implementation surface

### `crates/core/src/vocab.rs`

Rename `AutoMode::Wait` to `AutoMode::On`. Delete `AutoMode::Hint`.
Add a serde alias so `"hint"` and `"wait"` both deserialize as `On`:

```rust
#[derive(…, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoMode {
    Off,
    #[serde(alias = "hint", alias = "wait")]
    On,
}
```

This means existing agent configs with `"auto_mode": "hint"` or
`"auto_mode": "wait"` both round-trip through `On`. New configs
write `"on"`.

Update `AutoMode::as_str()`: `On => "on"`.

### `crates/core/src/hook_io.rs`

Update `HookOutcome::Silent` doc: remove "hint mode with no
pending work" (hint is gone).

### `crates/cli/src/cli/mod.rs`

- Delete `AutoModeArg` enum entirely.
- Remove `--mode` from `AutoOnArgs`.
- Update doc on `AutoCmd::On`: "Enable auto-mode (wait-for-work
  long-poll)."

### `crates/cli/src/cli/auto.rs`

Remove the `AutoModeArg → AutoMode` match. `clank auto on` always
writes `AutoMode::On`.

Update the `auto status` human rendering: `auto_mode: on` instead
of `auto_mode: hint|wait`.

### `crates/cli/src/cli/stop_hook.rs`

- Delete `compute_hint_outcome` and `render_wfw_suggestion`
  (hint-only helpers).
- Collapse the auto_mode match to two arms:
  `Off → Silent`, `On → compute_wait_outcome(…)`.
- `render_work_reason` stays (used by both hint and wait, but
  after this change only by wait's JSON-parsed items via
  `render_wfw_items`). If `render_work_reason` becomes dead code
  after the hint removal, delete it.

### `crates/cli/tests/stop_hook_integration.rs`

- Delete `hint_mode_no_plans_exits_silent` (hint-specific).
- Delete `hint_mode_plan_waiting_on_others_suggests_wfw`
  (hint-specific branch-2 behavior; wait mode doesn't suggest
  wfw — it IS wfw).
- Rename `hint_with_reviewable_work_emits_claude_continuation` →
  `auto_on_with_reviewable_work_emits_claude_continuation`.
  Change `turn_auto_on(repo, "hint")` to `turn_auto_on(repo)`.
- Same rename + update for codex variant.
- Update `stop_hook_active_still_fires_continuation`: use
  `turn_auto_on(repo)` (no mode arg).
- Update `turn_auto_on` helper: drop `mode` parameter, always
  `["auto", "on"]`.
- Update `turn_auto_on_codex` similarly.

### `crates/core/src/agent_config.rs`

Update test fixtures: replace `AutoMode::Hint` with
`AutoMode::On`.

### SKILL.md / README.md

Update docs: `clank auto on|off`, no `--mode` flag. Remove
mention of hint vs wait distinction.

### `/clank config` slash command

The SKILL.md `config` picker currently offers five options
including "Enable auto-mode (hint, default)" and "Enable
auto-mode (wait, blocking)". Collapse to one "Enable auto-mode"
option mapping to `clank auto on`.

## Tests

- Existing auto-on tests updated to not pass `--mode`.
- `"hint"` in a persisted agent config deserializes as `On`
  (backwards compat round-trip test).
- `"wait"` in a persisted agent config also deserializes as `On`.
- Stop-hook tests exercise the renamed `On` path.

## Acceptance criteria

- `AutoMode` has two variants: `Off` and `On`.
- `clank auto on` accepts no `--mode` flag.
- `clank auto status` prints `auto_mode: on` (not `hint` or
  `wait`).
- Existing agent configs with `"hint"` or `"wait"` deserialize
  without error.
- No references to `Hint`, `compute_hint_outcome`,
  `render_wfw_suggestion`, or `AutoModeArg` in the codebase.
