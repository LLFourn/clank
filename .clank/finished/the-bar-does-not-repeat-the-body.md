# the-bar-does-not-repeat-the-body
# The bar does not repeat the body

## Why

> "on the main tui page I don't think we need the top right hand status
> thing. You can see the status in the body."

Correct, and demonstrably so. The main page draws `remote_row(view.remote,
view.remote_detail)` in the body (`render.rs:632`), which gives the
remote's state AND its URL or its failure reason. The bar then reserves
the last cells of the top line for `cluster(reach, remote)`, whose remote
half is a `☁` glyph coloured by exactly the state the row below already
names in words.

So the lamp is a worse copy of a better row, and it is paid for twice: it
costs the cells, and it costs the bar its width. The reserve is
subtracted from the space the lamp text gets, and when the pane is narrow
enough the bar drops its right segment to keep it.

## The model

**An instrument earns its place in the bar by saying something the body
cannot.** The body is where state lives. The bar is for what you must see
when you are not looking at the body — and on the main page you are
looking at the body.

It also decides the one case where the lamp is NOT a duplicate. The
panel is drawn only when the roster is non-empty (`render.rs`, `if
!snap.agents.is_empty()`), while `panel_rows` always contains the remote
row — so on a teamless repo the row is navigable and invisible, and the
lamp is the only thing saying anything at all. The rule answers that
without a special case: the lamp earns its place exactly when the body
is not drawing the row, and loses it when the body is.

That distinguishes the two things in the cluster, which are not the same
kind of thing at all:

- `☁` — a **status**, duplicated in full one region below. It goes.
- `! zellij` — an **alarm**, drawn only when the session is unreachable,
  with no row anywhere that says it. It stays.

The bar's right segment (a short sha) is location, not status, and has no
duplicate row. It stays too.

## Deliverables

1. **`cluster` carries the remote lamp only when the body is not
   drawing the remote row** — that is, when the roster is empty. In the
   ordinary case the reserve drops to nothing and the bar gets its full
   width back.
2. The remote's presence in the bar is now the body row and the remote
   page, which is where it was already fullest.
3. `remote_hue` keeps its other callers; only the bar drops it.

## Tests

- With a roster, a bar built with the remote on, off, failed and
  unconfigured is identical in each case: no cell of it varies with
  remote state.
- With an EMPTY roster — where the body draws no remote row — the lamp
  is present and coloured by the state, because there it is the only
  thing that says so.
- An unreachable zellij still puts `! zellij` in the bar.
- With zellij reachable, the bar reserves nothing, so a pane one cell
  wider than the lamp text no longer truncates it.
- The body still names the remote's state and URL on the main page — the
  claim that makes the removal safe rather than a loss.
- Mutation-check each.

## Out of scope

- The bar's left segment and the lamp text.
- The zellij alarm's wording or its trigger.
