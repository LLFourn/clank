# placement-reads-zellij-stacks-correctly

## Problem

zellij tabs shift focus on their own, repeatedly, with nothing
happening in them. Reported: "my worktree in frostsnap-ci
'recovery-scan' keeps shifting focus to the ruthless pane even though
there's nothing going on there", and separately panes appearing
duplicated after an unrequested rebuild.

The reconcile pass is repairing a tab that is ALREADY correct, on every
refresh, forever.

## Root cause: the placement predicate misreads a zellij stack

`reviewers_are_stacked` (`open_zellij.rs:1230`) decides placement by
requiring every reviewer pane to report IDENTICAL
`(x, y, columns, rows)`, on the stated premise that "stack members are
reported at IDENTICAL geometry (the whole stack area)".

That premise does not hold on the installed zellij (0.45.0). A stack
renders as ONE expanded member plus COLLAPSED members one row tall.
Captured live, every two-reviewer tab in session `clank-fsctl`:

    nonce-aware-coin-selection  codex (116,1,62,1)   ruthless (116,2,62,93)
    firmware-upgrade-nudge      codex (116,1,62,93)  ruthless (116,94,62,1)
    sign-task-path-bounds       codex (116,1,62,1)   ruthless (116,2,62,93)
    change-index-leak-demo      codex (0,88,125,1)   ruthless (0,89,125,46)
    fix-anchor-above-tip        codex (116,1,62,1)   ruthless (116,2,62,93)

Same `x`, same width, vertically contiguous, exactly one expanded and
the rest one row. That is a STACK, and `stack-panes` had already built
it. The predicate reports "not placed" for all five.

## Why a wrong answer becomes unbounded work

`reconcile` (`status_tui/zellij.rs:590`) caches convergence only when
placement succeeded OR there are fewer than two reviewers:

```rust
if placed || reviewers.len() < 2 {
    self.converged = Some(cur);
}
```

So a permanently-false predicate means the pass NEVER converges: every
refresh re-runs `stack-panes` and the focus capture/restore
transaction. That is the churn — the user's focus lands on whichever
pane zellij last touched.

Two defects, and they compose: a predicate that can be wrong, and a
repair loop with no bound. Either alone is survivable.

## Goal

A correctly-stacked tab reads as placed. And no placement answer,
right or wrong, can produce unbounded repair.

## Approach

1. **Read the stack as zellij actually renders it.** Placement holds
   when this repo's reviewer panes share one column span (same `x` and
   `pane_columns`), tile a contiguous vertical run, and have exactly
   one expanded member with the rest at `pane_rows == 1`. Derive the
   rule from CAPTURED geometry, not from the old premise.

   There is no authoritative signal to prefer: `list-panes --json` on
   0.45.0 exposes `is_floating` / `is_fullscreen` / `is_held` /
   `is_suppressed` and an empty `index_in_pane_group`, but nothing that
   names a stack. Record that, so the next reader does not re-search.

2. **Keep the instrument-pane exclusion, re-derived.** The status pane
   must still be outside the reviewers' stack; under the corrected
   model that means it must not participate in the same column span and
   contiguous run. The original check exists because a reviewer sharing
   the instrument pane's stack is the reported bug, and it must not be
   lost while fixing the false negative.

3. **Bound repair regardless of the predicate.** After a small number
   of consecutive failed placements for the same `RosterView`, cache it
   and stop. `clank open` stays the escape hatch, exactly as it already
   is for the lone-reviewer case.

   This is the part that matters beyond this bug. Three of the four
   ways placement can fail are unfixable by retrying — reviewer panes
   in different tabs, the instrument pane already inside the stack (no
   `break-pane` exists), and a `stack-panes` the server silently
   rejected — yet only "fewer than two reviewers" is treated as
   terminal today. A wrong predicate should cost a wrong answer, never
   an infinite loop.

## Required tests

In-process, on CAPTURED geometry (no live zellij — spawning it in tests
is what leaked servers here before):

- **The five live fixtures above read as PLACED.** This is the
  regression; each is a real capture, not a constructed shape.
- **A genuine split still reads as NOT placed**: two reviewers side by
  side in different column spans.
- **Reviewers in different tabs read as NOT placed** and are not
  retried forever.
- **The instrument pane inside the reviewers' stack still reads as NOT
  placed** — the original bug must stay caught.
- **Unbounded repair is impossible**: a predicate stubbed to always
  fail converges after the bound and issues no further pane actions.
  Assert on the number of `stack` calls, since the symptom is the
  repeated ACTION, not the verdict.

## Acceptance

- A correctly stacked tab is never re-stacked.
- No reachable state issues pane actions on every refresh indefinitely.
- The instrument-pane exclusion still holds under the new model.

## Out of scope

- The swap-layout reflow that can move panes between slots
  (`zellij-layout-reflow-and-placement-limits`, blocked upstream).
- The `default`-team fallback that silently seeded `ruthless` into
  these rosters — real, and its own plan.
