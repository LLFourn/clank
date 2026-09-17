# a-squash-that-keeps-the-tree-need-not-be-clean
# A squash that keeps the tree need not be clean

## Why

> "it seems that clank finish with --squash or autosquash on doesn't do
> it when the repo is dirty. Can you fix that."

Two things go wrong, and the second is worse than the first.

`rewrite::run` refuses on `working tree dirty; commit or stash first`
— but `finish` calls it AFTER `finalize()` has already landed. So a
dirty tree gets you `finalized 'x'` followed by an error, with the plan
finished and NOT squashed. The operation is half-done, and the way back
is the retroactive `--squash` path on an already-finished plan.

And the refusal itself is over-broad. A plain `--squash` does not
change the tree at all: `apply_squash` builds the squashed tree from
the tip's, minus `head_strip_paths`, and for a squash those paths are
the plan's `.clank/plans/<stem>.md` — which finalize already RENAMED
away, so stripping it is a no-op on a tree that no longer has it. The
squashed commit carries the same tree as the tip it replaces. Moving
the branch to it cannot disturb a working tree, dirty or clean.

`--purge` is the opposite: `include_finalize` puts
`.clank/finished/<stem>.md` in the strip set, that path IS in the tree,
and the tree changes. There the blocker earns its place.

## The model

**The blocker belongs to rewrites that change the tree, not to
rewrites.** One flag was standing in for a property, and the property
is computable: the rewrite knows the tree it is about to write and the
tree that is there now. Ask, rather than assume from `--purge`.

And a refusal must come before anything lands. `finish` decides to
rewrite only after finalizing, so every blocker it could hit arrives
too late to matter.

## Deliverables

1. **Check the rewrite's blockers BEFORE `finalize()`**, so a refusal
   leaves the plan exactly as it was. Nothing half-done.
2. **The dirty blocker fires only when the resulting tree differs from
   HEAD's** — derived from the computed plan, not from `args.purge`,
   because a flag that happens to correlate today is the kind of thing
   that stops correlating.
3. **Autosquash gets the same treatment**, since it is `--squash` by
   another route and the report says it fails the same way.

## Tests

- A dirty tree squashes: the plan finalizes, the range collapses, and
  the working tree is untouched — the same diff and the same untracked
  files before and after, asserted, not assumed.
- A dirty tree still refuses `--purge`, with the message it has now.
- A refusal finalizes NOTHING: the plan file, HEAD and the branch are
  where they were.
- Autosquash on a dirty repo does what `--squash` does.
- Mutation-check each.

## Out of scope

- Any other blocker in `collect_blockers` — a non-linear range or a
  missing intro is not about the working tree and is not what was
  reported.
- Stashing anything. The point is that a tree-preserving squash has no
  reason to touch the working tree at all.
