# unfinish-rewrites-history

`clank unfinish` currently lands a NEW commit that reverts the
plans/→finished/ rename. That bloats history — the original
finish commit + an inverse commit on top. The intent is to
rewrite history so the finish commit goes away, not stack a
reversal on top of it.

## Behavior

Drop the finish commit from history. ALL three preconditions
must hold before any rewrite begins; if any fails we bail
with a clear error and touch nothing.

- **Clean worktree + index.** No untracked, modified, or
  staged paths. `git reset --hard` would obliterate them
  otherwise. Probe via `git status --porcelain` (empty =
  clean). Error tells the user to commit or stash.
- **A finish commit for `<stem>` exists at HEAD.** Identified
  the same way `head_is_finalize_for` does: HEAD adds
  `.clank/finished/<stem>.md`. v1 only supports unfinishing
  the most recent finish — keeps the implementation small,
  matches `clank finish --amend`'s "you're acting on the
  freshest event" shape. Deeper finishes get a
  "finish for <stem> is not at HEAD" error.
- **HEAD's tree change is exactly the rename.** A trivial
  finish: removes `.clank/plans/<stem>.md`, adds
  `.clank/finished/<stem>.md`, content identical. Anything
  else and we bail — don't silently drop unrelated work.
  Probe via `git diff-tree --no-commit-id --name-status -r
  HEAD` and check the file set.

When all three hold, the rewrite is mechanical:

1. `git reset --hard HEAD~` — drops the finish commit
   entirely. The plan file's pre-finish path is restored as
   a side effect (HEAD~'s tree had it under `plans/`).
2. No new commit. The branch tip is now exactly where it
   was before `clank finish` ran.

User-facing log: `unfinished `<stem>`; dropped finalize commit
<sha>`.

## Why always drop, not "strip the rename"

Resolving the previously-open question: `clank finish` always
creates a dedicated commit whose only change is the rename.
Stripping just the rename and keeping a now-empty commit
would leave an artifact; rewriting the commit's tree to
preserve other content is fiddly and only useful if finish
ever bundled non-rename work — which it doesn't, and the
third precondition makes sure of it. So "drop the commit"
is both simple and lossless.

If someone hand-modifies a finish commit to carry other
changes, the precondition fires and they get a clear error
— better than guessing.

## Why

The current code makes `clank unfinish` look like a forward
edit. It's actually the inverse of a rewrite-style command:
finish moves files + commits; unfinish should undo both at the
git level. Otherwise reviewers see an ugly inverse pair every
time someone hits unfinish.

## Surfaces touched

- `crates/cli/src/cli/unfinish.rs` — drop the
  `git mv finished/<stem>.md plans/<stem>.md` + `git commit`
  pair. Replace with: clean-worktree check → validate HEAD
  is the trivial finish commit for `<stem>` → run
  `git reset --hard HEAD~`.
- Existing `unfinish.rs` tests rewrite their expectations:
  previously "2 commits ahead after finish+unfinish"; now
  "branch tip is exactly the pre-finish sha after
  finish+unfinish."

## Tests

- `unfinish_drops_finish_commit_and_restores_plan_file` —
  seed a plan, finalize, unfinish; assert HEAD sha equals
  the pre-finish sha and `.clank/plans/<stem>.md` is back
  in the worktree.
- `unfinish_refuses_when_worktree_dirty` — finalize, then
  introduce an unstaged edit on an unrelated file; run
  unfinish; assert it bails with a "clean / commit / stash"
  message and that HEAD did NOT move.
- `unfinish_refuses_when_index_dirty` — same shape but with
  staged changes via `git add` before unfinish.
- `unfinish_refuses_when_head_is_not_a_finish_commit` —
  finalize, then land an unrelated commit on top; run
  unfinish; assert "finish for <stem> is not at HEAD" error
  and no rewrite.
- `unfinish_refuses_when_finish_commit_has_unrelated_changes` —
  hand-craft a commit that adds `.clank/finished/<stem>.md`
  AND modifies another file; run unfinish; assert bail and
  no rewrite.

## Out of scope

- Unfinishing a finish that isn't at HEAD. Needs an
  interactive rebase or filter-branch — separate plan if/when
  someone hits this.
- Restoring the original commit's metadata (author, ts) if
  unfinish gets bundled into a different flow later. Keep
  v1 mechanical.
