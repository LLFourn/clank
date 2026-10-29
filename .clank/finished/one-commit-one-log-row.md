# one-commit-one-log-row
# One commit, one log row

## Problem

In `~/src/frostsnap/.clank/worktrees/full-app-sim-driver` (2026-10-29), one commit
adds about 81 `.clank/finished/*.md` records. It is a restore of the `.clank/`
tree that a CI-fork rebase had stripped ("Restore the simulator line's clank
plans"). After a squash it is shown as its own commit, `44cc9b2`, and the TUI log
shows it as:

- about 30 rows all reading `44cc9b2 simctl-up`, each followed by a
  `── sim-… · 0s ──` divider
- the pile fills the pane and pushes real history out of view

Mechanism (traced in code):

- With no plan active, `apply_commit` (`repo_state.rs` ~806-827) turns every
  finished file a commit adds into its own `FinishedPlan { intro: sha,
  finalized_at: sha }` plus a `LogEvent::PlanFinalized` with duration 0.
- `umbrella_sections` (`repo_state.rs` ~386-428) starts a new run for each plan,
  and `oneline_rows` (`log.rs` ~1272-1361) emits a commit row and a footer for
  every run. So the same sha is repeated once per plan.
- `stands_for_plans` (`status.rs` ~1151-1163) is a `BTreeMap<sha, plan>`, so only
  the last plan in byte order survives. That is why every row says `simctl-up`.

## Change

- **A commit appears in the log once.** A commit that finishes several plans
  (squashed, restored or imported records) is one row, not one per plan. There
  are no per-plan `· 0s` dividers for it.
- **It gets its own marker:** `⁂`. Lloyd suggested a bug, but emoji are two
  columns wide and break the one-column gutter. Its text says it carries many plans: the commit subject plus
  a count, for example `44cc9b2 🐛 Restore the simulator line's clank plans
  (81 plans)`. No single plan is picked to stand for it.
- A commit that finishes exactly one plan renders as it does now.
- The bulk signal comes from the events themselves (several `PlanFinalized` on
  one sha), inside the shared row producer, so the TUI, web ledger and
  `clank log --oneline` all get it. A bulk commit is a run of its own, never part
  of a member plan's run, and keeps its subject as written. The finished-plan
  records stay per plan.
- The log window counts rows the user sees, so one bulk commit can't push the
  rest of history out of view.

## Out of scope

- Changing what squash or finish treat as a plan's intro.

## Acceptance

- A fold test with this real shape: no plan active, then one commit adding many
  `.clank/finished/*.md`, then a later tagged plan on top. The log rows contain
  that sha exactly once, with the multi-plan marker and the count, and no `0s`
  plan footers. The later plan's rows are unchanged.
- A commit finishing one plan renders exactly as before (existing tests stay
  green).
- Mutation: restore per-plan emission and the test fails. Restore the
  last-wins single label and the test fails.
- No WHAT comments.
