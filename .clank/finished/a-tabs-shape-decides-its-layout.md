# a-tabs-shape-decides-its-layout

> I swapped from claude to kimi for master and it set the layout to a
> proper portrait screen layout in zellij, but then I swapped back to
> claude as master and it changed it to a non-portrait layout. What's
> going on here. Can it either stick to the same thing — fix upon the
> full terminal dimensions to determine the layout — OR just preserve
> the existing layout. Choose one. — lloyd

## What is happening

Three places decide whether a tab is landscape or portrait, and two of
them agree.

`clank open` composes from `Orientation::detect(layout_term_size())` —
the terminal's shape, measured once (`open_zellij.rs:321,398`). The
live re-layout does not: `tab_orientation` INFERS the orientation from
where the panes currently sit — the instrument pane right of the stage
reads landscape, below it reads portrait — and only falls back to the
tab's extent when it cannot find both panes.

That inference is read at the worst possible moment. A master swap
adds the new master's pane with `new-pane`, which splits whatever is
focused and puts the pane wherever zellij likes; the reconciler then
LISTS the tab, infers orientation from that half-finished arrangement,
composes a layout and applies it. The stage is "the biggest agent pane
in the tab" (`agent_pane_pairs`), and a pane that was created seconds
ago, before any layout placed it, is not reliably the one the operator
sees as the stage. Infer from it and the instrument pane can easily
sit to its right — landscape — on a tab the operator has in portrait.

So the layout is derived from the geometry the re-layout is about to
overwrite: a loop whose input is unstable exactly when it is read. It
held for kimi and not for claude because the transient differs, not
because anything about the two agents differs.

## The model

> A tab's SHAPE decides its layout — the same rule at open and at
> every re-layout after it.

Of the two lloyd offered, fix-on-dimensions is the one clank can keep
honest, and it is not a new rule: it is the rule `clank open` already
uses, applied to the case that wandered off.

- **The inference goes.** `tab_orientation` becomes
  `Orientation::detect(tab_dims(panes, tab_id))` — its own fallback,
  promoted to the whole answer — and the stage/instrument comparison,
  with it, the reason a re-layout could disagree with the open that
  made the tab.
- **The input is one the reconciler does not mutate.** `tab_dims` is
  `max(x + columns), max(y + rows)` over the tab's panes, which is the
  tab's extent however the panes inside are arranged: a swap that adds
  or closes a pane cannot change it, because panes tile the tab and a
  new one is inside it. That is what makes the answer the same before
  and after a roster change — the property the inference lacked.
- **Portrait stays portrait on a portrait screen.** The cells are ~2:1
  so `detect` calls a tab landscape only at ≥2:1 columns:rows, which
  is what lloyd's monitor is not.

### What this gives up, plainly

alt+[ / alt+] are ZELLIJ's keys cycling the `swap_tiled_layout`
variants clank ships — clank never sees them pressed. So a manual flip
still works any time, and is still forgotten by the next re-layout,
which returns the tab to the shape its size implies.

"Preserve the existing layout" is the other option, and it cannot be
done by reading the tab: reading the tab IS the bug. Preserving would
mean clank owning the toggle — its own binding, its own recorded
choice, and zellij's swap keys taken away from the operator. That is a
bigger change and a worse trade for a flip that today is not
remembered anyway. If the forgetting turns out to matter more than the
stability, THAT is the plan to write, and this one does not block it:
one function answers the question, and a remembered override would
replace its body.

## Tests

- `tab_orientation` on the same tab, before and after a pane is added
  in a position that would have read as the other orientation: one
  answer, from the extent. This is the reported failure, and the
  pane-position inference is what fails it.
- A portrait tab (80 × 137) reads portrait and a landscape one
  (200 × 50) reads landscape, whatever the instrument pane's position
  within them — including the arrangement that used to say otherwise.
- Open and re-layout agree: `Orientation::detect` over the same
  dimensions is what both call, so a tab opened portrait re-lays
  portrait.
- The existing `tab_dims` extent test stands; the
  stage-and-instrument-position test goes with the rule it pinned.

Mutation-checked with production-only edits: the comparison restored
in front of the extent.

## Out of scope

- Remembering a manual alt+[ flip (above).
- Any change to `clank open`'s own detection, which is already this
  rule and is not implicated.
- Re-laying on terminal RESIZE. The shape decides the layout at every
  re-layout; nothing here makes a resize trigger one.
