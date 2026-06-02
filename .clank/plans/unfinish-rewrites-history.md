# unfinish-rewrites-history

`clank unfinish` currently lands a NEW commit that reverts the
plans/→finished/ rename. That bloats history — the original
finish commit + an inverse commit on top. The intent is to
rewrite history so the finish commit goes away, not stack a
reversal on top of it.

## Behavior

Drop the finish commit from history. Required preconditions:

- A finish commit for `<stem>` exists somewhere in the chain.
  Identified the same way `head_is_finalize_for` does: the
  commit adds `.clank/finished/<stem>.md`.
- The finish commit is at HEAD. v1 only supports unfinishing
  the most recent finish — keeps the implementation small,
  matches `clank finish --amend`'s "you're acting on the
  freshest event" shape.
- The finish commit's tree change is the rename and nothing
  else (the normal case via `clank finish`). If it carries
  other changes, bail with a clear error telling the user to
  resolve manually — don't silently drop unrelated work.

When the preconditions hold, rewrite by:

1. `git reset --hard HEAD~` (or equivalent) — drops the
   finish commit entirely.
2. Restore `.clank/plans/<stem>.md` from the
   pre-finish tree, since the rename took it out. This is the
   inverse of `clank finish`'s file move; same tooling.
3. No new commit. The branch tip is now where it was before
   `clank finish` ran.

User-facing log: `unfinished `<stem>`; dropped finalize commit
<sha>`.

## Why

The current code makes `clank unfinish` look like a forward
edit. It's actually the inverse of a rewrite-style command:
finish moves files + commits; unfinish should undo both at the
git level. Otherwise reviewers see an ugly inverse pair every
time someone hits unfinish.

## Surfaces touched

- `crates/cli/src/cli/unfinish.rs` — drop the
  `git mv finished/<stem>.md plans/<stem>.md` + `git commit`
  pair. Replace with: validate HEAD is the trivial finish
  commit, run `git reset --hard HEAD~`, restore the plan
  file from the dropped commit's parent tree.
- Tests in `unfinish.rs` (or a new integration test) need to
  rewrite expectations: previously asserted "2 commits ahead
  after finish+unfinish"; now should assert "0 commits
  ahead — branch tip is exactly the pre-finish sha."

## Open question

The user's framing: "Remove the finish action from it. If
it's not an empty commit it's meant to drop it." Two reads:

- (a) Always drop the finish commit; bail when it carries
  unrelated changes. (This plan's current shape.)
- (b) Remove just the rename from the commit; drop the commit
  only if it becomes empty.

(a) is simpler and matches `clank finish`'s "dedicated
commit" convention. (b) preserves other work but means
rewriting a single commit's tree, which is fiddlier. Codex
review or the user should pick before implementation starts.

## Out of scope

- Unfinishing a finish that isn't at HEAD. Needs an
  interactive rebase or filter-branch — separate plan if/when
  someone hits this.
- Restoring the original commit's metadata (author, ts) if
  unfinish gets bundled into a different flow later. Keep
  v1 mechanical.
