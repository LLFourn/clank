# console-scroll

## Problem

Mouse-wheel in the `clank open` console does nothing for an agent that
isn't requesting the mouse — the wheel event is swallowed (mod.rs, the
`else if wheel` arm), so you can't scroll back through an agent's output.
It should scroll the agent's content up and down, the way zellij scrolls
a pane's scrollback.

## What already works

The console enables SGR mouse reporting and — from console-mouse-select —
FORWARDS wheel events to an agent that has turned the mouse ON
(`mouse_protocol_mode() != None`); that agent scrolls its own view. This
plan covers the OTHER, common case: an agent in its MAIN buffer (e.g.
**claude code**) that does NOT grab the mouse. Its output belongs in the
console's own scrollback, and the wheel should scroll THAT.

## Design: console-owned scrollback

The console's per-agent `vt100::Parser` already IS a scrollback buffer —
it's just allocated with length 0 today (`Parser::new(rows, cols, 0)`,
screen.rs:64). Give it a real scrollback and `vt100::Screen`'s existing
`set_scrollback(rows)` / `scrollback()` / `alternate_screen()` do the
rest. The cell compositor already renders `screen()`, which reflects the
scrolled view, so the renderer needs NO change.

- **Allocate scrollback** on each agent screen's parser
  (`Parser::new(rows, cols, SCROLLBACK)`, e.g. 10_000 lines). The status
  screen (`clank status --tui`, an alt-screen view) stays at 0.
- **Wheel routing** — extend the Mouse handler's wheel arm. The decision
  is a pure function of `(grabbed_mouse, alt_screen, direction)` →
  `Forward | ScrollUp | ScrollDown | Swallow`, unit-tested in `mux`:
  - child under the cursor grabbed the mouse → **Forward** (unchanged —
    that agent scrolls itself).
  - else child's screen is NOT `alternate_screen()` → **Scroll** its
    console scrollback: wheel-up `set_scrollback(scrollback() + STEP)`,
    wheel-down `set_scrollback(scrollback().saturating_sub(STEP))`
    (vt100 clamps to the buffer; offset 0 == live). STEP ≈ 3.
  - else (alt-screen agent, no mouse) → **Swallow** — an alt-screen app
    has no scrollback to show (same as today).
- **Snap to live on input**: forwarding a keystroke to a child first
  resets that child's `set_scrollback(0)`, so typing always returns to
  the live prompt (never type blindly into scrolled-up history).
- `alternate_screen()` is the whole discriminator — claude code is
  main-buffer (scrolls), a full-screen TUI reports alt-screen (left to
  the forward path). No per-tool special-casing.

Scroll position lives in each agent's own parser, so it's independent
per agent and survives switching tabs.

## Acceptance

- Wheel-up over a main-buffer agent with history scrolls the console's
  view back; wheel-down returns toward live; clamps at both ends (top of
  buffer / live) via vt100.
- Typing into an agent snaps it back to live.
- An agent that grabbed the mouse still gets wheel FORWARDED; an
  alt-screen agent without mouse swallows wheel (no spurious scroll).
- Switching agents preserves each one's scroll position.
- Tests: the pure wheel-routing decision over
  `(grabbed_mouse, alt_screen, direction)`; a behavioral check that
  feeding a parser >screenful of lines then `set_scrollback(n)` surfaces
  the older rows (a vt100 parser fed bytes — no spawning). `cargo test
  -p clank --lib` green; fmt + clippy clean.

## Out of scope

- Anchoring the view against live output while scrolled up (vt100's
  offset-from-bottom means new output shifts the view; re-scroll to
  follow). A frozen copy-mode scrollback is a later refinement.
- A scrollbar / position indicator and keyboard scrolling (wheel only,
  per the request) — keep the console minimal.
