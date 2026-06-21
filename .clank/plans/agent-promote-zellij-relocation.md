# agent-promote-zellij-relocation

Follow-up to `agent-promote-rename-zellij`. When `clank agent promote
<name>` runs in a zellij session, relocate panes to follow the role
change: the promoted agent's pane becomes the master/STAGE pane and the
demoted old master's pane joins the reviewer STACK. The config role change
is already implemented (`set_repo_master`); this projects it onto the live
zellij layout, best-effort.

## Spike result (decided 2026-06-22, human-driven)

A clean stack↔stage swap IS feasible in zellij 0.44.3 with **no process
restart** — but NOT via a direct pane swap:
- No "swap pane A↔B by id" verb. `move-pane <dir>` swaps geometry with a
  directional neighbor, but a **stack intercepts** it (intra-stack
  rotation via `progress_stack_up_if_in_stack`), so a stacked reviewer
  won't swap into the stage. `break-pane` (pop out of a stack) is
  **0.45-only**, absent in our 0.44.3. `stack-panes` works but is
  fragile/geometry-dependent (a wrong invocation collapsed all panes into
  one column).
- **`override-layout` is the chosen primitive.** It re-flows the EXISTING
  panes into a freshly-composed target layout. Verified live: pane ids
  preserved (`0,1,2,3,8`); the status pane's **pid was unchanged with
  continuously growing elapsed time** across an override → panes are
  reused, not respawned.

## How override-layout matches (the load-bearing detail)

`override_tiled_panes_layout_for_existing_panes` runs three phases:
1. **Exact-match + consume.** For each layout slot it finds an existing
   pane where `pane.invoked_with() == slot.run` and **removes it from the
   pool** (so one pane fills at most one slot), repositioning it.
2. **Spawn.** Slots with no `invoked_with` match → `position_new_panes`
   starts a NEW pane (new process).
3. **Close.** Existing panes never consumed → closed, unless a `--retain-*`
   flag covers their type (0.44.3 CLI exposes only
   `--retain-existing-plugin-panes`, NOT terminal).

There is **no positional fallback** in the override path — it is exact
`invoked_with` match *or spawn*. A slot whose command is off by one byte
spawns a new pane AND lets the unmatched original get closed.

**`invoked_with` is the original LAUNCH invocation, not the running
process.** This is the trap:
- `list-panes --command` reports `invoked_with` → `clank agent start
  <label> --repo <repo>` (stable; the match key). `clank agent start`
  papers over start-vs-resume — the session id is resolved INSIDE it, so
  the command does not vary with session state.
- `dump-layout` reports the **current foreground process** instead —
  observed live as `caffeinate -i -t 300` for idle claude-tool panes and
  `codex resume <session-id> …` for codex. These do NOT equal
  `invoked_with`.
- **Therefore: introspect with `list-panes --command`, never
  `dump-layout`.** Mutating a `dump-layout` and re-applying it would
  mismatch every agent pane (`caffeinate` ≠ `clank agent start …`) →
  phase 3 kills the live agent sessions and phase 2 spawns bare
  `caffeinate` panes. Do not go near `dump-layout` for this.

## Mechanism

After `set_repo_master` flips the config, best-effort project onto zellij:
1. `$ZELLIJ`-gate. `list-panes --json --command` → the live panes for this
   tab: ids, geometry, titles, and `terminal_command` (= `invoked_with`).
2. Identify this repo's agent panes by matching `terminal_command` against
   `agent_start_command(label, repo)` for each roster agent (the shared
   helper that launched them — `agent_start_argv`/`agent_start_command` in
   `open_zellij.rs`), and the status pane by its `clank status … --tui`
   command.
3. Compose a fresh layout KDL placing the **new master's pane as the 65%
   stage** and every other agent pane (incl. the demoted old master) in
   the stacked 35% group, plus the status pane — reusing the layout
   composition `clank open` already emits, parameterized by which label is
   master. Each slot's `command`/`args` come from
   `agent_start_argv(label, repo)` (the single source that produced the
   launch), so they byte-match `invoked_with`.
4. `zellij action override-layout <path> --apply-only-to-active-tab
   --retain-existing-plugin-panes`.

## Safety gate (mandatory — override has whole-tab blast radius)

Unlike the old surgical `new-pane`, override re-flows the entire tab, so a
composition mistake can kill EVERY session (phase 3). Before applying:
- **Preserve every live pane.** Include all live terminal panes in the
  composed layout — agents, status, AND any pane the user opened manually —
  or phase 3 closes the omitted ones. Drive inclusion from `list-panes`,
  not from the roster alone.
- **Assert command equality.** For every pane we intend to reposition,
  assert the derived `agent_start_command(label, repo)` equals the live
  `terminal_command`. If ANY expected pane fails to match (path rendering
  drift, version skew, unexpected pane), **skip the relocation entirely**
  rather than risk a kill. The config role change still stands.
- **Compose from live panes, NOT the on-disk `.clank/zellij/layout.kdl`**
  (stale — omits runtime-added agents like `glm`).
- **Multi-line KDL only.** The compact `{ command "x"; args … }` semicolon
  form fails zellij's parser; emit the expanded node form `clank open`
  already generates.
- **Best-effort / `.output()`-isolated.** Not in zellij, list-panes fails,
  any pane mismatch, parse/override failure → silent no-op, never a
  promote failure.

## Focus & titles

- **Declarative focus.** `override-layout` focuses whatever the layout
  marks `focus=true`. Mark the caller's own pane (`caller_pane_id()` from
  [zellij-focus-restore], read from `ZELLIJ_PANE_ID`) `focus=true` in the
  composed KDL so promote doesn't yank the user elsewhere — no imperative
  save/restore needed.
- **Titles.** override kept existing titles in the spike (matched panes
  retain their title). Update to the new roles ("<new> (master)" / "<old>
  (reviewer)") via `agent_pane_title`. Resolve at impl whether the KDL
  `name=` renames a reused pane; if not, a focus-free rename path (titles
  are best-effort too).

## Also

- Drop the `set-master` clap alias on `agent promote` now the rename has
  settled (parent-plan / GLM note) — but KEEP any negative test asserting
  the old name stays gone.

## Future unification (noted, NOT in scope here — reviewers: opine)

The same `compose_roster_layout(live_panes, master) → override-layout`
helper generalizes to the other roster→pane projections, because the three
override phases map onto each op: **promote** = swap slot positions (all
match → reposition); **add** = all live panes + one new slot (new slot has
no match → phase-2 spawn — exactly what add wants); **remove** = omit the
removed agent (it becomes a leftover → phase-3 close). Converging
`add_reviewer_pane` (anchor-find + `new-pane --stacked` + focus
save/restore) and `remove_reviewer_pane` onto this one declarative path
would delete the imperative focus-juggling glue — the exact class that
produced the [zellij-focus-restore] bug. Deliberately a SEPARATE follow-up
(bigger blast radius; prove the mechanism in promote first), but structure
this plan's composition as a reusable helper so the follow-up is a clean
extension. Reviewers: flag if you think add/remove should converge now
instead.

## Testing (no-binary-spawning)

- Pure helper: `compose_roster_layout(live_panes, new_master_label) → KDL`.
  Assert: new master is the 65% stage pane; all other agent panes are
  stacked; status pane present; every other live pane preserved; multi-line
  node form; commands equal the live `terminal_command`s; an agent with no
  live pane is omitted (and triggers the skip gate); the on-disk
  `layout.kdl` is never read.
- A pure test for the safety gate: a mismatched/extra live pane → the
  composer signals "skip" rather than emitting a layout that would close
  it.
- Live `override-layout` / `rename-pane` calls stay untested like the rest
  of the zellij glue.

## Acceptance

- Promoting in zellij re-flows panes so the new master is the stage and the
  old master joins the stack, **no respawn** (pane reuse via `invoked_with`
  match), titles follow, caller's focus preserved — best-effort, no-op
  outside zellij or on any zellij failure or pane mismatch.
- The config role change remains the source of truth, unaffected by the
  layout outcome.
- `set-master` alias removed (negative test retained).
