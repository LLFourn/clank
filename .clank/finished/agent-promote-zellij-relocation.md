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

There is **no positional fallback** in the override path — exact
`invoked_with` match *or spawn*. A slot whose command is off by one byte
spawns a new pane AND lets the unmatched original get closed.

**`invoked_with` is the original LAUNCH invocation, not the running
process.** This is the trap:
- `list-panes --command` reports `invoked_with` → `clank agent start
  <label> --repo <repo>` (stable; the match key). `clank agent start`
  papers over start-vs-resume — the session id is resolved INSIDE it, so
  the command doesn't vary with session state.
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
2. **Classify every live terminal pane** (see Safety gate) — agent pane
   (command byte-equals `agent_start_command(label, repo)`) or status
   pane. If classification fails, SKIP.
3. Compose a fresh layout KDL placing the **new master's pane as the 65%
   stage** and every other agent pane (incl. the demoted old master) in
   the stacked 35% group, plus the status pane — reusing the layout
   composition `clank open` already emits, parameterized by which label is
   master. Each slot's `command`/`args` come from `agent_start_argv(label,
   repo)` (the single source that produced the launch), so they byte-match
   `invoked_with`.
4. `zellij action override-layout <path> --apply-only-to-active-tab
   --retain-existing-plugin-panes`.

## Safety gate — skip-on-unrecognized (v1 rule)

override re-flows the ENTIRE tab with no positional fallback: any live
terminal pane NOT byte-matched by a composed slot is closed (phase 3 —
0.44.3 retains only plugin panes, not terminal). So classification is
all-or-nothing, decided BEFORE applying:

- **Classify every live terminal pane in the tab.** Each must be either
  (a) a roster-agent pane whose `terminal_command` byte-equals
  `agent_start_command(label, repo)`, or (b) the status pane (its `clank
  status … --tui` command).
- **All classified → compose and override** (new master = 65% stage, other
  agents = 35% stack, status below; slot commands from
  `agent_start_argv`).
- **ANY live terminal pane unclassified (a manual shell/editor, or a pane
  with null/odd `invoked_with`), OR any expected agent pane fails the
  equality check → SKIP the whole relocation.** Config role change still
  stands; the layout is left as-is. Do NOT try to passthrough/reproduce an
  arbitrary manual pane — there's no natural slot for it in the
  stage+stack shape and a null `invoked_with` can't be matched (passthrough
  is a possible LATER enhancement, not v1).
- **Omit ≠ skip.** A roster agent that simply isn't running (no live pane)
  is just omitted from the composed layout — relocation still proceeds.
  Skip is triggered ONLY by an unclassified live pane, or a mismatch on a
  pane we'd reposition.
- **Compose from live panes, NOT the on-disk `.clank/zellij/layout.kdl`**
  (stale — omits runtime-added agents like `glm`).
- **Multi-line KDL only** (compact `{ command "x"; args … }` semicolon form
  fails zellij's parser).
- **Best-effort / `.output()`-isolated**: not in zellij, list-panes fails,
  skip-triggered, or parse/override failure → silent no-op, never a
  promote failure.

### Documented consequences of skip-on-unrecognized
- Relocation **no-ops whenever the agents tab has a stray non-agent pane**
  (or an agent pane whose command doesn't byte-match). Intended: the config
  is the source of truth and still changes; the layout just isn't touched.
- **Declarative focus needs the caller pane to be in the composed layout.**
  In the real workflow the caller IS the master agent pane (classified,
  byte-matchable), so `focus=true` on it works. A promote invoked from a
  manual shell pane would skip (its pane is unclassified) — consistent with
  the rule, just noted.

## Focus & titles

- **Declarative focus.** `override-layout` focuses whatever the layout
  marks `focus=true`. Mark the caller's own pane (`caller_pane_id()` from
  [zellij-focus-restore], read from `ZELLIJ_PANE_ID`) `focus=true` in the
  composed KDL so promote doesn't yank the user elsewhere.
- **Titles — REQUIRED, not cosmetic.** override keeps matched panes'
  existing titles, and the status-TUI retitle loop derives each agent's
  role by PARSING its pane title (`parse_agent_panes` reads the `(master)`
  / `(reviewer)` suffix), NOT from config — so a stale title makes the TUI
  re-affirm the OLD role forever. After the override, rename the two
  role-changed panes via `zellij action rename-pane --pane-id <id>
  "<label> (<role>)"` (`agent_pane_title`). This is **by-id, no focus
  change** (`rename-pane --pane-id` IS supported on 0.44.3 — the status TUI
  already uses it). The TUI re-adds the status emoji on its next refresh.
  Best-effort (skip a pane that isn't live).

## Also — drop the `set-master` alias

- Remove the `set-master` clap `visible_alias` on `agent promote`
  (`mod.rs:876`). The existing test at **`mod.rs:1443-1446` asserts `agent
  set-master` MUST STILL PARSE — INVERT it** to assert it no longer parses
  (do NOT delete it; keep the regression guard). Leave the separate `team
  set-master` negative test (`mod.rs:1491`) alone.

## Future unification (NOT in scope — reviewers concur: defer)

The same `compose_roster_layout(live_panes, master) → override-layout`
helper generalizes: **promote** = swap slot positions (all match →
reposition); **add** = all live panes + one new slot (no match → phase-2
spawn); **remove** = omit the removed agent (leftover → phase-3 close).
Converging `add_reviewer_pane`/`remove_reviewer_pane` onto it would delete
the imperative focus-juggling glue — but those ops currently WORK with low
blast radius (surgical `new-pane`/`close-pane`, focus now correct after
[zellij-focus-restore]), and override is a new whole-tab-blast-radius
primitive. All reviewers agreed: prove it on promote first (the safest
case — every slot matches), and each of add/remove needs its own safety
analysis (e.g. remove's phase-3 close must not catch a pane the user
wants). **Build `compose_roster_layout` as a reusable helper now** so the
later convergence is a clean extension; keep convergence a separate
follow-up.

## Testing (no-binary-spawning)

- Pure: `compose_roster_layout(live_panes, new_master_label) → Outcome`
  (an applyable KDL, or Skip). Assert:
  - all-classified tab → KDL with new master as 65% stage, other agents
    stacked, status present, multi-line node form, slot commands equal the
    live `terminal_command`s; on-disk `layout.kdl` never read.
  - a roster agent with NO live pane → OMITTED, still yields a KDL
    (relocation proceeds).
  - an UNCLASSIFIED live terminal pane (manual shell) → **Skip**.
  - an agent pane whose command doesn't byte-match → **Skip**.
- The live `override-layout` / `rename-pane` calls stay untested like the
  rest of the zellij glue.

## Acceptance

- In a clean agents tab (only agent + status panes), promoting in zellij
  re-flows panes so the new master is the stage and the old master joins
  the stack — **no respawn** (`invoked_with` match), titles follow,
  caller's focus preserved.
- With any unclassified pane or command mismatch, relocation **no-ops**
  (config role change still applies).
- Best-effort: no-op outside zellij or on any zellij failure.
- The config role change remains the source of truth, unaffected by the
  layout outcome.
- `set-master` alias removed; its parse test **inverted** (not deleted).
