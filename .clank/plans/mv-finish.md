# mv-finish

## Summary

Two changes:

1. **Finish = mv.** `clank finish <plan>` does
   `git mv .clank/plans/<plan>.md .clank/finished/<plan>.md`
   and commits. No marker files, no sealed approvals, no
   separate predicate. A plan is finished iff its `.md` lives
   under `finished/`.

2. **`clank unfinish <plan>`.** Finds the commit that moved the
   plan file to `finished/`, rewrites it out of history (drop if
   it was the only change, strip the mv otherwise), and leaves
   the plan file back in `plans/`.

## Current state

- `clank finish` creates an empty `.clank/finished/<plan>` marker
  and commits. The plan file stays in `.clank/plans/`.
- `finish_predicate_at` checks blob existence of the marker.
- `enrich_with_newly_finished` diffs parent vs commit tree for
  the marker to fire `newly_finished`.
- A rename OUT of `.clank/plans/` triggers `TouchKind::Delete`
  in the fold, which hard-removes the plan AND its
  `finished_plans` entry. This is wrong for the new model —
  the mv-to-finished rename must be a finish, not a delete.
- Finished plans in this repo have BOTH a plan file in `plans/`
  and an empty marker in `finished/`.

## Design

### Finish detection in the fold

Don't rely on git's rename detection. Instead, after
collecting all plan touches and finished-path touches from
the diff, reconcile: if the same stem has a Delete from
`plans/` AND an Add to `finished/` in the same commit,
collapse both into a single `TouchKind::Finish`. Reverse
(Delete from `finished/` + Add to `plans/`) collapses into
`TouchKind::Intro` (unfinish).

This means `parse_diff_tree` collects two independent lists:
- plan touches (as today): intro/revise/delete on `plans/`
- finished touches: add/delete on `finished/`

Then a reconciliation pass merges them. A plan Delete +
finished Add = Finish. A finished Delete + plan Intro =
Intro (unfinish). Unmatched finished adds/deletes are just
recorded for the clank_paths / strip_paths tracking.

This replaces the `newly_finished` / `enrich_with_newly_finished`
/ `finish_predicate_at` machinery entirely. Finish is
detectable from the diff alone — no tree inspection needed.

### `is_finished_path`

New predicate: `.clank/finished/<name>.md` (flat, same rules
as `is_plan_path` but under `finished/`). Used by
`parse_diff_tree` to detect finished-path touches.

### `clank finish`

Replace the current `finalize()` body with:

```
git mv .clank/plans/<stem>.md .clank/finished/<stem>.md
git commit -m "Finish <stem>"
```

Gate check (is the plan approved?) stays in the preview.
The `--amend` path simplifies too — HEAD must be a commit
whose only diff is the rename.

### `clank unfinish <plan>`

Does NOT rewrite history. Just moves the file back:

```
git mv .clank/finished/<plan>.md .clank/plans/<plan>.md
git commit -m "Unfinish <plan>"
```

The finish commit stays in history. The fold sees the reverse
rename (finished/ → plans/) and re-intros the plan. This is
the same mechanism as "rename INTO plans/" which already
produces `TouchKind::Intro`.

This avoids all the rewrite edge cases (later edits to the
finished file, non-HEAD finish commits, mixed commits). The
history accurately reflects what happened: plan was finished,
then un-finished.

### Migration

The repo currently has empty markers in `finished/` and plan
files still in `plans/`. Migration:

1. For each plan in `finished/`: if `plans/<plan>.md` exists,
   `git mv plans/<plan>.md finished/<plan>.md` and delete the
   empty marker. If `plans/<plan>.md` doesn't exist (shouldn't
   happen), just delete the marker.
2. Commit as `Migrate finished plans to mv-finish layout`.

This is a one-shot operation done in this plan's implementation,
not a migration command.

### `FinalizeChange` / `FinalizeChangeKind`

Remove entirely. Finish is now a `PlanTouch` with
`TouchKind::Finish`, not a separate `FinalizeChange`. The
`FinalizeChange` struct, `FinalizeChangeKind`, and all the
finalize-change handling in `parse_diff_tree` go away.

### `enrich_with_newly_finished`

Remove entirely. `newly_finished` on `CommitEvent` goes away.
The fold sees `TouchKind::Finish` directly from the plan touch
list — no second pass over the tree needed.

### `finish_predicate_at` / `read_finalize_snapshot`

Remove. No longer needed — finish is detected from the diff,
not from tree inspection.

### `disk_format::FinalizePath` / `parse_finalize_path`

Remove. The finished path is just `is_finished_path` — same
pattern as `is_plan_path`.

### `preview.rs`

`build_finish_preview` keeps its gate check via
`scan_feedback` + `compute_gate`. The sealed-approval
mechanism is already gone (adhoc-review removed it).

`build_rewrite_preview` / `tree_plan_paths`: update pathspecs
to include `.clank/finished/<stem>.md` instead of
`.clank/finished/<stem>`.

### `purge.rs`

Update `build_amend_program` path matching: the finalize
commit's diff is now a rename (`R100 .clank/plans/<stem>.md
.clank/finished/<stem>.md`), not an add of a marker file.
Strip paths include `.clank/finished/<stem>.md`.

### `head_is_finalize_for`

Check that HEAD's diff is exactly the rename from plans/ to
finished/ for the given stem.

## Implementation surface

### `crates/core`

- `repo_state.rs`: add `TouchKind::Finish`. Handle in
  `apply_commit`: archive to `finished_plans`, emit
  `LogEvent::PlanFinalized`. Remove `newly_finished` from
  `CommitEvent`.
- Remove `FinalizeChange`-related fields from fold input.

### `crates/cli`

- `git_io.rs`: add `is_finished_path`. Update `parse_diff_tree`:
  rename plans/→finished/ emits `PlanTouch::Finish`. Remove
  `FinalizeChange` handling, `finish_predicate_at`,
  `read_finalize_snapshot`, `parse_finalize_subpath`.
  Remove `enrich_with_newly_finished` calls.
- `disk_snapshot.rs`: remove `FinalizeChange`, `FinalizeChangeKind`,
  `enrich_with_newly_finished`, `newly_finished` from
  `CommitEvent`/`CommitChanges`.
- `finish.rs`: `git mv` + commit. Simplify `head_is_finalize_for`.
  Remove old `finalize()`.
- `preview.rs`: update `tree_plan_paths` pathspec. Keep gate check.
- `purge.rs`: update amend program for rename-based finalize diff.
- `disk_format.rs`: remove `FinalizePath`, `parse_finalize_path`.
- New: `cli/unfinish.rs` — find finish commit, drop or strip.
- `mod.rs`: add `UnfinishArgs`, wire subcommand.
- Migration commit: mv existing finished markers to plan files.

## Tests

- `git mv plans/foo.md finished/foo.md` → fold archives plan.
- `git mv finished/foo.md plans/foo.md` → fold re-intros plan.
- `clank finish` produces the mv commit.
- `clank unfinish` produces the reverse mv commit.
- Finish then edit `finished/foo.md` then unfinish preserves edits.
- `parse_diff_tree` rename plans→finished = Finish touch.
- `parse_diff_tree` rename finished→plans = Intro touch.
- Purge amend works with rename-based finalize.
- Migration: old markers + plan files → mv'd layout.
