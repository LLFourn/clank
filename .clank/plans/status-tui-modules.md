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

A pure, behavior-preserving refactor: split `status_tui.rs` into a
`status_tui/` directory of self-contained modules following the
console's pure-core / IO-shell discipline. **No functional change** —
identical rendering, key handling, and side effects; every existing test
still passes (moved alongside the code it covers).

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

Each module carries its own `#[cfg(test)]` tests, moved verbatim from
the current `mod tests` / `mod log_tier_tests` blocks (split by which
unit they exercise). Visibility tightens to `pub(super)` / private where
a symbol is only used within `status_tui`.

## Invariants / acceptance

- Behavior-preserving: no change to output bytes, key semantics, or side
  effects. The refactor is moves + visibility + module wiring only.
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
