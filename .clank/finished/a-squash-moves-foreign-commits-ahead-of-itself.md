# a-squash-moves-foreign-commits-ahead-of-itself
# A squash moves foreign commits ahead of itself

## Why

`clank finish --squash` refuses when a commit that is not the plan's own sits
inside the plan's range:

    --squash refuses N foreign commit(s) interleaved within the plan's own range

(`rewrite.rs`, the "Squash adds one more blocker" check). `--force` does not
reach it — it bypasses only the review gate — so there is no way through short
of `clank purge` and hand-retagging. An adhoc fix committed in the middle of a
plan is ordinary, so this refusal comes up in exactly the case it should not.

The refusal exists for a real reason, stated in the code: replay uses FULL
TREES, so replaying an interleaved commit after the squash would reset the
plan's later changes. The answer is not to collapse foreign work into the plan's
commit — that would erase its message and authorship — but to move it.

## What to do

When squashing, put the range's foreign commits BEFORE the squashed commit, and
error only if that cannot be done cleanly.

1. Starting from the intro's parent, re-apply each foreign commit in its
   original order as a CHANGE (cherry-pick semantics, through
   `git_plumbing`), keeping its message and author.
2. Create the plan's single squashed commit on top, with the tree of the plan's
   original tip — so the squashed commit is exactly the plan's own changes, and
   nothing about the final content moves.
3. Restack whatever followed the squash tip, as today.

If re-applying any foreign commit conflicts, stop, name that commit and why,
and leave history untouched. Typically this happens because the commit depended
on a plan commit that came before it.

"Foreign" here is the existing classification — a commit not attributed to this
plan (`foreign: !attributed` in `preview.rs`). That includes adhoc commits AND
another plan's commits, and moving either ahead preserves it the same way. Say
so in the output rather than treating them differently by accident.

## Acceptance

- An interleaved foreign commit that applies cleanly is moved ahead of the
  squash. The result is that commit, with its original message and author,
  then the one squashed plan commit.
- The final tree is byte-identical to the tree before the squash, asserted as an
  invariant of the operation. Reordering may never change content.
- Several foreign commits keep their relative order.
- A foreign commit that cannot be moved errors, names the commit and the reason
  (a conflict, naming the paths), and rewrites NOTHING — atomic, no
  half-reordered branch.
- Commits after the squash tip are unaffected, as today.
- `--dry` shows the planned order — foreign commits, then the squash — before
  anything moves.
- `--squash --purge` works the same way. A foreign commit keeps its own changes;
  only the plan's `.clank/` artifacts are stripped, as purge already does.
- The error message and `finish --help` describe the new behaviour. The old
  refusal's advice ("re-tag or move them, or use `clank purge` first") goes,
  since the squash now does the moving.
- Tests drive the real rewrite engine on a real repository fixture, which is
  permitted since test code may use git. The cases are: one clean foreign
  commit, several, one that conflicts, a foreign commit after the tip, and
  another plan's commit in the range.

## Watch out

- The post-rewrite feedback migration must still map every rewritten sha,
  including the moved foreign commits, which get new shas.
- A merge commit in the range is still refused, as today.
- Moving a foreign commit earlier changes the tree it was authored against. A
  clean apply proves only that it applies, not that it is still correct there.
  That is the same trust any rebase extends, and the dry run is where an
  operator sees it.
