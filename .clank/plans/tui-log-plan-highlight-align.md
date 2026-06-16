# tui-log-plan-highlight-align

Polish the `clank status --tui` commit-log tier (`log_row_spans`,
status_tui.rs:273): make the plan umbrella headers pop and tighten
the reviewer rows so they read as an aligned table.

## Current rendering

`log_row_spans` styles three row kinds:
- `Header { plan }` → `plain(plan)` at column 0.
- `Commit { sha, subject }` → `dim("  {sha7} ")` + `plain(subject)`
  — sha starts at column 2.
- `Review { verdict, author, summary }` → `plain("    ")` (4-space
  indent) + colored mark + `dim(" {author}")` + `plain(": {summary}")`.

So review rows indent further than commits, the mark sits at column
4 (not under the sha), and summaries start at a ragged column
because author names differ in length.

## Wants

1. **Highlight plan headers with a background color.** The `Header`
   row (a plan name with its commits below) gets a background-color
   highlight so it reads as a section divider. Needs a new span
   style carrying a background SGR (e.g. `Style::Highlight` emitting
   `\x1b[48;5;…m … \x1b[0m`), since today's `Style` only does
   foreground color/dim. `emit` owns the escape (same pattern as
   `Style::Link`); width math counts only the visible text.
   `visible()` already strips CSI `…m`, so a background SGR needs no
   new stripping.

2. **Marks align with the commit sha.** Ticks/crosses (✓ ✓✓ ✗)
   start at the SAME column as the 7-char sha — i.e. column 2, the
   commit's `"  "` indent — not the current 4. This is the "indent
   it less" ask: the mark + its trailing space supply the visual
   indent before the author, so reviews no longer need the extra
   pad.

3. **Summaries start at a common column.** Right-pad the author so
   the `: {summary}` begins at the same column across all reviewer
   rows ("align the end of the reviewer names"). Because the mark
   glyph width varies (✓✓ is 2 cols, ✓/✗ are 1 — `char_width`
   counts emoji-plane as 2), align on a FIXED field: render the mark
   in a fixed-width slot, then the author in a fixed-width slot, so
   the summary column is constant regardless of mark or name length.
   Width source: scan `snap.log_rows` for the max review-author
   display width and pass it into `log_row_spans` (render already
   iterates the rows), or use a sensible fixed minimum — implementer's
   call, but the result must be exact column alignment, verified by a
   test on `visible()`.

## Testing

Pure render tests (no binary spawn) on `log_row_spans` / `render`:
- A `Header` row carries the background SGR (assert the raw escape)
  and `visible()` still shows the bare plan name.
- For a set of review rows with DIFFERENT author lengths and
  DIFFERENT mark widths (Approve ✓, Finished ✓✓, RequestChanges ✗),
  the mark column == the commit sha column, and every summary starts
  at the same `visible()` column.
- Width truncation still holds (the alignment padding goes through
  the same `emit`/`display_width` path, so a narrow pane truncates
  cleanly).

## Non-goals

- No change to the log DATA (`OnelineRow` kinds, ordering,
  newest-at-top); this is purely the span styling/layout.
- No color-config knob; reuse fixed SGR codes like the verdict
  ticks and `dirty` gauge already do.
