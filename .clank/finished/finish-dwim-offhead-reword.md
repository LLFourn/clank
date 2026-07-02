# finish-dwim-offhead-reword
# `clank finish -m` DWIM: reword off-HEAD by rewriting history

## Problem / philosophy

`clank finish <plan> -m "..."` (and `--amend`) bail with
`require_head_is_finalize` (finish.rs:520) whenever the plan's finalize commit
isn't HEAD:

> "rewriting plan `<stem>`'s finish commit needs it to be HEAD, but HEAD is a
> different commit (work is stacked on top). Check out or rebase …"

This is exactly the hand-wringing lloyd wants gone. clank should adopt a
**git-branchless-style philosophy: aggressively rewrite history**, reserving
refusal for operations that lose data or can't be done linearly. The machinery
already exists and is already used by the `--squash`/`--purge` path:

- `preview.rs::build_rewrite_preview` walks first-parent from HEAD back to the
  plan intro — it already locates a plan's commits anywhere in linear history,
  not just at HEAD.
- `rewrite.rs::apply_plan` replays commits; `git_plumbing.rs::replay_commit`
  rebuilds a commit with a new tree/parent/message while preserving the author;
  `update_ref(ExpectedRef::Match(head))` moves the branch atomically.

Only the **message-rewrite path** refuses instead of using this. It's an
artificial gap.

This matters more now that **autosquash is the default**: every finished plan
collapses to a single commit, later plans stack on top, so rewording an
earlier plan's finish message is *inherently* an off-HEAD reword — the common
case, not an edge case.

## Core model

Rewording a commit = replace one commit's message, then replay its
first-parent descendants on top and move the branch ref. Model it as a
first-class operation in the rewrite engine (a `Reword` disposition, or a thin
wrapper over `apply_plan` that keeps every commit `KeepVerbatim` except the
target, which gets the new message). Do NOT hand-roll a bespoke rebase in
finish.rs — all rewrite logic stays in `rewrite.rs`/`git_plumbing.rs` per the
git-layer boundary.

## Scope

1. Relax `require_head_is_finalize` for `-m`/`--amend` message rewrites
   (finish.rs:87/105/138):
   - target commit **is** HEAD → fast path: `git commit --amend` (unchanged).
   - target **not** HEAD → route through the rewrite engine's reword-and-replay
     operation (new, in rewrite.rs).
2. Add the reword-and-replay operation to the rewrite engine, reusing
   `build_rewrite_preview`'s first-parent walk, `replay_commit`, and
   `update_ref`.
3. Protected branch: for finish's own reword, **imply**
   `allow_rewrite_protected` (consistent with the autosquash Part-2 decision we
   just shipped) — no `--allow-rewrite-protected` needed. The plan's commits
   are local; you reword before pushing.
4. Keep ONLY the refusals that prevent real harm / impossibility:
   - non-linear range (a merge commit among the descendants to replay),
   - dirty working tree,
   - target/intro not found in the first-parent walk.
   Everything else DWIMs.

## Files
- `crates/cli/src/cli/finish.rs` — relax `require_head_is_finalize`; route
  off-HEAD `-m`/`--amend` through the engine.
- `crates/cli/src/cli/rewrite.rs` — reword-and-replay operation;
  `collect_blockers` reused.
- `crates/cli/src/git_plumbing.rs` — `replay_commit`/`update_ref` already exist.
- `crates/cli/src/preview.rs` — first-parent walk already locates the range.

## Acceptance
- `clank finish <plan> -m "msg"` on a plan whose commit is buried under later
  commits rewrites just that commit's message and replays the stacked work on
  top; author dates preserved (via `replay_commit`); descendants intact; branch
  ref moved.
- Works on protected `master` without `--allow-rewrite-protected`.
- HEAD fast-path unchanged (still amends directly).
- Refuses only on: merge commit in the replay range, dirty tree, target not in
  first-parent history — each with a clear message.
- Tests (in-process cores; git fixtures OK): reword a finish commit with N
  later commits stacked → message changed, descendants intact, refs moved;
  reword at HEAD still fast-paths; merge-in-range refuses; dirty-tree refuses.

## Open questions — RESOLVED in implementation
- OQ1 (dirty tree): **refuse** ("working tree dirty; commit or stash first"),
  matching the existing rewrite engine. Auto-stash deferred.
- OQ2 (feedback migration): confirmed the engine rewrites via `update_ref`,
  which does NOT fire git's `post-rewrite` hook — so the reword explicitly
  migrates feedback by calling a new `rewire::migrate_feedback_pairs(repo,
  pairs)` (reuses `rewire`'s tested `plan_actions`/`copy_file`). `reword_in_place`
  returns the `(old,new)` sha pairs for the target + every replayed descendant;
  finish.rs feeds them to the migrator. No stranded feedback.
- OQ3 (finalize parity): a not-yet-finished `finish` finalizes at HEAD (HEAD is
  the tip), and retroactive `--squash`/`--purge` on an already-finished plan
  already DWIMs off-HEAD via the rewrite engine. So only the bare-`-m` **reword**
  path needed the relaxation.

## Implementation notes (as built)
- New engine op `rewrite::reword_in_place(RewordOpts)`: rebuilds the target with
  the new message (tree/parent/author preserved via `squash_commit`), replays
  each first-parent descendant with `replay_commit`, then moves the branch with a
  conditional `update_ref(Match(head))` + `reset_hard`. Returns `(old,new)` pairs.
- `git_io::commit_parent_count_at` added (path-based) for the merge check.
- ONE-COMPUTATION RULE (ruthless a2314e9): every history edit's `--dry` must
  print the SAME plan the live run applies — finish.rs owns NO preview text
  for a history edit. Converged: (a) bare-`-m` reword `--dry` threads into
  `reword_in_place(dry)` (no more hand-written "would rewrite" line, and the
  at-HEAD amend fast path is DELETED — one engine path regardless of position,
  which also migrates feedback for an at-HEAD reword, something the amend path
  never did); (b) retroactive already-finished `--squash`/`--purge` `--dry`
  routes into `run_post_finalize_rewrite` → `rewrite::run(dry)`. REMAINING
  GAP: the FRESH finalize-then-rewrite `--dry` (`dry_run_finish_composite`)
  still hand-rolls, because the finalize commit doesn't exist at preview time
  — needs a hypothetical-finalize preview. Tracked as the queued
  `finish-fresh-dry-through-engine` follow-up (per ruthless's "can ride or
  follow-up").
- SCOPE CHANGE (lloyd, after the first commit): **`clank finish --amend` is
  DROPPED entirely.** Bare `-m` subsumes its only real use (reword the finish
  message, now anywhere in history), and autosquash/`--squash` covers
  collapse-into-one — nobody could say what `--amend` was for. Dropping it also
  deletes `require_head_is_finalize` outright: the "needs it to be HEAD" error
  no longer exists anywhere in finish. `amend_already_finished` survives as the
  internal HEAD fast path for bare `-m`. No "flag is rejected" test added (the
  framework enforces absence); the kept paths — bare `-m` reword and fresh
  `--squash` refused-on-protected stamping the landing message — are tested.
- Autosquash gating: autosquash now applies only to a FRESH finish
  (`!already_finished`) so a bare `-m` on an already-finished plan reaches the
  reword path instead of being auto-converted to a retroactive squash. Explicit
  `--squash` still collapses retroactively.
