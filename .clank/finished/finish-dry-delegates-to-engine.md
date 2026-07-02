# finish-dry-delegates-to-engine

Complete the invariant established by finish-dwim-offhead-reword: EVERY
history-edit `--dry` in `clank finish` must be the exact plan the live run
executes, produced by the rewrite engine's own `dry` mode — finish.rs owns NO
hand-rolled preview text for a history edit.

(Merged from the two queued duplicates: ruthless's
`finish-dry-delegates-to-engine` + claude's `finish-fresh-dry-through-engine`.)

## Problem — what actually remains

The reword path satisfies the invariant (`--dry` threads through
`reword_in_place(dry)`), and the RETROACTIVE already-finished
`--squash`/`--purge` `--dry` was converged in e6f4aa8 (routes into
`run_post_finalize_rewrite` → `rewrite::run(dry)`). ONE case remains
hand-rolled: `dry_run_finish_composite` — the **fresh** (not-yet-finalized)
`--squash`/`--purge` `--dry` ("# would create finalize… # would then squash…").
It can silently diverge from what `rewrite::run` actually does — its blockers,
the exact commits collapsed, the strip sets — which is the drift the invariant
forbids (lloyd: "all edits of history must be done by taking the output that
produces --dry and executing it").

## The technical crux (why it wasn't converged already)

The fresh operation is finalize-THEN-rewrite, and the finalize commit doesn't
exist at preview time: `build_rewrite_preview` walks real history, so the
engine cannot compute the true post-finalize plan. NOTE: naively calling
`rewrite::run(dry)` on the pre-finalize state would preview a range MISSING
the finalize commit — a different plan than the live run executes, i.e. the
same drift in new clothes. The convergence must be exact:

- **Hypothetical-finalize preview**: compute the rewrite plan over `history +
  a synthetic finalize commit` (tree = HEAD tree with `plans/<stem>.md` →
  `finished/<stem>.md` applied in memory; gix tree builders already exist in
  git_plumbing). A dangling commit object (never ref'd) or an in-memory
  equivalent is acceptable. The engine's dry mode then prints the same
  commits/blockers the live composite run would produce.

## Fix

- **Finalize framing** (finish-level, the engine can't know it): "would create
  finalize commit / message / sealed approvals". Keep — legitimate context,
  not a history-edit preview.
- **The range rewrite** (squash/strip): DELEGATE to `rewrite::run(dry: true)`
  over the hypothetical-finalize preview. Do NOT re-describe it in finish.rs.
- Delete `dry_run_finish_composite`.

## The rule to encode (and assert)

finish.rs calls the engine (`reword_in_place` / `rewrite::run`) with `dry` set
from `args.dry`; the engine is the ONE place that both prints and performs, so
`--dry` and execute cannot drift. finish.rs must contain no bespoke "would
rewrite / would squash / would strip" narration for a history edit.

## Tests

- The squash `--dry` output and a real squash over the same fixture agree on
  the operation set (same commits collapsed, same resulting subject).
- A blocker fixture (merge commit in range, dirty tree, protected branch)
  makes BOTH the `--dry` surface the blocker AND the live run refuse — mirror
  `reword_in_place_refuses_merge_in_range_live_and_reports_in_dry`.
- The purge `--dry` reflects the actual strip the live run performs.
- Fresh `--dry` makes no commits/ref moves (incl. no stray refs; a dangling
  object itself is fine).

## Acceptance

- Fresh squash/purge `--dry` is produced by `rewrite::run(dry)` over a
  hypothetical-finalize preview — the same plan the live run then executes;
  `dry_run_finish_composite` no longer exists.
- `--dry` and execute provably agree on operation set + blockers (tests).
- clippy at baseline; suites green.

## Non-goals

- No change to reword or retroactive dry paths (already converged).
- No `--dry` for plain finish (no history rewrite happens there).
