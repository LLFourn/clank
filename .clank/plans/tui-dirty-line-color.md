# tui-dirty-line-color

In `clank status --tui`, give the worktree dirty stats their own
line and color the `+N` / `−M` counts the usual GitHub way —
additions green, deletions red. Untracked count and the rest of
the `git` gauge stay dim.

## Current state

`render` (crates/cli/src/cli/status_tui.rs:159-168) folds branch,
head, and dirty into ONE `git` gauge line, every span `dim`:

```rust
let dirty = match &snap.dirty {
    Some(d) => format!(" (dirty: {})", super::status::dirty_summary(d)),
    None => String::new(),
};
body.push(vec![label("git"), dim(format!("{branch} {head}{dirty}"))]);
```

`dirty_summary` (status.rs:949) pre-joins the parts into a single
`String` (`+12 −3 · 2 untracked`). That pre-join is exactly what
blocks per-part coloring: by the time the TUI has the string the
numbers are no longer separable. The TUI already holds the raw
numbers — `snap.dirty: Option<DirtyStats>` (insertions / deletions
/ untracked) — so the colored line must be built from the struct,
not from the summary string.

The styling vocabulary already exists: `Style::Color(&'static str)`
(status_tui.rs:55) emits a fixed ANSI code (the log tier's verdict
ticks use it). GitHub colors are `"32"` (green) / `"31"` (red).

## Design

- Drop dirty from the `git` line. That line becomes just
  `label("git"), dim("{branch} {head}")`.
- When `snap.dirty` is `Some(d)`, push a SECOND gauge line with its
  own gutter — `label("dirty")` ("dirty" is exactly the 5-col
  gutter width) — built as multiple spans from the raw stats:
  - `+{insertions}` → `Style::Color("32")` (green), only when
    insertions > 0;
  - `−{deletions}` → `Style::Color("31")` (red), only when
    deletions > 0 (keep the existing `−` U+2212 glyph, not ASCII
    `-`);
  - the `· {n} untracked` part stays `dim`, only when untracked > 0;
  - mirror `dirty_summary`'s zero-omission and the all-zero
    fallback (a mode-only change reads `changes`, dim) so the new
    line never shows `+0 −0`.
  - keep a dim separator space between the +/− pair and the
    untracked part so it still reads as one instrument.
- When the tree is clean (`snap.dirty` is `None`), no second line —
  same as today.

A new line costs one row. The render is a greedy row-budget fit
(status_tui.rs:184-193) and the `git`/`dirty` gauges sit in the
mid-priority `body`, so on a tiny terminal the dirty line simply
falls off the bottom like any other low-priority gauge — no special
handling needed.

## Scope / non-goals

- TUI only. The plain `clank status` and `--watch` text paths keep
  routing through `dirty_summary` (status.rs:319) — do NOT change
  their single-line, uncolored format. `dirty_summary` stays; it is
  still the right model for the text surface.
- No per-file diffstat, no new pane — still one summary line, now
  on its own row.
- No new color config knob; reuse the fixed `"32"`/`"31"` codes the
  way the verdict ticks already do.

## Testing

In-process, no clank-binary spawning (per the standing test rule):
- The TUI renderer is already pure (`render(snap, rows, cols) ->
  Vec<String>`). Add a unit test that builds a `StatusSnapshot`
  with `dirty: Some(DirtyStats { insertions, deletions, untracked })`
  and asserts:
  - a distinct `dirty` gutter line exists, separate from `git`;
  - the rendered line carries the green code around `+N` and the
    red code around `−M` (assert the `\x1b[32m` / `\x1b[31m`
    sequences wrap the right numbers — emit at status_tui.rs:436
    is the format to match);
  - zero parts are omitted and an all-zero `DirtyStats` renders the
    dim `changes` fallback with no color codes;
  - a clean snapshot (`dirty: None`) emits no dirty line.
- Confirm the existing `git`-line test (if any) still passes with
  dirty removed from it.
