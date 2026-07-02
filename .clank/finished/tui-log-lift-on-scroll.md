# Lift-on-scroll: the LOG bar signals scroll state

## Problem

After `tui-panel-focus-tops-log-invariant`, a wheel burst BUMPS at the log's
top before the next notch crosses into the panel — correct, but silent: there
was no visual indication of whether the log is at its top, so the bump reads
as a mystery pause.

## Design (iterated live with lloyd)

Rejected before landing here, each installed and eyeballed:
- Text on the rule (`↑ 12 above`): noisy; lloyd doesn't read bar text.
- A dim `⋯ N above` edge row: costs a content row; also text.
- Fading the top visible rows (dim): dim already means "secondary" in this
  UI, so the fade sends mixed messages.

CHOSEN — Material app-bar "lift on scroll": the `── LOG ──` region rule is
the indicator itself. At the top it is the normal flat dim rule; the moment
entries scroll UNDER it, the same bar renders on a RAISED surface (dark-grey
`48;5;238` background across the full width, title at full brightness). No
text to read, and no reuse of a color that already carries meaning (dim =
secondary, accent = gate state, reverse = selection). The bar settling flat
doubles as the cue that the next Up/wheel notch crosses into the panel.

## Implementation

- `text.rs::region_rule_elevated(title, hint, focused, cols)` — the raised
  twin of `region_rule`: same visible content, bg fill, no dim.
- `render.rs` log section: pick elevated vs flat by `clipped > 0` (the same
  clamped offset the window uses).

## Tests

- `region_rule_elevated_is_the_same_bar_on_a_raised_surface`: identical
  visible text to the flat rule; carries the bg SGR; drops dim; the flat
  rule has neither.
- `log_rule_lifts_while_entries_are_scrolled_under_it`: render-level —
  offset 0 → flat, offset > 0 → lifted.

## Acceptance

- Scrolling the log down lifts the bar; topping out settles it flat.
- No other region's rule changes; no text/count added anywhere.
- clippy at baseline; status_tui suites green.
