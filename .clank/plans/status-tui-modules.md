# status-tui-modules

## Problem

`crates/cli/src/cli/status_tui.rs` is one 4697-line file mixing pure
view/state/input logic with the IO event loop and a ~2000-line test
module. It's hard to navigate and makes feature work (e.g. a log-entry
detail view) noisier than it should be. The console already
demonstrates the discipline we want: a pure, headless-tested core
(`mux`) split from the IO shell (`mod.rs`) plus a focused renderer
(`render`).

## Goal

A behavior-preserving refactor (no change to rendering, key handling, or
side effects; every existing test still passes) that does **two** things,
not one:

1. Split `status_tui.rs` into a `status_tui/` directory of self-contained
   modules following the console's pure-core / IO-shell discipline.
2. **Improve the model while the code is in hand — don't just relocate
   it.** Collapse repeated patterns into structs/enums, and give the
   free functions that currently float at module scope a home as methods
   on the type they naturally belong to. Moving 4697 lines verbatim into
   seven files would be a wasted pass; the win is that each module ends
   up with a small number of types that own their data and their rules.

Behavior stays identical; the *shape* improves.

## Proposed split

Group by responsibility (final names/boundaries can be refined during
implementation, but the pure-vs-IO separation is the requirement):

- **`mod.rs`** — the IO shell only: `run_tui` event loop, `Ev`, terminal
  setup via `super::term` (`AltScreen`, `paint`), thread wiring
  (watcher / SIGWINCH / stdin), and on-demand log fetching. Re-exports
  the items other modules in `crate::cli` use today (e.g. `render_at`,
  `bar_emoji`, `attention_state`, `master_is_active`,
  `awaited_reviewers`, `Mode`, `PanelView`, `ConfirmAction`,
  `strip_leading_emoji`) so external call sites are unchanged.
- **`text.rs`** — text/cell primitives: `Style`, `Span`, `dim/plain/
  label/tier_label/auto_mark/dirty_spans`, `emit/emit_selected/row_line/
  one_line`, `wrap`, `char_width/display_width/truncate_to`,
  `region_rule`. Pure, unit-tested.
- **`render.rs`** — the view: `render_at` + the bar/gauge/agents/log
  composition, `render_add_screen`, `render_agent_detail`,
  `log_row_spans`, spinner + `in_progress_spans`, `bar/bar_text/
  pr_bar_text`. Pure: snapshot + viewport → lines.
- **`scroll.rs`** — the scroll model: `Seg`, `build_scroll`,
  `merge_review_block`, `block_ask_spans`, `scroll_to_show`,
  `log_up_target`, `InProgress`, `in_progress_rows`. Pure.
- **`input.rs`** — the input state machine: `Key`, `parse_keys`, `Mode`,
  `PanelView`, `ConfirmAction`, `DetailAction`, `DetailNav`,
  `PanelAction`, and the pure routing/decision/apply functions
  (`agent_panel_action`, `agent_detail_nav`, `confirm_decision`,
  `detail_actions`, `relocate_detail`, `toggle_focus`, `move_selection`,
  `flip_auto`, `apply_confirm`, `apply_detail_action`). Pure.
- **`derive.rs`** — snapshot-derived domain helpers:
  `attention_state`/`AttentionState`, `master_is_active`,
  `awaited_reviewers`, `state_color`, `agent_status_emoji`, `actor_of`,
  `emoji_of`, `verb_of`, `bar_emoji`, `strip_leading_emoji`. Pure.
- **`zellij.rs`** — the zellij tab/pane integration: `zellij_current_tab`,
  `parse_current_tab_info`, `zellij_rename_tab`, `TabIndicator`,
  `zellij_list_panes`, `zellij_rename_pane`, `parse_agent_panes`,
  `PaneStatus`. (IO via `std::process::Command` for zellij is fine —
  the git/gix boundary rule does not apply to zellij.)

Each module carries its own `#[cfg(test)]` tests, moved from the current
`mod tests` / `mod log_tier_tests` blocks (split by which unit they
exercise; updated only where a call site becomes a method). Visibility
tightens to `pub(super)` / private where a symbol is only used within
`status_tui`.

## Consolidate, don't just relocate

Concrete targets found in the current file — apply where it genuinely
removes repetition or gives a floating function a home (use judgment;
don't manufacture types that don't earn their keep):

- **The event loop's loose state → one owned struct.** `run_tui` carries
  ~8 bare `let mut` (`offset`, `log_cursor`, `mode`, `picker`, `frame`,
  `needs_fill`, `log_window`, `log_complete`) whose *invariants live in
  comments* — e.g. "offset is DERIVED from log_cursor via
  `scroll_to_show` each frame", "needs_fill is set only by Key/Refresh,
  never by a tick", "picker is cleared when the picker closes". Bundle
  these into a state struct whose **methods enforce those rules** (cursor
  movement + derived offset, opening/closing the picker clearing it, the
  fill-permission flag), so the invariants are code, not prose. This is
  the highest-value consolidation.
- **Mode transitions → methods on `Mode`.** `impl Mode` already holds
  `agents_focused`/`log_focused`/`selected`; fold in the free functions
  that are really mode transitions/queries (`toggle_focus(mode,
  agents_len)` → `Mode::toggle_focus`, and the `apply_confirm` /
  `apply_detail_action` appliers where they hang naturally on `Mode` or
  the new state struct rather than on no one).
- **Span/line assembly → a small builder.** `Style`/`Span` plus
  `dim/plain/label/tier_label/auto_mark`, `emit/emit_selected/row_line`,
  and the repeated `Vec<Span>` construction in `log_row_spans` /
  `in_progress_spans` / `dirty_spans` / `bar` are the same
  push-spans-then-emit shape repeated. A `Line`/span-builder type with
  methods (push a styled span, render to a padded ANSI string with/
  without the selection band) collapses that repetition and removes the
  free `emit*`/`row_line` helpers.
- **Zellij integration → one type.** The free `zellij_current_tab` /
  `zellij_rename_tab` / `zellij_list_panes` / `zellij_rename_pane` /
  `parse_*` functions plus `TabIndicator` / `PaneStatus` are one
  subsystem; group them so the parsing helpers and the side-effecting
  ops are methods of a cohesive zellij type (or its two indicator
  structs), not module-scope free functions.
- **Snapshot-derived view helpers → cohesive, considered for methods.**
  `attention_state`, `master_is_active`, `awaited_reviewers`,
  `state_color`, `bar_emoji`, `agent_status_emoji`, `actor_of`,
  `emoji_of`, `verb_of` all derive from `&StatusSnapshot`. Keep them
  together in `derive.rs`; if a thin view/newtype lets them read as
  `snap.attention()` etc. without reaching into `status.rs`, prefer that
  — but do **not** move `StatusSnapshot` itself (it stays in
  `status.rs`).

If a candidate turns out not to pay for itself, say so in the commit
rather than forcing it — the bar is "removes a real repetition or
re-homes a genuinely floating function", not "wrap everything in a
struct".

## Invariants / acceptance

- Behavior-preserving: no change to output bytes, key semantics, or side
  effects — verified by the existing tests still passing. Internal shape
  (new structs/enums, free functions becoming methods) MAY change; the
  observable behavior MUST NOT.
- All existing tests pass, relocated next to their units; `cargo test -p
  clank --lib` green; `cargo fmt --check` and `cargo clippy
  --all-targets` clean (no new `#[allow]`s).
- External call sites in `crate::cli` (and anywhere else) compile
  unchanged — the public surface is preserved via `mod.rs` re-exports.
- The git boundary test still passes (status_tui touches no git/gix
  directly; it goes through `git_io` / `status`).

## Out of scope

- Any behavior or UX change (the log-entry detail view is a SEPARATE
  follow-up plan that builds on this structure).
- Touching `status.rs` (the data layer) beyond what's needed to keep
  imports compiling.
