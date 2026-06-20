# status-tui-scroll

Make `clank status --tui` scrollable through the full plan log, and stop
stray keypresses from corrupting the pane. Prototyped on branch
`spike/tui-scroll`; this lands the de-spiked, tested implementation.

NOTE: this plan's intro and its implementation are deliberately the SAME
commit (testing whether clank's gate handles a combined intro+impl commit
cleanly).

## Why

The TUI is read-only with no input handling: it never enters raw mode, so
arrow keys echo onto the alt-screen as `^[[B…` and corrupt it; and the
log is capped at a fixed 30-commit window with no way to reach older
history. Because the TUI owns the alternate screen, zellij's own scroll
can't help.

## Design

- **Raw input mode.** `AltScreen` puts the terminal in no-echo /
  non-canonical mode (ICANON+ECHO off, VMIN=1), restored on every exit
  path — `Drop`, the panic hook, and the SIGINT/SIGTERM handler (original
  termios held in an `AtomicPtr` so the async-signal-safe handler can
  restore it without a `static mut`). This alone fixes the echo
  corruption.
- **Event-driven input, no polling.** A dedicated stdin reader thread
  does a blocking `read` (the kernel notification) and feeds parsed keys
  into a unified `Ev` channel alongside the existing watcher/SIGWINCH
  wakes (bridged in). The loop drains one channel.
- **Scroll viewport.** `render_at(snap, rows, cols, offset) -> (lines,
  capacity)` windows the log tier by `offset` and reports the viewport
  capacity (log rows that fit), so the loop can clamp the offset and know
  when to page.
- **One load rule.** At the loop top, keep at least `offset + capacity`
  log rows loaded — this fills a tall pane on first paint AND pages in
  older rows as you scroll. Growth is a screenful of commits per rebuild
  (re-fetch a bigger window via `status::tui_log_rows` — the fold replays
  from a base, so this is re-fetch-bigger, not incremental), latching at
  the repo root.
- **Cost model.** Keys only move the offset and repaint from the cached
  snapshot (instant); only crossing the loaded bottom pays a rebuild.
  Snapshot rebuild + zellij pane/tab queries happen only on `Refresh`,
  never per keystroke.
- **Keys:** ↑/k, ↓/j, PgUp/b, PgDn/Space, g = newest (top), G =
  bottom-of-loaded, q = quit. On new activity the snapshot rebuilds and
  the scroll offset is preserved (offset 0 keeps showing newest, since
  rows are newest-first).

## Tests (no-binary-spawning)

- `parse_keys`: CSI arrows + PgUp/PgDn and vi-style j/k/g/G/space/q;
  unmapped bytes ignored.
- `render_at`: offset windows the log (newest at 0, older as offset
  grows) and reports a sane viewport capacity.
- The git-backed grow loop stays untested, like the other zellij/git
  glue in this module.

## Acceptance

- Stray keys no longer echo/corrupt the pane.
- The pane fills on first paint at any height; scrolling pages through the
  full plan history to the root, then stops.
- Scrolling is smooth (cached repaint); no rebuild or zellij query per
  keystroke.
- Live updates still work; offset preserved across refreshes.
- No polling; terminal restored on normal exit, panic, and SIGINT/SIGTERM.
- cli + core tests green; clippy within budget (cli ≤30); fmt clean.
