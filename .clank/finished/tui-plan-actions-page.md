# tui-plan-actions-page
# TUI plan-actions page: discoverable, state-aware plan operations

## Why (lloyd)

Plan operations (finish, squash, purge, stash, html) exist only as CLI
verbs; in the TUI a plan offers just the doc overlay and an
undiscoverable `o`. Lloyd wants a visible actions menu when you go to a
plan, with hotkeys, state-dependent options, scary confirmations for
the destructive ones, and errors shown IN the TUI.

## Design: the danger gradient

One landing page per plan. Routine actions sit quiet at the top;
destructive ones live below a `── danger ──` rule and the chrome gets
HEAVIER the deeper you go — menu row (dim red) → purge chooser →
full-width red reverse-video confirm. The drop confirm is deliberately
the loudest thing in the whole TUI and the only type-to-confirm.

**Entry**: `⏎` on a log umbrella Header row (they exist for active AND
finished plans) opens the plan page — replacing today's direct jump to
the doc overlay. The doc stays one keypress away as the first row;
menu-first is the point (discoverability beats one saved keystroke).

**Page** (`Mode::PlanDetail { stem, sel }`, agent-detail's shape —
rebind by stem across refreshes; plan vanished → back to log):

```
 PLAN soft-disallow-multiple-plans      ACTIVE · gate unreviewed · 3 commits

   ⏎  read the plan
   o  open in browser
   s  stash…           set the commits aside; pop later to resume
   f  force finish…    finalize NOW, bypassing the review gate
  ── danger ────────────────────────────
   p  purge…           rewrite history; next screen chooses how

  ↑↓ move · ⏎ select · esc back
```

FINISHED plans swap the middle block: `q squash…` (shown only when the
plan's range is >1 commit — an autosquashed plan is already one) and no
stash/force-finish. Selected row = reverse-video band (existing
convention); danger rows render dim red until selected, then red
reverse. Hotkeys act directly; ↑↓/⏎ equally. Hint footer like the
agent page.

**Purge chooser** (second screen, per lloyd: drop is a purge option):

```
 PURGE soft-disallow-multiple-plans

   a  artifacts only    strip .clank/ files from history; the code stays
   d  drop EVERYTHING   the plan AND its implementation commits vanish

  esc back
```

**Confirms**: artifacts-purge → red confirm modal, consequence sentence,
`y`/esc (default No). Drop → the scary one: full-width red
reverse-video banner, the consequence spelled out (verbatim from the
CLI help so wording stays single-sourced), and a TYPE-THE-STEM gate —
the input must equal the plan stem before `y` arms. Nothing else in the
TUI looks like this screen; that's the point.

**Force finish** (active plans): requires a real message — the page
opens a one-line input for the WHAT subject (must be non-empty); the
WHY paragraph is auto-provenance: "force-finished from clank status
--tui; gate was <gate> at bypass". Reviewers: challenge this default if
you think a typed WHY should be mandatory — the alternative is a
second input line, not a multiline editor (non-goal).

**Squash** (finished plans): one-line input prefilled with the finalize
commit's subject, editable, feeds `purge --squash <MSG>`.

**Block / unblock** (lloyd, added mid-plan): active plans get a pause
switch. `b block…` opens a one-line input for the reason (the block
question) and creates a plan-scoped block — a pending block already
suppresses every wait item for the plan, so this IS "pause" with no
history edit (softer than stash). On a blocked plan the row flips to
`b unblock` — since the human is the one clicking, unblocking answers
the pending block with "unblocked from the TUI" (the answer file), so
the creator's `block clean` flow stays intact. Creator label for
TUI-created blocks: the repo's MASTER agent (blocks live under an
agent dir; the master is the plan's owner). The page header shows
`BLOCKED · <question>` on blocked plans.

**One new widget**: a single-line editable text input (insert/backspace/
left/right/esc), reused by force-finish subject, squash message, and
the drop type-to-confirm. Pure state struct, unit-tested.

**Errors in the TUI**: any action returning `Err` opens an error
overlay — red title strip, the anyhow chain rendered scrollable, esc
back to the plan page. Also route the two currently SWALLOWED action
errors (`let _ =` on add/remove agent in run_confirm_action /
apply_detail_action) through the same overlay — silent failure is a
bug this plan fixes in passing.

## CLI addition: `clank finish --force`

Does not exist today; finish refuses when the gate isn't FINISHED.
`--force` bypasses ONLY the review-gate readiness check (readiness =
compute_finalize_readiness): messages stay mandatory, autosquash still
applies, worktree-dirty and other safety refusals stay. Prints a loud
`force: gate was <state>` line. In-process test: force-finishing an
unreviewed plan succeeds and autosquashes; without --force it refuses.

## Execution discipline

The TUI is a front-end to the cores — call `finish::run`,
`purge::run(yes: true, ...)`, `stash::run_push(yes: true, ...)`,
existing html-open plumbing in-process. Never spawn the clank binary;
never reimplement a write. Confirmation lives in the TUI screens, so
the cores' own prompts are bypassed with their `--yes` equivalents —
the TUI confirm IS the confirmation.

## Tests (pure, no binary spawning)

- Page row derivation: active vs finished vs finished-already-squashed
  vs blocked (block↔unblock flip); hotkey→action mapping incl. danger
  rows.
- Purge chooser flow; drop confirm arms only when the typed stem
  matches exactly (near-miss stays disarmed).
- Input widget editing ops.
- Error overlay from an Err (message text preserved).
- Entry: Enter on Header row (active + finished) → PlanDetail; refresh
  rebinds by stem; vanished plan exits to log.
- finish --force core test as above.

## Non-goals

Multiline message editing; unfinish/pick/pop rows (stash pop already
lives on STASH rows); reordering the log; changing `o` on other
overlays.

## Acceptance

From the TUI: force-finish an unreviewed active plan (message typed),
squash a multi-commit finished plan, stash an active plan, purge via
the chooser with the scary drop path requiring the typed stem, open a
plan's html — and a failing action (e.g. stash push with foreign
commits) shows its error in the TUI instead of dying or vanishing.
clippy/fmt/suites green.
