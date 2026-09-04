# a-swap-layout-describes-a-shape-not-a-roster

Adding a reviewer works: the pane is born in the reviewer stack. Then
alt+[ or alt+] — zellij's swap-layout keys, which clank ships to flip
between landscape and portrait — throws the new pane out of the stack
and drops it in the middle of the master's stage. The reviewers that
were on the roster at `clank open` stay put; only the one added later
is ejected.

## The swap layouts are a snapshot of the roster

`add_swap_variants` builds each `swap_tiled_layout` by calling
`agent_group_kdl` with the SAME reviewer list as the tab's launch
layout. So a swap variant names every reviewer pane by its exact
command, one slot each — for the roster that existed when the layout
file was written.

A swap layout is not a launch description. zellij applies it to the
panes that EXIST, and it has a rule for a tab with more panes than the
layout has slots (`TiledPaneLayout::position_panes_in_space`, zellij
0.45.0): the surplus is inserted at the layout's `children` node. Our
variants have no `children` node, so nothing is inserted; the surplus
pane finds no slot and is placed by `insert_pane` — a plain split of the
largest pane. That is the stage.

Measured against zellij 0.45.0 with a throwaway session (master + r1 +
r2 stacked + status, then `new-pane --stacked` r3 focused on r1):

```
after new-pane --stacked:  r3 x=143 y=1 cols=77 rows=40   (in the stack)
after next-swap-layout:    r3 x=0   y=30 cols=143 rows=28 (split off the master)
```

## Change

The stack slot of every swap variant becomes zellij's own construct for
"however many panes are here":

```
pane stacked=true {
    children
}
```

Master and status keep their command-bearing slots, which is what holds
them in place — zellij matches those by `invoked_with` first, then fills
the `children` slots with the rest. The parser explicitly allows a
stacked pane whose only child is `children` inside a swap layout.

The tab's own layout is unchanged: it still names each reviewer, because
it is what starts them. Only the swap variants stop knowing the roster.
`add_swap_variants` therefore no longer takes the reviewer list — the
signature says what the model is.

The swap variants carry the `children` stack even when the roster opens
with ZERO reviewers. The tab layout still omits the region (there is
nothing to launch), but the variant must have somewhere to put the
first reviewer added later.

Measured with the new shape, same session and steps: r3 stays at
`x=143 y=1 cols=77 rows=40` across landscape, portrait, and back. Also
measured: zero reviewers at open then one plain-added and one stacked
reviewer; one reviewer at open then add, swap, close, swap. Every case
keeps the reviewers in the side region and master/status in their slots.

## Tests

- The composed built-in layout's swap variants contain a stacked pane
  whose only child is a `children` node, and name NO reviewer — with a
  two-reviewer roster, neither reviewer's launch command appears inside
  any `swap_tiled_layout` block. Master and status commands do.
- With zero reviewers, the tab layout has no stack region (existing
  test, narrowed to the tab) and each swap variant still has the
  `children` stack.
- Mutation: restoring the roster snapshot in `add_swap_variants` fails
  the first test.

## Out of scope

- Live tabs opened before this change keep their old swap layouts until
  reopened; there is no zellij action to replace a tab's swap layouts.
- Which stack member is expanded after a swap (zellij's choice).
