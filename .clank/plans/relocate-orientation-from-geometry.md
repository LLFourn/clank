# relocate-orientation-from-geometry

The "weak fix" for the promote-relocation orientation flip, on top of the
reverted-to-compose-fresh relocation (the tree-edit was dropped via `clank
purge --drop` — it broke pane reuse, and true swap-pane semantics need
zellij 0.45's `break-pane`/`move-pane --pane-id`, deferred).

## Problem

`relocate_for_promote` derives orientation from `term_size()`, which is an
`ioctl` on the CALLER's pane — the 65% stage, not the whole tab. A portrait
tab (e.g. 178×137 → portrait) reads LANDSCAPE from the stage pane alone
(178×88: `178 >= 2*88`), so promoting flips a portrait layout to landscape.
Confirmed live.

## Fix

Derive the orientation from the live `list-panes` GEOMETRY of the caller's
tab, not from `term_size`:
- Add the geometry fields `pane_x`, `pane_y`, `pane_columns`, `pane_rows`
  (`u16`, `#[serde(default)]`) to `ZellijPane` (`list-panes --json` already
  emits them).
- Pure helper `tab_dims(panes, tab_id) -> (u16, u16)` = `(max(pane_x +
  pane_columns), max(pane_y + pane_rows))` over that tab's panes — the
  tab's full extent.
- In `relocate_for_promote`: find the caller pane (`caller_pane_id()`), take
  its `tab_id`, and pass `tab_dims(&panes, tab_id)` to
  `compose_promote_layout` instead of `term_size()`. `compose_kdl`'s
  existing `Orientation::detect((cols, rows))` then sees the real
  TERMINAL/tab aspect and picks the orientation `clank open` would have —
  i.e. no flip. Fall back to `term_size()` only if the caller pane isn't
  found.

Keep `compose_promote_layout`'s `term` param (no signature change) — only
what `relocate_for_promote` passes changes.

## Scope (it's the WEAK fix — say so)

This re-detects orientation from the tab's aspect ratio (what `clank open`
does), so it's correct for the common case. It does NOT preserve a *manual*
`alt+[` orientation flip, nor a manual pane resize (compose-fresh
re-applies canonical sizes). True minimal-mutation / swap semantics that
preserve arbitrary user geometry require zellij 0.45 — out of scope, a
future plan.

## Testing (no-binary-spawning)

- Pure `tab_dims` test: a portrait fixture (stage full-width, side region
  below) yields tab dims whose `Orientation::detect` is Portrait; a
  landscape fixture yields Landscape. Assert the dims are the max extents,
  scoped to the given `tab_id` (panes in other tabs ignored).
- Live `list-panes`/`override-layout` stay untested (zellij glue).

## Acceptance

- Promoting in a portrait layout stays portrait (no flip); landscape stays
  landscape. Panes still reused (no respawn — unchanged from compose-fresh).
- Best-effort: falls back to `term_size` if geometry/caller is unavailable;
  no-op outside zellij.
