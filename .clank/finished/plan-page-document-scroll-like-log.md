# plan-page-document-scroll-like-log

Make the plan page's inline document scroll the way the main-page LOG does:
you can scroll DOWN past the action buttons into the plan document and keep
scrolling it, and the document's header rule uses the log's lift-on-scroll
trick to show whether you're at the top. Unlike the log, document lines are
NOT highlighted while scrolling — a plan line is read-only prose, there is
nothing to press Enter on.

## Today

The plan page (tui-plan-page-redesign) renders the action/button blocks over
the inline document (`render_plan_detail`, render.rs), and the body already
scrolls by a signed line delta (`input.rs` ~550) with the virtual height
counting the pinned hint row (tui-plan-page-redesign scroll-clamp fix:
`total = chrome + body + 1`). What's missing is (a) a continuous options →
document scroll and (b) a top-vs-scrolled indicator on the document.

## What

1. **Continuous scroll into the document.** Scrolling down (↓ / PgDn) moves
   through the action rows and then into the document body, mirroring the
   panel→log crossing model (tui-panel-focus-tops-log-invariant). The buttons
   keep their cursor/activation; the document does NOT get a cursor.

2. **Lift-on-scroll indicator, reused verbatim.** The document's header rule
   is the FLAT `region_rule` at the top and the raised `region_rule_elevated`
   the moment body lines scroll under it — exactly what the log does at
   render.rs:480 (`if clipped > 0`). Key the flat-vs-lifted choice on the SAME
   clamped offset the document window uses, so the indicator can never drift
   from the actual scroll (the one-source rule; the log-lift review noted
   computing that clamp once and sharing it — do that here from the start).

3. **No line highlighting on the document (the key difference from the log).**
   The log reverse-video-highlights the cursor line because Enter opens an
   overlay on it. The plan document has no per-line action, so it must NOT
   render a selection/reverse-video line while scrolling. Scrolling only moves
   the viewport; there is no document cursor and no Enter target inside the
   body.

## Reuse, don't fork

- The log's scroll math (`scroll.rs`: offset/clamp/`scroll_to_show`) and the
  lift twin (`region_rule` / `region_rule_elevated`, pinned by the strip-ANSI
  content-equality test) already exist — the plan page should consume them,
  not reimplement. If a piece is log-specific, generalize it, don't copy it.
- Preserve the pinned-hint virtual-height accounting so the last document line
  stays reachable at the loop's max clamp (its regression test must still pass).

## Decisions to flag for review

- Where the options→document boundary sits: does ↓ on the last button step into
  the document (log-style crossing), or is there an explicit focus handoff?
  Lean: log-style continuous crossing, since that's the mental model the user
  asked for ("scroll past the options and down to the plan").
- Does ↑ from the top of the document return to the last button (symmetry with
  the log's up-crosses-into-panel), and does entering the document snap it to
  its own top? Lean: yes to both, matching the panel/log invariant.

## Tests (pure render/scroll layer; no binary spawn)

- Scrolling down past the last action row reveals document lines; the last line
  is reachable at the loop's max clamp (extend the existing plan-page clamp
  test).
- The document header rule is FLAT at offset 0 and ELEVATED at offset > 0,
  keyed on the same clamp as the window (mirror the log's flat/lifted render
  test).
- No document body line is ever rendered with the selection/reverse-video style
  while scrolling (assert the absence — this is the read-only-prose guarantee).
- Buttons still take their cursor + Enter; the document takes neither.

## Acceptance

- On the plan page you can scroll from the options down through the whole plan
  document; the document header lifts when scrolled and sits flat at the top.
- No document line is highlighted while scrolling; Enter/selection only apply to
  the action buttons.
- clippy/fmt/suites green; the plan-page clamp regression still passes.
