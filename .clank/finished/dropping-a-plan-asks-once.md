# dropping-a-plan-asks-once
# Dropping a plan asks once

## Why

> "can you please remove the 'type the name of the plan' thing from
> clank purge. it's git you can always recover it."

The gate is in the TUI, not the CLI — `clank purge --drop` on the
command line asks nothing. Choosing `drop EVERYTHING` on the plan page
opens `PlanInputKind::DropStem` and refuses to arm until the exact stem
is typed, byte for byte.

The reasoning written beside it was "a y/N would be one habitual
keystroke from losing real work". That is the wrong fear. A dropped
plan's commits are unreachable, not gone: they sit in the reflog and
the object store until gc, and `git reflog` plus `git reset --hard`
brings the branch back. The typing gate protects against something git
already protects against, and charges for it on every use.

## The model

**Ask once, and say what it costs.** Every other destructive action on
that page is a `Confirm` screen — a full page that names the
consequence and takes one keystroke. `PurgeArtifacts`, the roster
removals, killing an attended wait: all of them. Drop is the only one
that asks the operator to transcribe a name, and the only reason was
that it felt scarier.

So drop joins them: `ConfirmAction::PurgeDrop` on the same `Confirm`
screen the others use. That screen must not say LESS than the typed
gate did, and today it says less than I claimed here: `drop_banner`
lives in `purge.rs` and goes to stderr during execution, while
`render_plan_confirm` has its own wording and is handed only the stem
(codex on e34afc6). So the count and the branch have to reach the
page — the count from the fold, which already records every commit
attributed to a plan, and the branch from the snapshot.

The recovery is worth SAYING on that screen rather than leaving the
operator to know it. A confirm page that names the reflog is a better
safeguard than a spelling test, because it tells them what to do after
a mistake instead of trying to prevent one.

## Deliverables

1. **`PurgeChoice::Drop` goes to `Mode::Confirm { PurgeDrop }`**, not
   `Mode::PlanInput { DropStem }`.
2. **`PlanInputKind::DropStem` and `input::drop_armed` go**, with the
   render arm and the armed-indicator logic that existed only for
   them. A gate nothing reaches is worse than no gate.
3. **The confirm page names the way back**: the reflog, and that the
   commits become unreachable rather than destroyed — until the
   reflog expires or gc prunes them, which is the honest limit.
4. **And the size and the branch**, from the fold and the snapshot, so
   the screen that decides a drop says how much it takes and from
   where.
5. **Nothing else about drop changes** — same refusals (foreign
   commits), same engine, same default of No.

## Tests

- Choosing drop lands on the confirm page, and one keystroke there
  runs the same action the typed gate used to run.
- `DropStem` is gone: no mode, no predicate, no render arm. The tests
  that exercised near-miss and padded stems go with it, since they
  test a rule that no longer exists rather than one that changed.
- The confirm page still names the count, the branch, and no-archived-
  copy — and now the reflog.
- Mutation-check the ones that remain.

## Out of scope

- `clank purge --drop` on the CLI, which already asks nothing.
- Whether drop should exist, or should stash first. `clank stash push
  --to-queue` is the answer to "I might want this back" and is
  unchanged.
- Any other confirm page's wording.
