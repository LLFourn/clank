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
- FIRST protects the current tip with a real git ref
  (`refs/clank/shelved/<plan>`) — GC-safe, see the design
  section below.
- Drops the plan's commits from the branch via the SAME rewrite
  path demote uses today (not a reset) — inheriting all its
  guards, including the foreign-commit refusal: a plan
  interleaved with other work cannot be shelved (same policy as
  demote today; `plan-reorder` is the future enabler for
  disentangling).
- The plan's commits stay reachable from the protective ref;
  shelve state (`.clank/shelved/<plan>.json`) records the
  ordered plan shas + the ref + optional `--for`.
- The plan file leaves the branch with the dropped intro commit;
  `clank status` shows the plan as **shelved**, listing what it
  was waiting on if `--for` was used (see below).

Optional flag: `--for <other-plan>` records the dependency
("shelved A is waiting on B"). Self-documenting and powers the
prompts described below.

### Unshelve

`clank unshelve <plan>` on a shelved plan:
- Cherry-picks the recorded plan commits (in order) from under
  the protective ref onto current HEAD.
- The plan file returns with the replayed intro; clank state
  notes the plan is live again; ref + shelve state are deleted
  only after the restore fully lands (fail-closed).
- **Reviews reset — by design (lloyd 2026-06-10)**: the
  cherry-picked commits are NEW shas in a NEW code context (the
  awaited work landed underneath), so prior verdicts don't
  carry. No feedback/state migration happens at all; the fold
  sees the new head, derives gate=Unreviewed, and the commit
  reviewers wake to re-review. This is a feature: nobody
  approved A-on-top-of-B. (It also deletes the would-be most
  complex machinery — git fires `post-rewrite` only on
  rebase/amend, never cherry-pick, so verdict preservation
  would have needed bespoke re-map wiring we now don't build.)

Unshelve nudge:
- `clank status` flags shelved plans whose `--for X` is now in
  finished_plans ("shelved for X — X finished"). No interactive
  finish-time prompt (finish stays non-interactive).

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
clank shelve list               # OPTIONAL/v2 — `clank status`
                                #   already shows shelved plans
clank shelve clean A            # discard a shelved plan permanently
```

## Relationship to `plan-reorder` (split out)

`shelve` is **workflow-time intent** — captured before the
interleaving exists. Cheap to use: a guarded drop of a clean
single-plan range + a protective ref (mechanically a rewrite,
but of the trivial contiguous kind).

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

## Git mechanics DESIGNED (ruthless c8f216b concerns folded)

The three concerns converge on ONE mechanism that answers all of
them — shelve routes through demote's rewrite engine, with a real
ref protecting everything:

### Shelve (all variants, incl. --to-queue)

1. **Protect first**: `git update-ref refs/clank/shelved/<plan>
   <current-tip>` — a REAL ref at the pre-shelve tip. Every one
   of the plan's commits is an ancestor of that tip, so all of
   them are reachable and GC-protected no matter how interleaved
   they are (concern 1: a sha in `.clank/` state protects
   nothing; `git gc` would eat the work shelve promises to keep).
   The rewrite engine already shells `git update-ref`
   (rewrite.rs:147/164) — same tool.
2. **Record** the plan's attributed commit shas IN ORDER in
   shelve state (`.clank/shelved/<plan>.json`: shas, the
   protective ref, optional `--for <plan>`, shelved-at).
3. **Drop via the rewrite engine** — the same
   `build_rewrite_preview` + `run_rewrite` path demote uses
   (demote.rs), NOT a reset. CORRECTION (codex 3a6b14f): demote
   does NOT handle interleaved plans — it unconditionally
   refuses foreign commits in the range (rewrite blockers, a
   deliberate safety from codex 625b8af). Shelve inherits the
   SAME refusal: shelving an interleaved plan errors cleanly
   (no narrowing — capability parity with demote — and no
   reset-style discarding of commits layered on top).
   `plan-reorder` (split-out sibling) is the future path to
   disentangle interleaved plans into shelvable blocks.
4. `--to-queue` additionally saves the plan body back to the
   queue (demote's existing body-save), removing the in-flight
   plan file; plain shelve marks the plan shelved (file moves to
   `.clank/shelved/`).

### Unshelve

1. Cherry-pick the recorded shas (in order) from under the
   protective ref onto current HEAD.
2. **No re-map — reviews reset (concern 2, RESOLVED by design
   decision, lloyd 2026-06-10)**: verdict preservation across
   unshelve is not wanted. The cherry-picked commits are new
   shas in a new context; old approvals don't apply, so nothing
   migrates. (Context on the machinery this avoids: git fires
   `post-rewrite` for rebase/amend only — never cherry-pick —
   and clank's own rewrites do their migration in-process, so
   preservation would have required bespoke old→new pair
   plumbing. With re-review semantics the gate machinery does
   everything: new head → Unreviewed → reviewers wake.)
3. On success: delete the protective ref + shelve state, restore
   the plan file to `.clank/plans/`.
4. Conflicts: stop like a normal cherry-pick; the protective ref
   and shelve state stay until the user completes or aborts
   (fail-closed — nothing is deleted until the restore fully
   lands).

### Minor notes (ruthless), decided

- `--for` surfacing: recorded in shelve state; `clank status`
  (and the TUI extras tier, later) flag "shelved for X — X
  finished" once X is in finished_plans. NO finish-time prompt in
  v1 (finish stays non-interactive; the status flag is the
  prompt).
- Blocks: shelving a plan with an active block DROPS the block,
  and unshelve does NOT restore it — conscious choice: the block
  conversation is stale by the time the plan returns; the user
  re-asks if still relevant. Documented in shelve's help.
