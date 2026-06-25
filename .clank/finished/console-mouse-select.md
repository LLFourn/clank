# console-mouse-select
# console mouse select — DWIM copy (like zellij)

## Problem

Copying needs `Alt-z` zoom first — not DWIM. Root cause: the console
never enables mouse reporting, so the TERMINAL does the selection, and
the terminal selects raw physical lines with no idea where the panes
are — so a drag in the agent pane grabs the status pane beside it.

Zellij "just works" because it captures the mouse itself and does
PANE-AWARE selection + puts the result on the clipboard. That's the fix
here: the console should own selection, not the terminal.

## Fix: console-owned mouse selection

Enable mouse reporting; on a drag, select within the pane under the
cursor, highlight it, and on release copy that pane's text to the
system clipboard. No zoom, no key — drag and it's copied.

### Mouse capture + the agent-mouse conflict (the crux)

- On startup emit `CSI ?1002h` (button + drag motion) + `CSI ?1006h`
  (SGR coordinates, so columns past 223 work); disable on teardown
  (`?1002l ?1006l`), alongside the existing alt-screen restore.
- Parse SGR mouse from stdin: `CSI < Cb ; Cx ; Cy (M|m)` — `M` press,
  `m` release; `Cb` low 2 bits = button, bit 5 = motion; `Cx/Cy` are
  1-based terminal coords. Pure parser → `(button, pressed, x, y)`.
- **Routing (like zellij):** if the FOCUSED pane's child has mouse
  enabled (`vt100::Screen::mouse_protocol_mode() != None`), the mouse
  belongs to the AGENT — translate the coords to that pane's origin and
  forward the (re-encoded) event to its PTY. Otherwise the mouse is the
  console's — use it for selection. Most agent TUIs don't grab the
  mouse, so selection "just works"; the ones that do still get it.
- Wheel events always forward to the focused agent (scroll), never
  select.

### Selection + clipboard

- Press in a pane sets the anchor `(pane, row, col)` (pane-relative,
  via a hit-test against the layout rects); drag extends the end;
  release finalizes. Esc / a click clears it.
- Render: the compositor marks cells inside the [anchor, end] range
  (linewise reading order) reverse-video — `region_row` already builds
  each cell, so it just flips selected cells; the per-row diff repaints
  only the changed rows.
- Copy: on release, `vt100::Screen::contents_between(r0,c0,r1,c1)` gives
  the selected text directly (no manual cell-walking); write it to the
  clipboard via **OSC 52** (`ESC ] 52 ; c ; <base64> ESC \`), which
  modern terminals route to the system clipboard. base64 is ~20 lines
  hand-rolled — no new dependency (consistent with the console's
  deps-only-where-it-hurts rule).
- Selection works in EITHER pane (agent or status) — the hit-test
  picks whichever the drag started in; both are vt100 grids.

## Keep

`Alt-z` zoom stays — it's orthogonal (full-screen an agent to read),
and a useful fallback if a terminal's mouse story is odd. The user just
shouldn't NEED it to copy.

## Out of scope

Rectangular/block selection (Alt-drag in many terminals already does
that natively); scrollback selection (the console has no scrollback).

## Order

1. SGR mouse parse + the forward-vs-select routing (pure tests:
   parse, hit-test coords→pane+cell, mouse-mode routing).
2. Selection model + highlight render + OSC 52 copy (pure base64;
   headless test that selected cells render reverse-video and
   contents_between is called with the right range).
3. Forward path for agents that grab the mouse (pane-relative
   re-encode), wheel-forward.
