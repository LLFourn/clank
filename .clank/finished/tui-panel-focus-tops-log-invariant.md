# tui-panel-focus-tops-log-invariant
# Panel focus ⟹ log at top: make it an invariant

## Bug (lloyd, reproduced by reading the input path)

In `clank status --tui` you can end up with the AGENTS panel focused while
the log viewport is still mid-scroll. The TUI's continuous-column model says
this state is contradictory: the panel sits ABOVE the log (Up from the log's
first entry crosses into it; Down past the panel's last row crosses back),
so panel focus should imply the log is at its top — otherwise the screen
reads as two cursors at once.

## Mechanism (why only mouse wheel)

The TUI does no mouse parsing; the terminal translates a wheel notch into a
BURST of `↑` escape sequences, which `parse_keys` yields as several
`Key::Up`s drained in ONE event batch (`status_tui/mod.rs`, the per-key
drain loop). The crossing guard `log_up_target(cursor, agents_len)`
(`scroll.rs:178`) checks only `cursor == 0` — so within a burst the cursor
walks to 0 and the NEXT Up crosses into the panel before any paint has let
`LogView::settle` derive the offset down. Once the mode leaves `LogScroll`,
`settle(focused=false)` stops deriving offset from cursor entirely, freezing
the stale mid-scroll viewport under a focused panel.

Keyboard never hits it: one keypress per batch means a settle/paint between
each Up, so the viewport visibly tops out before the crossing fires.
`Mode::toggle_focus` (Tab/`a`, input.rs) can create the same state from ANY
scroll depth — unreported but the same violation.

## Fix — two layers

1. **Gate the crossing on the viewport, not just the cursor**:
   `log_up_target(cursor, offset, agents_len)` crosses only when
   `cursor == 0 && offset == 0`. A wheel burst then BUMPS at the top (the
   remaining Ups in the burst are swallowed; the next notch after a paint
   crosses) instead of teleporting into the panel over a half-scrolled log.

2. **Enforce the state invariant in ONE place** — not by per-transition
   discipline: every pass of the event loop (before settle/render), if the
   mode is any panel-family mode (`!matches!(mode, Mode::LogScroll)`), force
   the log to its top (`LogView::enter_first`: cursor = 0, offset = 0). A
   named function (e.g. `enforce_panel_tops_log(mode, &mut log)`) with the
   invariant in its doc, so the Up-crossing, Tab's teleport, the refresh
   rebind, and ANY FUTURE entry path cannot violate it — new code gets the
   invariant for free instead of needing to remember it.

Layer 2 alone would suffice for correctness; layer 1 is the UX half (bump at
the top rather than surprise-jumping focus mid-spin). Consequence of layer 2
worth stating: Tab into the panel from a deep scroll SNAPS the log to the
top, and Tab back lands at the top, not the old depth — that is the
invariant working as demanded, not a regression.

## Tests (pure layer, no binary spawn)

- `log_up_target`: crosses only at (0, 0); cursor 0 with offset > 0 (the
  mid-burst state) does NOT cross; existing cases updated for the new arg.
- The enforcement fn: a deep cursor/offset with a panel-family mode → both
  reset to 0; `LogScroll` → untouched.
- Burst simulation at the routing level if cheap: a sequence of Ups from
  (cursor=2, offset=2) never yields a panel mode while offset > 0.

## Files
- `crates/cli/src/cli/status_tui/scroll.rs` — `log_up_target` signature +
  doc (why the offset guard exists: wheel bursts).
- `crates/cli/src/cli/status_tui/mod.rs` — call site + the loop-top
  enforcement fn + `LogView::enter_first` reuse.

## Acceptance
- Wheel-scrolling up from deep in the log stops at the top; a further notch
  (after the top is visible) crosses into the panel.
- No reachable state renders a focused panel over a mid-scroll log — Tab
  included.
- clippy at baseline; status_tui suites green.
