# Plan lifecycle verbs — `clank shelve`/`unshelve`; `demote` removed

One obvious verb per intent:

| intent                      | verb                          |
|-----------------------------|-------------------------------|
| set aside, come back later  | `clank shelve` / `unshelve`   |
| set aside AND re-queue body | `clank shelve --to-queue`     |
| delete for good             | `clank purge --drop` (stays)  |

`clank demote` is REMOVED — hard cut, no alias (unreleased
surface). Its transactional machinery (guarded drop + body-save)
becomes shelve's internals, not a parallel verb. `purge --drop`
becomes THE documented full-delete everywhere lifecycle verbs are
explained (skills, README, help text) so nobody hand-rolls
`git rm` again.

(Verb set decided by lloyd 2026-06-10 at promote time. The
post-hoc history-reordering tool split out to the `plan-reorder`
queue item.)

## Why

**Precursor mid-flight** is the motivating pattern: partway
through plan A you realize plan B must land first (a refactor A
depends on, a bugfix unblocking A's tests). Today you either
accept interleaved history or hand-roll git-reset/rebase, losing
the "I'm still working on A" signal. Shelve captures the intent:
*set A's commits aside, get me back to before they happened,
remember to put them back later.*

The verb sprawl is the second problem: demote ("drop + re-queue")
and a would-be shelve ("set aside / return") overlap, and no verb
obviously means "fully delete this plan."

## Design

### Shelve

`clank shelve <plan>` on an in-flight plan:

1. **Protect first**: `git update-ref refs/clank/shelved/<plan>
   <current-tip>` — a REAL ref at the pre-shelve tip. Every plan
   commit is an ancestor, so all are reachable and GC-protected.
   (A sha recorded in `.clank/` state protects nothing — `git gc`
   prunes unreachable commits; that would silently eat the work
   shelve promises to keep. Ruthless c8f216b concern 1.)
2. **Record** shelve state at `.clank/shelved/<plan>.json`: the
   plan's attributed commit shas in order, the protective ref,
   optional `--for <plan>`, shelved-at timestamp.
3. **Drop via the rewrite engine** — the same
   `build_rewrite_preview` + rewrite path demote uses today, NOT
   a reset. All its guards inherit, including the
   **foreign-commit refusal**: a plan interleaved with other work
   cannot be shelved (identical policy to demote — codex 3a6b14f
   confirmed demote refuses foreign commits in range, a
   deliberate safety from codex 625b8af; there is no
   non-contiguous capability to preserve). Interleaved plans
   error cleanly; `plan-reorder` is the future enabler. The plan
   file leaves the branch with its dropped intro commit.
4. `--to-queue` additionally saves the plan body back to
   `.clank/queue/` (demote's existing body-save) — "set aside and
   re-attempt from scratch later" rather than "restore exactly".

`clank status` (and later the TUI extras tier) shows shelved
plans, with "shelved for X — X finished" once a recorded `--for X`
appears in finished_plans. That status flag IS the unshelve
nudge; finish stays non-interactive (no prompt).

### Unshelve

`clank unshelve <plan>`:

1. Cherry-picks the recorded shas, in order, from under the
   protective ref onto current HEAD. Conflicts surface like any
   cherry-pick; the user resolves or aborts.
2. **Reviews reset — by design (lloyd 2026-06-10)**: the
   replayed commits are NEW shas in a NEW code context (the
   awaited work landed underneath). Prior verdicts don't carry —
   nobody approved A-on-top-of-B. NOTHING migrates: the fold
   sees the new head, derives gate=Unreviewed, and the commit
   reviewers wake to re-review automatically. The re-review
   trigger falls out of the existing gate machinery.

   Why this also deletes complexity: hooks are a PORCELAIN
   feature — git fires `post-rewrite` for rebase/amend only,
   never cherry-pick, and clank's own surgery (gix reads +
   plumbing writes: commit-tree/update-ref) never fires hooks at
   all, which is why demote does its migration writes in-process
   ("step 5"). Verdict preservation would have required bespoke
   old→new re-map plumbing; with reviews resetting, none of it
   exists.
3. **Fail-closed cleanup**: the protective ref + shelve state are
   deleted only after the restore fully lands (plan file back in
   `.clank/plans/`, branch updated). An aborted/conflicted
   unshelve leaves everything recoverable.

### Edge cases

- **Uncommitted work** at shelve time: refuse (the rewrite
  engine's dirty-tree blocker already does this).
- **Protected branch**: the rewrite engine's existing refusal +
  `--allow-rewrite-protected` override apply unchanged.
- **Active block on the shelved plan**: dropped, and NOT restored
  on unshelve — conscious choice: the block conversation is stale
  by the time the plan returns; re-ask if still relevant.
  Documented in shelve's help.
- **Forgetting to unshelve**: `clank status` keeps showing
  shelved plans; `clank shelve clean <plan>` discards one for
  good (deletes ref + state — the only way shelved work is ever
  deleted).
- **Old feedback files** for the dropped shas become inert (the
  fold only reads reviews for shas in the live timeline); they
  are not migrated or deleted.

### Invocations

```
clank shelve A
clank shelve A --for B          # records the dependency (status nudge)
clank shelve A --to-queue       # absorbs demote: body -> queue
clank unshelve A
clank shelve clean A            # discard shelved work permanently
clank shelve list               # OPTIONAL/v2 — status already shows shelved
```

## Surfaces

- `crates/cli/src/cli/shelve.rs` (new) — shelve/unshelve/clean
  cores + state file; reuses the rewrite-drop path and demote's
  body-save.
- `crates/cli/src/cli/demote.rs` — REMOVED; reusable pieces move
  to shelve.
- `cli/mod.rs` — `Shelve`/`Unshelve` args; `Demote` removed.
- `status.rs` / `status_tui.rs` — surface shelved plans (+ the
  `--for` nudge) from `.clank/shelved/`.
- Skills + README + RELEASE-CHECKLIST cross-refs: document the
  three-verb set; `purge --drop` named as the delete.
- Tests: in-process only (the cores; no binary spawning) —
  shelve protects-then-drops (ref exists, commits reachable,
  branch clean of plan); interleaved plan refused; unshelve
  restores + gate goes Unreviewed on the new head (reviews
  reset); conflicted unshelve leaves ref+state intact;
  `--to-queue` lands the body back in the queue; `shelve clean`
  removes ref+state; demote gone from the CLI.

## Out of scope

- `plan-reorder` (split-out sibling): rewriting history to
  disentangle/reorder interleaved plans.
- Multi-plan-at-once shelve.
- Auto-detected dependencies (`--for` is explicit).
- Restoring review state across unshelve (explicitly rejected —
  reviews reset by design).

## Verification

1. Start plan A, commit twice, `clank shelve A` → branch has no A
   commits, `refs/clank/shelved/A` exists, status shows A
   shelved.
2. Promote + finish plan B end-to-end; status shows "A shelved
   for B — B finished" (when `--for B` was used).
3. `clank unshelve A` → A's commits replayed on top, plan file
   back, gate Unreviewed on the new head, reviewers wake; ref +
   shelve state gone.
4. `git gc` between shelve and unshelve does not lose A's work.
5. Shelving a plan interleaved with foreign commits refuses with
   a clear error.
6. `clank demote` no longer exists; `clank purge --drop` is
   documented as the delete.
