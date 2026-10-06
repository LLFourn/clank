# the-log-takes-the-whole-pane
# The log takes the whole pane when you scroll into it

## Why

In the status TUI, scrolling down into the LOG keeps the header regions (agents, stash,
queue, drafts) on screen. The log keeps only the rows left below them. On a long roster or
a short terminal that is a handful of lines.

The plan pages already work the way the user wants. Their document starts below the action
menu. Once you scroll down into it, it takes the whole content area, and it shrinks back
when you scroll up to the top. The log should behave the same way.

## What to build

- **Into the log, the log fills the pane.** When focus moves into the log (Down past the
  last panel row, or the log's focus key) and the cursor moves down through its entries, the
  header scrolls away. The log then uses the full content area under the pinned status bar.
- **Back up, the header returns.** Moving the cursor back to the top of the log, or leaving
  it for the panel, brings the header back, as the plan page's menu returns when its document
  scrolls back to the top.
- **One mechanism.** Reuse the existing header-lift machinery (`view.lift`, `pressure_lift`
  and `log_budget` in `status_tui/scroll.rs`) rather than adding a second layout mode.
  Behave like the plan page's document scroll, so the two pages cannot disagree.
- **Selection is preserved.** The selected log entry stays on screen through the
  expansion and the collapse. A refresh, a resize or new events do not jump the view.

## Acceptance

- With a tall header and a short pane, moving the log cursor down expands the log to the
  full content height, under the pinned bar.
- Moving back to the top of the log, or back into the panel, restores the header.
- The selected entry is visible at every step, including after a resize while expanded.
- With nothing to scroll (header plus log fit), nothing changes.

## As built

- **The cap was the first cause.** `pressure_lift` capped its target at five log rows
  (`MIN_LOG_ROWS`), so descent stopped lifting with most of the header still on screen. The
  cap is gone.
- **The lift is held viewport state, not a function of the cursor.** Deriving the lift from
  the cursor's index each frame meant a refresh that moved the cursor's index lifted the
  header with nobody scrolling (codex on 23723af). `LogView` now holds `(lift, offset)`,
  and only `LogView::settle` (navigation, resize, header changes) and `LogView::follow` (a
  refresh) move them. At the log's top, the log is composed with the header: the header lifts
  row by row as the cursor descends, until the log has the pane under the bar and its rule.
  Scrolled, the log keeps its lift, takes more of the header before it scrolls further, and
  gives the header back when it returns to its top, as a plan page's document does.
  `settle_frame` settles against exactly the capacity the render draws, and paints the
  cursor it settled.
- **The panel tops the log, and only the panel does.** `Mode::panel_tops_log` is true only
  for `AgentPanel`. Before, every mode but the log reset the viewport, so opening a commit,
  event or plan page from the log and coming Back landed at the top.
- **A refresh keeps the selected entry.** The cursor was a bare index, so an event arriving
  above it moved the selection to another entry. `scroll::HeldEntry` names the entry: a
  commit by its sha; a review or banner by itself plus the commit below it; an event by its
  page keys (any shared key, since a merge can gain a member); a notice by its text. It also
  records which occurrence it is, counted from the top. The loop holds it before the
  rebuild, including under a page opened from the log, then finds it in the new rows.
  `LogView::follow` moves the arrivals above the viewport and keeps the lift, so the frame
  does not change: same header rows, same terminal row for the selection.
- **History top-up is unchanged.** It already filled to `offset + rows` on input, resize or
  data change, which covers the full pane, and ticks stay pure repaints.
