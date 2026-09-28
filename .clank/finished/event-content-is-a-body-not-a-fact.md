# event-content-is-a-body-not-a-fact

## Goal

A command event's content is a body. Render it as one — its own lines preserved, each wrapped to the pane — instead of flattening it into a single fact row that is clipped at the pane edge.

## Problem

`render_event_detail` (`status_tui/render.rs:2899`) has two ways to put text on the page, and command content is going through the wrong one.

The body way is already correct and is used twice in that same function: the GitHub content (`render.rs:2980-2987`) and the standing prompt (`render.rs:3053-3057`) both keep the text's own line breaks and run each line through `super::text::wrap(line, width)` at `cols - 4`, so long text stays reachable through the page's scroll.

The fact way is `fact(&mut body, k, v)` (`render.rs:2993`), which emits exactly one line, `  {k:<9} {v}`, with no wrapping — `emit` then clips it at `cols`. It is the right shape for `event`, `age`, `number` and `actor`, which are short by construction.

Command content is routed through the fact way (`render.rs:3011-3020`), and through `clean` on the way (`render.rs:2908`), which is `sanitize_multiline(s).replace('\n', " ")`. So a command event's content is first flattened — every newline becomes a space — and then clipped at the pane width. Everything past one pane width is not merely off-screen: it is **unreachable**, because the page scrolls by line and the text was never turned into lines.

Two backends share this row. The `content` label was introduced precisely because "every other backend just has content, and calling it a title is a small lie" — the naming already recognises that this is not a fact, but the rendering still treats it as one. GitHub's `title` is genuinely one short line and belongs where it is; a command's content is arbitrary output and does not.

`url` has a smaller version of the same problem: a long URL is clipped with no way to see the rest. So do `title` and `actor`, and so will any fact long enough — `fact` is a one-line emitter and `emit` truncates, which is the right shape for a short label and the wrong one for a body. This plan fixes the body and leaves the facts alone; it does not make every value on the page reachable, and must not claim to.

## Work

- Render command content as a body region: preserve its own line breaks, wrap each line to the post-indent display width with the existing `super::text::wrap`, and let the page's scroll cover it — the same treatment the GitHub body and the standing prompt already get. Do not add wrapping to `fact`; the fix is that content was never a fact.
- Keep `sanitize_multiline` for control characters, but stop collapsing newlines for this path. Collapsing is right for the one-line facts and wrong for a body.
- Keep GitHub's `title` where it is, as one line. It is a short label and the pages differ for a reason.
- Leave `url` as a one-line fact, clipped like the other facts, and say so where it is written. The page now carries actions that LAUNCH the addresses it names — the object and its container — so a truncated display costs a reader the ability to read a URL off the screen, not the ability to reach it. Reopening this later means deciding a fact row can wrap, which is a change to every fact and not to this one.
- The page's existing never-clipped contract (`codex bd110f0`: every body line stays reachable through the scroll in any pane) must now hold for command content too, which today it does not — with the floor above making "in any pane" precise, since a glyph two columns wide cannot be shown in one column by any amount of care.

## Acceptance

- A command event whose content is several lines long renders as several lines, in its original line structure, each wrapped to the pane width, with every line reachable through the page's scroll at a narrow pane.
- Wide glyphs are measured by display width, not by scalar count, so CJK content does not lose its tail — the same property the prompt wrapping already has, asserted for this path.
- A COMMAND EVENT'S CONTENT is never clipped with no route to the rest, at or above a STATED minimum pane width, and that minimum is named in the code and tested at its boundary rather than comfortably above it. `render_at` clamps to `cols.max(1)`, so narrower panes are reachable; below the floor the page must still render without panicking or overflowing, and promises nothing about completeness. The floor is measured — the width at which the body's indent plus a glyph of display width 2 still fits — not chosen for roundness.
- Everything the page renders through the one-line fact path — GitHub's `title`, `actor`, `url`, `event`, `age`, `number` — keeps today's behaviour exactly, truncation included. This plan does not widen the no-clipping contract to them, and the acceptance above must not be read as if it did.
- A GitHub event's `title`, and the short facts `event` / `age` / `number` / `actor`, render exactly as they do today.
- An empty or whitespace-only content renders as it does today rather than as a blank region.

## Scope

Presentation only, inside `render_event_detail`. No change to the event record, the WAL, event classification, or what any backend stores. Out of scope: the timeline row rendering, which truncates deliberately and should keep doing so.
