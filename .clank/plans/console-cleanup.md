# console-cleanup
# console cleanup — meta keybindings + always-on status pane

Follow-up to `clank-console` (the MVP shipped: `clank open` launches a
PTY multiplexer of the roster + status, `Ctrl-a` prefix, working-agent
`●`, press-Enter respawn). Hands-on use surfaced two things to fix.

## 1. Meta-based keybindings (replace the `Ctrl-a` prefix)

`Ctrl-a` is readline's start-of-line — confirmed in practice it's the
wrong default (it doesn't *clash* badly, but it's unnatural and steals
a key agents' editors use). Move to **Meta (Alt)** chords, and drop the
two-keystroke prefix model for the common actions:

- **`Meta-a`** becomes the main/leader key (was `Ctrl-a`). Keep a
  leader for the rarer commands (quit, etc.).
- **`Meta-1` / `Meta-2` / `Meta-3` …** switch directly to that agent —
  no leader prefix first. This is the headline ergonomic win: one chord
  to jump to an agent.
- **`Meta-s`** focuses the status pane (see §2).

### The technical crux: parsing Meta in a raw terminal

Alt/Meta chords arrive as **ESC-prefixed** byte sequences: `Meta-a` is
`ESC a` (`0x1b 0x61`), `Meta-1` is `ESC 1`. The current console reads
input byte-at-a-time with a single-byte prefix (`mux::route`), which
can't model this. The redesign must distinguish, after an `ESC`
(`0x1b`):

- `ESC` then a meta-mapped char (`a`, `s`, `1`–`9`) **arriving in the
  same read** → a console Meta chord.
- `ESC [` / `ESC O` → a CSI/SS3 sequence (arrow keys, etc.) → forward
  verbatim to the active child.
- a lone `ESC` (nothing follows in the same read) → the Escape key →
  forward to the child.

The standard disambiguation is **read-granularity, not a timer**:
terminals deliver an Alt chord as one `read()` burst (`ESC` + byte
together), whereas a bare Escape arrives as `ESC` alone. So
`mux::route` should consume a *slice* and recognize a leading
`ESC <meta-char>` in one burst; keep it a pure function over `&[u8] ->
(consumed, Action)` so it stays exhaustively unit-testable (the MVP's
discipline). Caveat to verify by hand: some terminals can split a
paste/burst mid-sequence; the slice parser must forward an unrecognized
`ESC …` rather than swallow it.

This subsumes MVP behaviors: the `CLANK_CONSOLE_PREFIX` env override and
the single-byte `parse_prefix`/`route` get replaced by the meta model
(keep an override path if cheap).

## 2. Status pane always on screen (not a togglable tab)

Today the status is one of the switchable full-screen screens. Instead,
**pin `clank status --tui` as an always-visible pane**, sized by
orientation (the console owns the winsize, so this is a layout call, not
inference):

- **Landscape** (`cols >= 2*rows`-ish): status fixed on the **right**,
  the active agent fills the **left**.
- **Portrait**: status fixed on the **bottom**, the active agent fills
  the **top**.

The chrome bar stays; the active agent occupies the main region; status
occupies the side/bottom region. **`Meta-s`** focuses status (so its
own keys — scroll, the agents panel — receive input); any `Meta-digit`
returns focus to an agent.

### Technical crux: compositing two live grids

The renderer currently paints ONE active grid full-bleed. Now it paints
**two regions per frame** from two `vt100::Screen`s — the active agent
(main rect) and status (side/bottom rect) — into the composed frame.
`vt100` makes this tractable (read cells/format per region), and the
flicker-free `contents_diff`-against-an-exact-clone model extends to
per-region diffs. Each child's PTY winsize must match ITS region (the
agent gets the main rect, status gets the side rect), re-propagated on
resize/orientation-flip. Cursor goes to whichever region has focus.

This is the original `clank-console` draft's "overlay status in the
corner" goal, promoted to a first-class always-on pane.

## 3. Mouse click to switch (stretch, "if possible")

Enable SGR mouse reporting (`CSI ?1000h` + `?1006h`), parse `CSI < …
M/m` from stdin, map a click's (row,col) to a region/tab → focus it;
forward mouse events to the focused child otherwise. Keep behind the
same pure-routing discipline (a click is just another input mapped to an
Action). Defer if it bloats scope — the Meta keys are the priority.

## Out of scope / keep minimal

Per the standing steer: keep it minimal. No multi-tab/worktree, no
split of multiple agents at once (still one active agent + the pinned
status), no scrollback search. Crash handling stays as-is (✗ tab +
press-Enter respawn).

## Suggested order

1. Meta keybinding parser (slice-based `route`), `Meta-digit` direct
   switch + `Meta-a` leader — pure-function tests first.
2. Pin the status pane (orientation layout + two-grid compositing +
   per-region winsize) and `Meta-s` focus.
3. Mouse (if it stays cheap).
