# tui-log-entry-cursor
# status --tui: per-entry cursor in the log timeline

Follow-up to the agents-panel work. Today the log region shows focus
only by highlighting its title rule; navigating it is pure viewport
scrolling (a single `offset`). Make the log a SELECTABLE timeline: a
cursor on the current entry, highlighted with the same full-row band
the panel/picker use, and the viewport scrolls only when the cursor
reaches the edge and you keep going.

## Why / what the user asked for

- When scrolling the log, don't just highlight the LOG title bar —
  highlight each entry (the one under the cursor), so it's clear where
  you are in the timeline.
- Scrolling should happen when you reach the bottom visible entry and
  move down (cursor-driven, viewport follows) — not free scroll.

## Model: cursor + viewport, one source of truth

Today `LogScroll` is a unit mode and `offset` (loop state) is the
viewport top. Give the log a CURSOR (the selected entry index into the
scroll `seq` — ask lines + log rows + in-progress placeholders, the
same `build_scroll` sequence render already uses), and derive the
viewport from it:
- `Down`: cursor += 1 (clamped to the last entry). If the cursor falls
  below the visible window, the viewport follows (offset += 1) — that's
  where scrolling happens.
- `Up`: cursor -= 1; if it rises above the window, the viewport
  follows. At the TOP entry (cursor 0), `Up` crosses back into the
  panel — extend the existing `log_up_target` to fire on cursor==0
  (not offset==0), keeping the panel↔log boundary pure + tested.
- Entering the log from the panel (Down past "+ add") lands the cursor
  on the FIRST entry (top), viewport at 0.
- PageUp/PageDown/g/G move the cursor by a page / to the ends, viewport
  following.

Keep the cursor↔viewport math in ONE pure function (e.g.
`scroll_to_show(cursor, offset, capacity, total) -> offset`) so "the
viewport follows the cursor" is decided in a single tested place rather
than scattered across key arms — the same discipline the panel routing
already follows.

## Render

Render highlights the entry at the cursor with `emit_selected` (the
unified band), exactly as the panel/picker highlight their selected
row — so "selected" reads identically everywhere. The LOG title rule
still marks region focus; the band marks the entry. Both cues, no
conflict.

Note: the log window currently clamps `offset` to keep the last page
full; reconcile that with cursor-driven offset so the cursor is always
within the painted window (the pure `scroll_to_show` is where this
lives). The animation-tick / spinner-visibility logic keys off the
window range — keep it correct once offset is cursor-derived.

## Acceptance

- The log shows a highlighted cursor entry (the unified selection band)
  whenever the log region is focused; the title rule still shows region
  focus.
- `Down`/`Up` move the cursor entry-by-entry; the viewport scrolls only
  when the cursor would leave the visible window.
- At the top entry, `Up` crosses to the panel ("+ add" row); entering
  the log from the panel lands on the first entry.
- PageUp/PageDown/g/G move the cursor (and viewport) as expected; the
  cursor never leaves the painted window.
- Spinner-tick visibility still tracks the visible window after the
  offset becomes cursor-derived.

## Tests (in-process; no binary spawn)

- Pure `scroll_to_show`: cursor inside the window → offset unchanged;
  cursor below → offset advances to reveal it; cursor above → offset
  retreats; last-page clamp respected.
- The boundary: `Up` at cursor 0 crosses to the panel; `Down` past the
  last entry stays put; entering from the panel starts at entry 0.
- Render: the cursor entry carries the band; moving the cursor moves
  the band; the band stays within the painted window across a scroll.

## Out of scope

- Actions on a selected log entry (e.g. Enter on a commit/review to
  open details) — this plan delivers selection + cursor-driven scroll
  only; entry actions are a later step.

---

## Addendum — drop the section highlight (post-ship feedback)

Now that the selected ITEM is highlighted (the band on the cursor
row/entry), the SECTION highlight is redundant. Remove it: `region_rule`
no longer renders the focused region as a reverse-video bar — it is
ALWAYS a thin dim labelled rule. The focused region's rule still carries
the key hint (a quiet aid, not a highlight). Focus is shown solely by
the item band, which always sits in the focused region (the panel's
selected agent/`+ add`, or the log's cursor entry). No circles, no rail,
no section bar — one cue, the item. Test updated to assert no
reverse-video region rule (`\x1b[1;7m`) in either focus state, with the
item band as the focus cue.
