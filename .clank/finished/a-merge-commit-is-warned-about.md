# A merge commit is read as one commit, not held to the tag rule

## Why

GitHub's "Merge pull request #6" (b733b1dc) became HEAD after a pull. The
HEAD commit-tag check diffs HEAD against its first parent, so the merge
looked like an untagged commit touching
`.clank/finished/github-watches-are-event-driven.md`. That raised a
`fix-commit-tag` item whose only cure is amending a pushed master
commit, so it would fire forever.

Lloyd's direction (2026-11-05): clank should cope with a merge quietly
when it can be read coherently. Read it as one commit in a linear
history, containing everything the merged branch brought in. Cache at
the merge like any other commit, add no new history or cache logic, and
do not raise an alarm just because a merge exists.

The fold already reads merges that way. `commit_events_between` walks
first parents, and a merge's event is its diff against its first
parent. So b733b1dc folds as the one commit that finished
`github-watches-are-event-driven`, and its state is cached under its own
sha. Anything in that diff the fold cannot fit, such as an intro while
another plan is active, is already handled by the existing fold rules
and warnings, as for any commit.

The only defect is the tag check. A merge's subject is written by
whatever made the merge. It is not a claim about any plan, so holding
it to the `[plan]` tag rule has no meaning.

## What changes

- `HeadCommit` gains `merge: bool`. `git_io::head_commit` sets it when
  HEAD has more than one parent, using the existing
  `commit_parent_count`.
- `head_tag_violation` returns `None` for a merge HEAD, alongside the
  existing not-adopted guard.

Nothing else changes: the fold, the cache, status, the TUI and doctor
stay as they are. A merge raises no warning.

## Acceptance

- The `head_tag_violation` table: the same untagged plan-file touch is
  a violation on an ordinary commit and none on a merge.
- `derive_status` with a merge HEAD touching the active plan's file
  gives no `head_correction`, no `MasterToFixCommitTag` on the plan, and
  no `FixCommitTag` item for the master.
- `head_commit` on a fixture repo, built in the test with git, reports
  an ordinary commit as `merge: false` and a `--no-ff` merge as
  `merge: true`.
- Mutation check: drop the merge guard, and both core tests fail.

## As landed

2d1ebee4, as revised in 296f3f6d. The intro's warning design was dropped at Lloyd's direction: clank copes with a merge quietly.
- `HeadCommit::merge`, set in `git_io::head_commit` from `commit_parent_count`.
- The `head_tag_violation` guard is `!head.adopted || head.merge`.
- Tests:
  - `wait::tests::a_merge_head_is_not_held_to_the_tag_rule`;
  - `wait::tests::derive_status_merge_head_is_no_correction`;
  - `git_io::tests::a_head_with_two_parents_is_a_merge`.

  Dropping the merge guard fails both core tests.

The stem still says "warned-about" from the intro. Renaming an active plan was not worth the churn, so the title carries the real meaning.
