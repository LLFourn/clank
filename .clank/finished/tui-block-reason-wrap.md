# tui-block-reason-wrap

In `clank status --tui`, a pending block's reason is unreadable: the
`ask` gauge shows only `first_line(&b.question)` and then `emit`
truncates that single line to the pane width. Multi-line block
messages (e.g. a question with a numbered recipe) lose everything
after line 1, and even line 1 gets cut with `…`. Since a block means
"a human must act", the reason is the one thing that must be fully
legible — word-wrap it instead of truncating.

## Current behavior

`render` (status_tui.rs, the `ask` section):
```rust
for b in snap.blocks.iter().filter(|b| b.answer.is_none()) {
    body.push(vec![label("ask"), Span(Style::Accent, first_line(&b.question))]);
}
```
`first_line` drops all but the first line; `emit` then display-width-
truncates it. Net: the human can't read why they're blocked — the
exact complaint.

## Design

- Word-wrap the FULL `b.question` across as many body lines as it
  needs, within the pane width. The block reason is the highest-
  priority detail (its own comment says detail outranks state), so
  letting it use multiple rows — even dominating a small pane — is
  correct; the existing greedy row-budget fit already caps it when
  rows run out.
- Preserve the author's explicit line breaks AND wrap long lines:
  split `question` on `\n` into segments, word-wrap each segment to
  the available width, emit all resulting lines in order. (My block
  recipes are newline-structured; flattening them would hurt.)
- Layout: first wrapped line carries the `ask` label gutter; the
  continuation lines use a blank gutter (`label("")`, as the queue
  block already does) so the text aligns in a column. Available
  width = `cols - gutter_width`.
- Accent-colored throughout (it's the action-demanding line).
- Multiple unanswered blocks: wrap each.

## The wrap helper (pure, tested)

Add `fn wrap(text: &str, width: usize) -> Vec<String>`:
- display-width aware (reuse `display_width` / `char_width` — emoji
  count as 2, matching the rest of the TUI),
- breaks on whitespace at word boundaries,
- hard-breaks a single token longer than `width` (a long URL / id
  must not overflow),
- a `width` of 0 degrades safely (don't panic / infinite-loop).
This is the testable core; the render wiring stays thin.

## Scope / non-goals

- Only the `ask`/block gauge wraps. Other gauges (plan stems, log
  subjects, git line) stay single-line truncated on purpose — that
  truncation is intentional; the block reason is the exception.
- Text `clank status` / `--watch` already prints the full block in
  its `blocks:` footer; no change there.
- No scrolling / pager — if a block reason is taller than the pane,
  the greedy fit truncates by ROW as today (acceptable; the first
  several lines are the actionable part).

## Testing

Pure render/`wrap` tests (no binary spawn):
- `wrap` matrix: plain wrap at a boundary; a word longer than width
  hard-breaks; embedded `\n` preserved; emoji width counted; width 0
  safe.
- `render` with a multi-line block question asserts (via `visible()`)
  that later lines of the message appear on their own rows (not lost)
  and that no rendered line exceeds the pane width.
- A clean snapshot (no blocks) renders no `ask` lines (unchanged).
