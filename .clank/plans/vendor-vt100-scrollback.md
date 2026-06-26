# vendor-vt100-scrollback

## Problem

codex — and any TUI that pins a bottom bar via a DECSTBM scroll region —
can't be scrolled in the `clank open` console, though it scrolls fine in
Ghostty and zellij.

Root cause (PROVEN by instrumentation + a unit test): the `vt100` crate
only saves a scrolled-off line to scrollback when there is NO margin at
all. `grid.rs::scroll_up` gates the scrollback push on
`!self.scroll_region_active()`, and `scroll_region_active()` is
`scroll_top != 0 || scroll_bottom != rows-1`. codex sets a region with
`scroll_bottom = rows-2` (fixed input bar on the last row), so every line
it scrolls is DISCARDED — its scrollback stays empty
(`sb=Some(0)->Some(0)` on every wheel notch, vs claude-in-main-buffer
climbing to 37). The console's existing wheel→`Scroll` path then has
nothing to scroll.

Reference: zellij (its own emulator) saves to scrollback whenever the
region TOP is row 0, regardless of a bottom margin —
`add_canonical_line`: `if scroll_region_top == 0 && alternate_screen.is_none()
{ transfer_rows_to_lines_above(1) }`. xterm/Ghostty do the same. vt100's
condition is one notch too strict.

## Fix: vendor vt100 + relax one condition

The bug is inside the external `vt100` crate (0.16.2, unmaintained — a
`vt100-ctt` fork exists). Vendor it in-repo and fix the condition.

- **Vendor** vt100 0.16.2 source under `vendor/vt100/` (its `src/`,
  `Cargo.toml`, `LICENSE`), wired via the workspace root
  `[patch.crates-io] vt100 = { path = "vendor/vt100" }`. clank's
  `vt100 = "0.16.2"` dependency line is unchanged; `use vt100::…` is
  unchanged. (Confirm `vendor/vt100` is NOT swept into the workspace
  `members` glob; `[patch]` doesn't require membership.)
- **The patch** (`vendor/vt100/src/grid.rs`, `scroll_up`):
  `if self.scrollback_len > 0 && !self.scroll_region_active()` →
  `if self.scrollback_len > 0 && self.scroll_top == 0`. A line scrolled
  off the TOP of the screen belongs in scrollback even with a bottom
  margin (codex); a region whose top is below row 0 (a contained
  mid-screen region) still doesn't accumulate. The alternate grid is
  built with `scrollback_len = 0` (screen.rs:76), so alt-screen apps
  still never accumulate scrollback — matching zellij's not-alt-screen
  gate. This is the exact `scroll_region_top == 0` rule zellij uses.
- Record the deviation: a one-line comment at the patch site
  (`// clank: scrollback when the region top is row 0 (was
  !scroll_region_active); matches xterm/zellij`) plus a short
  `vendor/vt100/VENDOR.md` noting the upstream version, the single diff,
  and why — so a future bump is trivial to re-apply, and upstreaming
  later is easy.

No console code changes: the existing `wheel_action` → `Scroll` path
already scrolls the pane's vt100 scrollback; once vt100 captures codex's
lines, codex scrolls. (Remove the temporary `CLANK_CONSOLE_DEBUG`
instrumentation — already reverted.)

## Acceptance

- Regression test (clank-side, in `console/screen.rs`): a vt100 parser
  given a DECSTBM region with a bottom margin AND `top == 0`, then
  scrolling output, accumulates scrollback (`scrollback()` climbs); a
  region with `top > 0` does NOT; the no-margin case still does. (This is
  the proof test, flipped from "drops" to "keeps".)
- Manual: wheel over codex in the console scrolls its transcript
  (lloyd verifies); claude (forwarded wheel) and main-buffer agents
  (console scrollback) are unaffected; drag-select still works over codex.
- `cargo test` green; `cargo fmt --check` + `cargo clippy` clean; the
  `git_boundary` test still passes (vendoring vt100 doesn't touch the git
  layer). Build + `cargo install` so it's live.

## Out of scope

- Upstreaming the fix to vt100 / switching to `vt100-ctt` (the VENDOR.md
  note makes either easy later).
- Any wheel-routing change (alternate-scroll arrows, etc.) — unnecessary;
  scrollback capture is the whole fix.
- Vendoring more of vt100 than 0.16.2 verbatim-plus-one-line.
