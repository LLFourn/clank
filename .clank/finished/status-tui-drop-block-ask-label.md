# status-tui-drop-block-ask-label

## Problem

In `clank status --tui`, a pending block (a human "ask") renders with a
7-column dim `ask` label gutter, and the question text is indented behind
it on every wrapped line. The label is redundant — the question is already
drawn in `accent` (the red text), which makes it obvious it's a block — and
the gutter wastes horizontal space for what is often a long, multi-line
human question. Reclaim the width: drop the `ask` label and its indent so
the question uses the full pane.

## Where

`crates/cli/src/cli/status_tui/render.rs` — `block_ask_spans` (≈line 509).
Today each wrapped question line is built as:

```rust
let gutter = display_width(&label("ask").1);     // 7 cols
let ask_width = cols.saturating_sub(gutter).max(1);
// …
lines.push(vec![label(if i == 0 { "ask" } else { "" }), accent(line)]);
```

`label(name)` is `dim("{name:>5}  ")` → a fixed 7-col gutter
(`text.rs:71`).

## Change

In `block_ask_spans` only:

- Wrap each pending block's question to the **full `cols`** width (no
  gutter subtraction).
- Emit `vec![accent(line)]` per wrapped line — **no `label(...)` span**, on
  the first line or continuations. The question starts at column 0 and runs
  full width, in the existing accent style.

That's the whole behavioral change.

## Do NOT touch

The `gutter` / `ask_width` computed at `render.rs:135-136` is **shared with
the `fix` (head-correction) line**, not the block ask (its own comment says
so). Leave the `fix` line's wrapping intact. Optional tidy (reviewer's
call): since the block ask no longer has a gutter, the `label("ask")` used
there purely as a width proxy now reads as a stale leftover — switching it
to `label("fix")` (identical 7-col width) and refreshing the comment would
remove the misleading reference. Not required for the UX fix.

## Tests

- `status_tui/scroll.rs` exercises `block_ask_spans` + `Seg::Ask` (the
  `seg_kind` "ask" classification and scroll arrangement stay — only the
  span shape changes). Update any assertion that expects the `ask` label or
  the `cols − 7` wrap width.
- `status_tui/render.rs` tests: update any case asserting the `ask` label /
  gutter in the block-ask output to the new full-width, label-less shape.
- Keep a test proving: first line has no label span and starts at the left
  edge; a question longer than `cols` wraps at full width across multiple
  `accent`-only lines; multiple pending blocks still produce one line-group
  each.

## Acceptance criteria

- Pending block questions render full-width in `accent` style with **no
  `ask` label and no leading gutter**; continuation lines are full-width
  too.
- The signal bar still shows the 🙋 block lamp (untouched); the `fix`
  line's gutter/wrapping is unchanged.
- `cargo clippy -p clank` stays at baseline; tests updated and passing.
