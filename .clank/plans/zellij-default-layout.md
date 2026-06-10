# zellij-default-layout — master stage, stacked reviewers, built-in --tui pane, orientation-aware

Redesign the layout `clank open zellij` GENERATES (lloyd
2026-06-10). Today every agent pane sits in one equal
`split_direction="horizontal"` row. Instead:

1. **Master gets a big main pane** (the stage — most of the
   screen; it's where the work happens).
2. **Reviewers stack in a smaller region** (zellij
   `stacked=true` pane group — one reviewer visible, the rest as
   collapsed stack bars; N reviewers don't shrink each other).
3. **The reviewer region splits to fit a small
   `clank status --tui` pane** next to/below the reviewer stack —
   the instrument panel ships in the default layout, not just as
   the documented template example.

## Orientation: detected default + both variants on alt+[

The arrangement axis depends on the terminal's shape:

- **Landscape** (wide): master left (~65%), right column =
  reviewer stack on top + short --tui pane at the bottom.
- **Portrait** (tall): master top (~65%), bottom row = reviewer
  stack left + narrow --tui pane right.

BOTH variants ship in the generated KDL as zellij
`swap_tiled_layout` blocks so the user flips between them with
the built-in swap keys (alt+[ / alt+]). The DEFAULT (the base
layout) is chosen at generation time by detecting the spawning
terminal's dimensions from Rust.

## Grounded notes (verified)

- **Terminal dimensions from Rust: already in-tree.**
  `status_tui::term_size()` does the `TIOCGWINSZ` ioctl via libc
  (per-platform constants, 24x80 fallback). Reuse it — possibly
  hoisted to a shared module — at `open zellij` spawn time:
  cols/rows of the SPAWNING terminal pick landscape vs portrait
  (compare cols vs rows scaled by the ~0.5 cell aspect ratio —
  a "square" terminal is ~2:1 cols:rows; pin the exact rule +
  fallback default at sizing).
- **Generation surface**: `open_zellij.rs` `BUILT_IN_TEMPLATE` +
  `agent_group_kdl` (the `clank_agents` substitution payload from
  zellij-layout-config-around-agent-panes). The agent GROUP is
  what gets restructured (stage + stack + tui); the built-in
  template gains the swap_tiled_layout variants.
- **User templates** (`~/.clank/config.json#/zellij/layout`):
  unchanged contract — the marker still receives the agent group.
  Decide at sizing whether the group substituted into USER
  templates is the new stage/stack/tui arrangement (probably yes
  — it's "the agent panes" and the user controls everything
  around it) and whether swap variants are built-in-only (lean
  yes: swap blocks are layout-root constructs; splicing them into
  arbitrary user templates is fragile — document that template
  authors write their own swaps).
- **Compose stays pure + testable**: thread `(cols, rows)` into
  the compose path as a parameter (ioctl only at the run()
  shell), so in-process tests pin both orientations' KDL —
  parse-validity (kdl 4), master-pane size, stacked reviewer
  group, the --tui pane command, and which variant is the base
  for given dims. No binary spawning.

## Open questions to pin at sizing

- zellij minimums: stacked panes need a minimum height per stack
  bar; the --tui pane wants ~3-10 rows (it degrades gracefully —
  the 1-row bar invariant — so small is fine). Pick sizes in
  percent vs fixed rows.
- Master % (65? 70?) per orientation.
- Does `swap_tiled_layout` interact sanely with
  `default_tab_template` + runtime tabs? (The current built-in
  uses default_tab_template for the bars.) Verify with a real
  zellij; pin what works.
- Zero reviewers (master-only team): stage-only layout + --tui
  pane; no empty stack region.

## Out of scope

- The zellij-focus-follower (RELEASE-CHECKLIST item) — active
  pane steering is a separate feature; this is static layout.
- Changing the user-template marker contract.

## Status

Stub — queued lloyd 2026-06-10.

## Promote-time notes (2026-06-10)

- Donors verified: `status_tui::term_size()` (TIOCGWINSZ via
  libc, 24x80 fallback) and `open_zellij::BUILT_IN_TEMPLATE` /
  `agent_group_kdl` exist as described. Compose is already pure
  over its inputs (the zellij-layout-config tests pin it), so
  threading `(cols, rows)` keeps the no-binary-spawning test
  discipline.
- The swap_tiled_layout × default_tab_template interaction is
  the one genuinely empirical question — pin it by generating
  with `--print`, hand-spawning, and flipping alt+[ in a real
  session during implementation (lloyd can eyeball; the
  generated KDL itself stays test-pinned).
- Sizing leans (implementer to confirm): master 65% both
  orientations; --tui pane fixed rows (size=8) in landscape's
  right column, fixed cols (~30) in portrait's bottom row —
  the TUI degrades gracefully at any size (1-row bar invariant),
  so err small. Orientation rule: landscape iff cols >= 2*rows
  (the ~0.5 cell aspect makes 2:1 cols:rows roughly square;
  wider → landscape), fallback landscape when the ioctl fails.
