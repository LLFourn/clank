# a-plan-name-is-a-breaker
# A plan's name is a breaker across the log

## Why

> "for the clank status --tui output could you center the plan name and
> put some horizontal lines around it to the edge of the screen so it
> looks like a breaker."

The row that closes a plan's run in the TUI log
(the-plan-name-sits-under-its-commits) is the plan name alone, flush
left in the highlight style (`log_row_spans`, `OnelineRow::Header`). It
separates one plan's commits from the next, but it reads like one more
row; a divider should span the pane.

## The model

**The header row is a rule with the plan name in its middle.**
`──────── the-plan-name ────────`: the name keeps the highlight it has
today, with one space either side, and dim `─` fills to both edges of
the pane — the same dim rule the region headings use
(`region_rule`). The name is centred; with an odd remainder the extra
column goes to the right. Only the pane knows its width, so the row is
laid out where the log region is (`render.rs`, the loop that calls
`row_line` with `cols`), not in the width-free `log_row_spans`.

A pane too narrow for the name keeps at least one `─` on each side and
truncates the name with `…`, the way every other cut in the log is
marked.

## Deliverables

1. **The breaker.** A header row renders as dim rule, highlighted name,
   dim rule, exactly the pane's width, name centred. `adhoc` (a header
   with no plan) is drawn the same way.
2. **Narrow panes** truncate the name, never the rules on either side.
3. **Nothing else moves**: commit, review, GitHub and notice rows, the
   age gutter, and the selection band on a selected row render as
   they do today.

## Tests

- At 40 and 41 columns a header row is exactly the pane's width, starts
  and ends with `─`, and its name sits centred — the left fill equals
  the right, or is one less.
- A name longer than the pane keeps a `─` at each edge and ends in `…`.
- A commit row's rendering is unchanged beside it.

Run `cargo test -p clank --lib cli::status_tui`, then the whole
`-p clank --lib` suite.

## Out of scope

- `clank log --oneline` and the web ledger, which show the same row in
  their own forms.
