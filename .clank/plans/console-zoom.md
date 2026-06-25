# console-zoom
# console zoom — clean copy from the agent pane

## Problem

The terminal's native mouse selection works on whole PHYSICAL lines —
it knows nothing about the console's panes. So selecting text in the
agent pane drags in the always-on status pane beside it (landscape) and
the divider, producing garbage when you copy. There's no way for the
console to intercept the terminal's own selection (that's the terminal
app's, not the PTY's).

## Fix: a zoom toggle (Alt-z)

The standard multiplexer answer (tmux `prefix-z`): momentarily zoom the
active agent to fill the whole content area — no status pane, no
divider — so a native line selection covers only the agent and copies
clean. Toggle off to restore the split.

- **`Alt-z`** (and `Alt-a z` via the leader) toggles zoom.
- Zoomed: the active agent fills `(0,0, content_rows, cols)`; the status
  pane and divider are not drawn. The status child stays alive
  (`screen_rect` already clamps a 0-size pane's PTY to 1×1, off-screen,
  so its reader keeps draining — no block).
- The chrome bar stays (so you can see you're zoomed and how to get
  back); it shows a `[zoom]` marker, like `[follow]`.
- Toggle is transient state (`zoomed: bool`, default off).

### Why this solves it

When zoomed the agent spans the full width, so a native selection of
its rows is exactly the agent's text — nothing spills in from the side.
Copy with the terminal's own copy (Cmd-C / selection), then `Alt-z`
back. Leverages the terminal's native copy rather than reimplementing
selection.

## Implementation sketch

- `mux`: a `zoom_layout(rows, cols) -> Layout` (or a `zoomed` flag on
  `layout`) returning main = full content, divider/status = 0-size.
  Pure, unit-tested (main fills, status/divider zero, chrome_row intact).
- Loop: `zoomed: bool`; the effective `layout` is recomputed from it on
  zoom-toggle AND resize. On toggle: recompute layout, re-`set_winsize`
  + `set_size` every child to its new rect (agent → full, status → 1×1
  clamp), `frame.resize` (clear, since the whole split changed), repaint.
- `route`: `Alt-z` → `Action::ToggleZoom` (passthrough + leader).
- `render`: no special-casing needed — a 0-size status/divider already
  blits to nothing (add a `blit` guard for a 0-row/col rect so it emits
  nothing at all). Chrome gains the `[zoom]` marker.

## Out of scope

A full in-console **mouse copy-mode** (capture mouse, render a
selection, copy via OSC 52) is the heavier "proper" alternative — it'd
let you select a sub-region without zooming, but it's a much bigger
feature and risks stealing mouse events the agents use. Zoom is the
minimal solve; revisit copy-mode only if zoom proves insufficient.

## Order

1. `zoom_layout` + the `Alt-z` route + `Action::ToggleZoom` (pure,
   tested).
2. Loop wiring: toggle recomputes layout + resizes children + repaints.
3. `[zoom]` chrome marker + the `blit` 0-size guard.
