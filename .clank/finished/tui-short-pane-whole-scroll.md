# tui-short-pane-whole-scroll
# status TUI: whole-pane scroll when the log viewport gets squeezed

In a short pane (exactly the old fixed-10-row zellij pane, but any
short pane) the TUI's FIXED region — the who's-active signal bar,
gauge stack, agents panel, and the region rules — consumes nearly the
whole height, leaving the log a sliver. (The block ASK lines are
already part of `build_scroll`'s scrollable sequence, not the fixed
header — codex 04ec507.) Only the log scrolls (`LogView` owns the
single viewport), so as the selection descends it crawls inside a
1–2-row window under a wall of fixed chrome: the cursor "gets lost".
Reported by lloyd 2026-07-12.

## Model

One vertical scroll space instead of a pinned header over a private
log viewport, engaged only under pressure:

- **Pinned chrome is height-conditional** (codex 3d463b1): the
  who's-active signal bar always renders (its existing rows==1
  invariant). The focused log rule is pinned ONLY when a row remains
  for it beyond the bar (`rows ≥ 2`), and it YIELDS its row when
  pinning it would leave no entry row for a selection (`rows == 2`
  with a selected entry): selection visibility outranks the rule,
  the bar outranks both. Everything else in the fixed region — gauge
  rows, the agents panel and its rule — is scrollable under
  pressure.
- **The feasible minimum is derived from that height-conditional
  budget**: `min_log = min(5, rows − chrome(rows))` with
  `chrome(1) = 1` (bar only), `chrome(2) = 1` when a selection needs
  the second row (bar + entry; the rule yields) else 2, and
  `chrome(rows ≥ 3) = 2` when the log is focused (bar + rule) else
  1. Never negative by construction.
- Compute the log's viewport as today. When it is ≥ `min_log`,
  nothing changes: header pinned, log scrolls internally — tall-pane
  rendering is IDENTICAL to today's, pixel for pixel.
- When it would fall BELOW `min_log` while the log is focused, a
  whole-pane offset shifts the SCROLLABLE header rows up (gauges,
  then agents rows, off the top) exactly far enough to give the log
  `min_log` rows with the selection visible.
- Moving the selection back up (or unfocusing the log) unwinds the
  offset — MAXIMUM FEASIBLE restoration: the header is fully restored
  at cursor-at-top whenever it and one entry row coexist; with an
  over-tall header a baseline lift remains at the top, preserving the
  one guaranteed entry (selection visibility outranks the full
  header). Resize reclamps. The plan page / document overlays are
  separate screens and untouched.

## Acceptance

- Short-pane fixture (e.g. 10 rows, roster + long log): selection
  walking down keeps ≥ `min_log` rows of log visible, the gauge/
  agents rows leave the top progressively, the signal bar and focused
  log rule never leave, and the selected row is ALWAYS on screen;
  walking back to the top restores the header to the MAXIMUM FEASIBLE
  extent — fully when it coexists with one entry row, else down to
  the baseline lift that keeps that entry (the over-tall case).
  Pinned through the pure render/scroll layer (render_at / the scroll
  module), no PTY.
- BELOW-five-rows boundary fixtures (codex 04ec507, 3d463b1),
  asserting the INTENTIONAL degradation at each height: rows == 4/3 →
  bar + rule + the remaining rows of log with the selection visible;
  rows == 2 with a focused selection → bar + the selected entry row
  (the rule yields); rows == 2 unfocused → today's rendering
  unchanged: bar + whatever the greedy fit paints next (a gauge/
  header row — the rule appears only if it naturally fits; the
  pressure offset never engages unfocused); rows == 1 →
  bar only, and the selection is simply off screen (no panic, no
  overdraw — the one state where selection visibility is
  unsatisfiable, stated as such).
- Tall-pane fixture: rendering byte-identical to today (offset never
  engages).
- Unfocused short pane: header stays pinned (no surprise jumps on
  refresh).
- fmt/clippy at the 18/6 baseline; suites green.
