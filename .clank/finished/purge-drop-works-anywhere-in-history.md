# purge-drop-works-anywhere-in-history
# purge --drop works anywhere in history

## Problem

`clank purge <plan> --drop` only works when the plan's commits are the tip of the branch. Its range runs from the plan's intro commit to HEAD, and every commit in that range not attributed to the plan is "foreign". `drop_safety_check` (crates/cli/src/cli/purge.rs) refuses if any foreign commit exists:

> purge --drop refuses: foreign commit(s) interleaved in plan range: … Resolve via `git rebase -i` or coordinate with the foreign-commit author(s).

So a plan finished last week, followed by other plans' commits or the user's own, can't be dropped. Agents report this as "purge --drop only works on tip commits" and fall back to hand-written `git rebase -i`, which is what the command exists to replace.

The user's ruling: `purge --drop` must work on a plan anywhere in history and do what you mean. When dropping the plan's commits makes a later commit conflict, it stops in a rebase like `git rebase` does, for the operator to resolve and continue.

## What changes

`--drop` drops the plan's commits (code and `.clank/` artifacts) wherever they are, and keeps every other commit in the range, in order. Commits from other plans and the user's own commits are kept, not refused. Only `--drop` changes; ordinary purge, `--squash` and `clank stash` keep their current behaviour.

### One transformation, two executors

Both executors carry out the same program, built once from the preview (call it the drop program):

- **Base:** the parent of the plan's first commit, or the empty tree when that commit is the root.
- **Each plan commit:** dropped.
- **Each kept commit, in order:** its own parent-to-commit *change* is applied to the previous rewritten tree. It must not reuse its original full tree; that is what `rewrite::apply_plan` does for kept commits today, and with the safety check gone it would bring the dropped plan's code back. After applying, the dropped plan's artifact paths (its `.clank/` plan, finished and feedback files) are removed from the result. Other plans' artifacts and unrelated files are left as they are.
- **Every kept commit stays a commit.** That holds even if the preview classified it as artifact-only (`Drop`/`Rewrite`), if it was already empty, or if dropping the plan's artifacts leaves it empty. Nothing is skipped as "already applied" or "empty".
- **Metadata:** each kept commit keeps its message and its author name, email and date. The committer is the person running the command, at the time it runs, as with `git rebase`. Both executors do exactly this; the plan does not promise to keep committer dates.

The executors:

- **In-process (the default):** applies the program in the object database with `git_plumbing::replay_change`. Only a genuine replay *conflict* moves to the git executor. An I/O, object or permission error stays an error, and nothing moves. If nothing conflicts, the branch moves with the existing conditional ref update.
- **Git rebase (on conflict):** runs the same program as a real `git rebase --interactive` (through `git_plumbing`).
  - It starts from the base, or with `--root` when the plan's first commit is the root.
  - The generated todo has `drop` for plan commits and `pick` for kept ones. After each kept pick, an `exec` line removes the dropped plan's artifact paths from the index and worktree, then amends the commit (allowing it to be empty), so the result matches the in-process executor.
  - The run pins every git option that could change what the todo means: no autostash, no autosquash, and empty commits kept.
  - It runs the rebase until git stops at the conflict, in git's normal state. Clank prints which commit conflicted, on which paths, and that `git rebase --continue` finishes it (the remaining `exec` steps still run) or `git rebase --abort` restores the original branch exactly.
  - Before starting, it checks again that the branch and HEAD are the ones the preview was built from. It refuses if a rebase, cherry-pick or other sequencer operation is already in progress, or if the worktree is dirty.

### Other rules

- **A dirty working tree is refused before anything starts,** for both executors. This is a deliberate change for `--drop`: the shared engine currently allows some dirty rewrites that would leave the tree unchanged.
- **`--dry` runs the same assessment** (the in-process replay, built but not written to any ref), touching no ref, index or worktree and starting no rebase. It reports what would be dropped and kept, and the conflict it would stop at, if any.
- **`--into-branch`** works when nothing conflicts. If there would be a conflict it refuses, since a git rebase can't target a new branch, and says to run without it.
- **Merge commits in the range** still refuse.
- **A drop that would leave no commit at all** keeps its existing explicit refusal.

The post-rewrite hook already moves review feedback to rewritten shas, and a rebase the user finishes by hand goes through the same hook. The tests must show this, not assume it.

Sweep the belief: grep for "foreign" and "interleaved" in purge, rewrite, README and the skill/docs text. Remove every statement that `--drop` needs the plan at the tip. `clank stash push` shares `safety_check` and is out of scope, but make sure its wording doesn't now contradict purge.

## Out of scope

- `clank stash` and `--squash` foreign-commit handling.
- Dropping several plans in one run (`--drop --all` stays refused).

## Acceptance

In-process tests with real temporary git repos (no clank binary spawned):

- A plan whose commits are followed by another plan's commits and a user commit: `--drop` removes only the plan's commits and its `.clank/` artifacts. The later commits keep their messages, authors, author dates and order, and the final tree equals the old tree minus the plan's changes; no plan file reappears.
- Plan commits interleaved with others (A-plan, B, A-plan, B): only A's commits go, and B's keep their order.
- A kept commit that is empty, or that only touched `.clank/` for another plan: it is still in the result, as a commit.
- A plan whose first commit is the root: the clean path builds from the empty tree; the conflict path uses `--root`.
- Conflict: a later commit edits a line the plan introduced. The run stops in a git rebase at that commit, and the message names the commit and paths and says `--continue` / `--abort`. After the test resolves the conflict and runs `git rebase --continue`, the final tree has none of the dropped plan's artifacts, later commits and their metadata and order survive (including commits after the conflict, which the `exec` steps cleaned), and review feedback has moved to the new shas.
- `git rebase --abort` after a conflict restores the original HEAD exactly.
- A clean `--dry` with a conflict ahead reports the conflict and leaves refs, index and worktree unchanged, with no rebase in progress.
- Refusals, each changing nothing: a dirty tree; an existing rebase or sequencer operation; HEAD moved since the preview; `--into-branch` with a conflict.
- `--into-branch` without a conflict: the new branch has the rewritten chain, and the current branch is untouched.
- The plan at the tip, today's case, still works and gives the same result as before.
- Mutation checks:
  - restoring the foreign-commit refusal fails the first test;
  - reusing kept commits' original trees fails it too, because the plan's code comes back;
  - skipping the conflicting commit instead of stopping fails the conflict test;
  - removing the `exec` cleanup fails the continue-to-final-tree test.
