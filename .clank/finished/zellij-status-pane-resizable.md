# zellij-status-pane-resizable
# zellij status pane: resizable (and taller) in landscape

In the generated zellij layout the status pane's size is
orientation-dependent (`agent_group_kdl`, open_zellij.rs): portrait
gets `size="30%"` but landscape gets the ABSOLUTE `size=10`. Zellij
treats absolute-sized layout panes as FIXED and refuses interactive
resizes — the "pane is fixed" message. So in every wide terminal the
instrument pane is locked at 10 rows, which is also just too short.

## Fix

- Landscape's status pane becomes a PERCENTAGE (`size="30%"` of the
  side column — taller than today's 10 rows on typical terminals, and
  resizable). Portrait stays `30%`.
- Sweep the layout composition for any other absolute `size=N` on
  CONTENT panes (the 1–2-row zellij chrome bars are fixed on purpose
  and stay). The swap-layout variants and fork layouts compose
  through the same builder, so one site should cover all, verified.

## Acceptance

- The composed KDL (both orientations, including swap variants and
  the fork path) carries no absolute size on the status pane —
  pinned in the existing layout snapshot/compose tests, updated.
- `clank open zellij --print` output shows the percentage in both
  orientations.
- fmt/clippy at the 18/6 baseline; suites green.
