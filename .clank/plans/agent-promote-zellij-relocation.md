# agent-promote-zellij-relocation

Follow-up to `agent-promote-rename-zellij`. When `clank agent promote
<name>` runs in a zellij session, relocate panes to follow the role
change: the promoted agent's pane becomes the master/STAGE pane and the
demoted old master's pane joins the reviewer STACK. Config role change is
already implemented (`set_repo_master`); this projects it onto the live
zellij layout, best-effort.

## Spike result (decided 2026-06-22, human-driven)

A clean stack↔stage swap IS feasible in zellij 0.44.3 with **no process
restart** — but NOT via a direct pane swap:
- There is no "swap pane A↔B by id" verb. `move-pane <dir>` swaps with a
  directional neighbor, but a **stack intercepts** the move (intra-stack
  rotation via `progress_stack_up_if_in_stack`), so a stacked reviewer
  won't swap into the stage. `break-pane` (pop out of a stack) is
  **0.45-only**, not in our installed 0.44.3. `stack-panes` works but is
  fragile/geometry-dependent (a wrong invocation collapsed all panes into
  one column).
- **`override-layout` is the chosen primitive.** It re-flows the EXISTING
  panes into a freshly-composed target layout. Verified live: pane ids
  preserved (`0,1,2,3,8`), status-pane **pid unchanged with continuous
  elapsed time** across an override → panes are reused, not respawned.

## Mechanism

After `set_repo_master` flips the config, best-effort project onto zellij:
1. `$ZELLIJ`-gate; `list-panes --json --command` to find this repo's live
   agent panes (match by `clank agent start <label> --repo <repo>`
   terminal_command) and the status pane.
2. Compose a fresh layout KDL with the **new master's pane as the 65%
   stage** and every other agent pane (incl. the demoted old master) in
   the stacked 35% group, plus the status TUI pane — by reusing the layout
   composition `clank open` already uses (`open_zellij.rs`), parameterized
   by which label is master.
3. `zellij action override-layout <path> --apply-only-to-active-tab
   --retain-existing-plugin-panes`.

## Critical constraints (learned in the spike)

- **Compose from the live roster ∩ live panes — NOT the on-disk
  `.clank/zellij/layout.kdl`.** That file is stale: it only lists agents
  present at `open` time (runtime-added agents like `glm` are absent), so
  reusing it would orphan/misplace panes. Build the KDL for exactly the
  panes that currently exist; omit roster agents with no live pane so
  override doesn't spawn one.
- **Multi-line KDL only.** The compact `{ command "x"; args ... }`
  semicolon form fails zellij's KDL parser; emit the expanded node form
  (the form `clank open` already generates).
- **Best-effort / `.output()`-isolated.** The config role change is the
  source of truth and persists regardless of any layout outcome; a zellij
  hiccup (not in zellij, list-panes fails, parse/override fails) is a
  silent no-op, never a promote failure.

## Titles

`override-layout` matched panes by command and KEPT their existing titles
in the spike (the promoted pane stayed "<name> (reviewer)"). So after the
override, update titles to the new roles ("<new> (master)" / "<old>
(reviewer)") via `agent_pane_title`. Resolve the mechanism at impl: the
KDL `name=` may not rename a reused pane; if not, `rename-pane` (note it
targets the focused pane — prefer setting via the layout or a path that
doesn't steal focus, and restore focus to the caller via the
`caller_pane_id()` primitive from `zellij-focus-restore`). Title accuracy
is best-effort too.

## Also

- Drop the `set-master` clap alias on `agent promote` now the rename has
  settled (tracked from the parent plan / GLM note) — but KEEP any
  negative test asserting the old name is gone.

## Testing (no-binary-spawning)

- Pure helper that COMPOSES the promote layout KDL given (live panes,
  new-master label) → assert structure: new master is the 65% stage pane;
  all other agent panes are in the stacked group; status pane present;
  multi-line node form; an agent with no live pane is omitted; the on-disk
  `layout.kdl` is never read.
- The live `override-layout` / `rename-pane` calls stay untested like the
  rest of the zellij glue.

## Acceptance

- Promoting in zellij re-flows panes so the new master is the stage and
  the old master joins the stack, **no respawn** (pane reuse), titles
  follow — best-effort, no-op outside zellij or on any zellij failure.
- The config role change remains the source of truth, unaffected by the
  layout outcome.
- `set-master` alias removed (negative test retained).
