# Plan lifecycle verbs — shelve/unshelve + consolidate demote/purge-drop

Split from `plan-ordering` at promote time (2026-06-10): the
verb-consolidation decision gates the shelve design, and the
reorder engine is independent post-hoc cleanup (now its own
queue item `plan-reorder`).

## Why

Plans don't always land in the order they're authored. Two
patterns are common today and have no first-class support:

1. **Precursor mid-flight.** You're partway through plan A and
   realize plan B has to land first (a refactor A depends on, a
   bugfix that unblocks A's test path, an extraction A would have
   to do anyway). Today you either: keep A's commits sitting next
   to B's and accept interleaved history, or manually
   git-reset/rebase to set A aside. Both are clunky and lose the
   "I'm still working on A" signal.

2. **Wrong order in retrospect.** Two plans landed but the newer
   one should logically come first in history (a bugfix in
   plan B that retroactively fixes something in plan A;
   architecture cleanup that morally precedes the feature it
   enabled). Today the only way to fix this is manual interactive
   rebase, which loses clank's plan attribution and is risky on
   long histories.

Both patterns interact badly with `clank purge --squash`, which
refuses ranges containing foreign-plan commits. If plans aren't
neatly contiguous, the squash workflow doesn't apply.

## `clank shelve <plan>` / `clank unshelve <plan>`

**Mental model**: "set this plan's in-flight commits aside, get
me back to before they happened, remember to put them back later."

### Shelve

`clank shelve <plan>` on an in-flight plan:
- Notes the plan's first commit and current HEAD in clank state.
- Resets the branch back to before the plan's first commit.
- The plan's commits become unreachable from the branch BUT
  preserved (refs in clank state, recoverable).
- The plan's `.clank/plans/<plan>.md` file moves to a
  shelved-plans area (or stays put with a "shelved" marker —
  detail).
- `clank status` shows the plan as **shelved**, listing what it
  was waiting on if `--for` was used (see below).

Optional flag: `--for <other-plan>` records the dependency
("shelved A is waiting on B"). Self-documenting and powers the
prompts described below.

### Unshelve

`clank unshelve <plan>` on a shelved plan:
- Cherry-picks the plan's commits back on top of current HEAD.
- Restores the plan file from shelved to in-flight.
- Clank state notes the plan is live again.

Auto-prompted unshelve:
- When the plan recorded as `--for X` finishes, clank prompts:
  "Plan X just finished. Plan A was shelved for it — unshelve
  now?"
- `clank status` flags shelved-but-precondition-met plans.

### Edge cases the user should see handled

- Shelving a plan with **uncommitted work**: refuse (work would
  be lost) until the user commits or stashes.
- Shelving an **active reviewer block**: drop the block (it's
  scoped to in-flight state).
- Shelving with **conflicts** during unshelve cherry-pick:
  surface them like a normal rebase, leave the user to resolve.
- **Forgetting** to unshelve: `clank status` keeps showing
  shelved plans; explicit `clank shelve clean <plan>` to discard
  for good.

### Concrete invocation shapes

```
clank shelve A
clank shelve A --for B          # records the dependency
clank unshelve A                # explicit
clank shelve list               # show all shelved plans + reasons
clank shelve clean A            # discard a shelved plan permanently
```

## Relationship to `plan-reorder` (split out)

`shelve` is **workflow-time intent** — captured before the
interleaving exists. Cheap to use, no history rewrite needed
(reset + cherry-pick).

`reorder` is **post-hoc cleanup** — needed when the interleaving
already exists, either because shelve wasn't used or because two
plans were both legitimately in flight.

They overlap in result-space but solve different points in the
workflow. A user who consistently uses `shelve` rarely needs
`reorder`, and vice versa.

## Out of scope

- Multi-plan-at-once shelve/reorder (one at a time is fine).
- Auto-detection of plan dependencies (the user states `--for`
  explicitly if they want the dependency recorded).
- Stack-style PR-per-plan workflows. This plan is about the
  on-disk history; PR shape is independent.

## Verification

For shelve:
1. Start plan A, commit two things, run `clank shelve A` → HEAD
   moves back two commits, `clank status` shows A as shelved.
2. Run `clank queue promote B`, finish B end-to-end.
3. Run `clank unshelve A` → A's two commits cherry-pick on top,
   `clank status` shows A as in-flight again.

## Consolidate the delete / set-aside verbs (lloyd 2026-06-09)

When sizing this, reconcile the overlapping plan-lifecycle verbs
— we have too many and they blur together:

- `clank demote` — drop a plan's commits from history AND save
  the body back to the queue/stub (re-attempt later).
- `clank purge --drop` — drop a plan's commits AND its work from
  history, save nothing (true delete). Feedback survives because
  the `post-rewrite` hook migrates it through the rewrite.
- `clank shelve` (this plan) — set commits aside, restore later.

lloyd's steer: **we probably don't want both `demote` and
`shelve`** (they overlap — "set aside / come back to it"), and
**one verb should be able to FULLY delete a plan** (commits +
body + plan file), forward-cleanly, without the operator
hand-rolling `git rm` + block removal.

Concretely for the deletion case that prompted this: deleting a
plan that has no commits of its own (only a plan `.md` edited
under other plans' commits) was clunkier than it should be.
`clank purge --drop <plan>` was the right existing tool; a
first-class "delete this plan and everything about it" verb (or
making `purge --drop` the obvious documented path) should fall
out of this consolidation. Decide the final verb set:
keep/merge/rename demote, shelve, and purge --drop so there's
one obvious command per intent (set-aside-and-return vs
delete-for-good vs re-queue).

## Verb set DECIDED (lloyd 2026-06-10, promote time)

Option chosen: **shelve replaces demote; `purge --drop` stays the
documented full-delete; no new top-level verb.**

- `clank shelve <plan>` / `clank unshelve <plan>` — the set-aside
  verb (commits preserved + restorable, per the design above).
- `clank shelve <plan> --to-queue` — absorbs demote's behavior
  (set commits aside AND move the body back to the queue for
  re-attempt). After this lands, `clank demote` is REMOVED — its
  implementation (transactional drop + body-save, the
  post-rewrite feedback migration) becomes shelve's internals,
  not a parallel verb.
- `clank purge --drop <plan>` — stays THE full-delete (commits +
  body + plan file; feedback migrates via the post-rewrite hook).
  Document it as such everywhere the lifecycle verbs are
  explained (skills, README, help text) so nobody hand-rolls
  `git rm` again.
- Hard cut per house style: no deprecation alias for `demote`;
  it's unreleased.

Sizing should verify what of demote's existing transactional
machinery (drop-from-history + save-body, fail-closed
boundaries) is directly reusable as shelve's `--to-queue` path.
