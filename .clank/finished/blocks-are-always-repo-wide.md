# blocks-are-always-repo-wide
# Delete plan-scoped blocks. A block halts the repo until the human answers.

## Problem

Reported by lloyd 2026-08-11, after watching the stop hook fire
several hundred identical times against a parked session:

```
  - blocked: claude/zellij-scope-reduction on zellij-pane-placement-and-cost (awaiting human)
  - promote: zellij-layout-reflow-and-placement-limits (priority 450)
```

The agent replied "unchanged, waiting on you" to every one. The
payload never changed, because nothing in it could change the repo.

Mechanically: a plan-scoped block suppresses its own plan
(`wait.rs:1005-1015`), which drops master's actionable plan count to
zero, which drops the initial pass into `queue_promote_outcome`
(`wait.rs:273-296`). That emits `PromoteFromQueue`, which is
non-`Blocked`, so `wake_worthy` (`wait.rs:1031`) returns true and the
hook fires. `wait` is level-triggered over repo state with no latch
(no dedupe anywhere in `stop_hook.rs`), so a payload the agent
declines re-fires verbatim, forever.

## The modeling error

A block means: *I cannot proceed until the human answers.* Scoping it
to one plan asserts the agent can meaningfully proceed somewhere else.
It cannot. clank's workflow forbids a second active plan, so the only
"other work" a plan-scoped block can surface is a queue promote — and
taking it requires stashing the blocked plan first. That makes the
surfaced item something the agent must either refuse (→ the loop) or
answer with an unrequested stash of the human's in-flight work.

The scope distinction encodes a capability the workflow does not have.
Every defect in this family descends from it:

- `wfw-surfaces-work-around-blocked-plans` exists solely to surface
  work "around" a block — work that cannot legitimately be taken.
- `wfw-block-suppresses-queue-promote` patches the same surface from
  the other side, so a block on a queued item hides its own promote.
- A plan-scoped block outlives its plan. Verified 2026-08-11: after
  `clank stash push zellij-pane-placement-and-cost`, the block still
  emitted, scoped to a plan no longer in the active set, with no
  discharge path (`block clean` only removes *answered* pairs).

Patching any one of these leaves the others. Removing the scope
removes all three.

## Goal

One kind of block: repo-wide. Creating one parks every agent until the
human answers. `wait` emits nothing wake-worthy while it stands —
which is already the verified behavior for `plan: None`
(`wait.rs:1010`, `result.suppress_all = true`).

## Approach

1. **`clank block create`**: delete `--plan` and `--all`. Scope is no
   longer a parameter; every block is repo-wide. `<NAME>` and `-m`
   remain.

2. **`BlockEntry.plan`**: delete the field. Callers that branch on it
   collapse to the unconditional path.

3. **Reader** (`block.rs:169-190`): keep the flat-file scan at `:172`;
   delete the per-plan subdir walk at `:174-182`.

4. **Thin the suppression machinery — do NOT delete the queue
   fallback.** Corrected after codex REQUEST_CHANGES on db1706e; the
   previous wording said to delete `queue_promote_outcome` and both
   call sites, which was wrong.

   Verified 2026-08-11 at `wait.rs:279` and `:462`: both sites gate on
   `actionable == 0`, and with nothing plan-suppressed that is simply
   "no active plans" — i.e. the ORDINARY idle repo. They are the
   normal queue-promotion path, not a block workaround. Deleting them
   removes `PromoteFromQueue` from clank entirely.

   Keep at both sites: the queue scan and promotion of the first
   queued item. Remove only:
   - `suppressed_plans` and the per-plan filter. `actionable`
     collapses to `fold.plans.is_empty()`; the
     `find(|q| !suppressed…)` scan collapses to `queue.first()`.
   - `Blocked` co-surfacing — drop the `block_items` parameter to
     `queue_promote_outcome`. With scope gone, a block sets
     `suppress_all` and short-circuits upstream of these sites, so
     there is never a block to co-surface here.

5. **`wake_worthy`** (`wait.rs:1031`) exists to reject `Blocked`-only
   payloads. Once `Blocked` is never co-surfaced beside queue items,
   every emitted item is actionable and the predicate may be
   trivially true. VERIFY against all emit paths before deleting — if
   any other path can still emit a `Blocked`-only vector, keep it and
   record why in a comment.

6. **`clank unblock` must flatten with the reader.** `UnblockArgs.plan`
   (`mod.rs:1620-1622`) and the write path (`block.rs:82-88`) still
   choose `unblocks/<plan>/` when `--plan` is passed.

   This is the sharpest edge in the change. If the reader goes
   flat-only (step 3) while `unblock` still honours `--plan`, an
   ACCEPTED answer is written where nothing reads it. The block never
   pairs, never clears, and under repo-wide semantics that parks every
   agent permanently with no recovery short of hand-editing `.clank/`.
   Steps 3 and 6 MUST land in the same commit.

   Delete `--plan` from `UnblockArgs`; always write
   `unblocks/<name>.md`.

7. **Status TUI** — DECIDED, not deferred. Three sites verified
   2026-08-11: `mod.rs:307-315` creates the pause with
   `plan: Some(stem)`; `mod.rs:342-362` `unblock_plan` filters
   `b.plan.as_deref() == Some(stem)` and blanket-answers every match
   with a canned string; `input.rs:225` toggles the Block/Unblock row
   off per-plan `st.blocked`, copy at `render.rs:1587`.

   **Ownership: `b` becomes a GLOBAL key, off the plan row.** Repo
   pause is repo state; rendering it as a per-plan toggle is the same
   category error this plan removes. Remove Block/Unblock from
   `PlanAction` and the plan-row hotkey table entirely.

   **Availability**: `b` is live whenever the TUI is, including with
   zero active plans and an empty queue. That case is precisely when
   pausing matters (the repo is idle and the human wants it to stay
   that way), and today it is unreachable because there is no row to
   select.

   **`b` with no pending block** → prompt for question text → create
   ONE repo-wide block, master-authored, fixed name `pause`. Fixed
   name keeps today's idempotent re-block property, and repo-wide
   scope means there is exactly one, so pause/resume stays 1:1 with
   no selection ambiguity.

   **`b` with the `pause` block pending** → clear that block only.

   **Agent-authored blocks are NOT answered by `b`.** They are
   answered from the blocks section, which becomes navigable: select a
   block, `u` opens an input, the typed message answers THAT block.

   This deliberately kills the blanket canned answer at
   `mod.rs:342-362`. Repo-wide, that loop would write one string
   ("unblocked from clank status --tui") under several agents'
   distinct questions at once — the fabricated-answer failure, applied
   in bulk. One question, one typed answer, one pair.

   **Multiple pending blocks**: the repo stays parked until every one
   is answered. Answering the selected block clears only that pair;
   `suppress_all` still holds for the rest. The blocks section already
   lists them, so "what is still holding the repo" is legible without
   new surface.

8. **On-disk migration**: `blocks/<plan>/<name>.md` →
   `blocks/<name>.md`, same for `unblocks/`.

   **A question and its answer migrate as ONE unit, and the whole repo
   is preflighted before anything moves.** Walking `blocks/` and
   `unblocks/` as independent passes lets a collision refuse the
   question while the answer still moves — which marks an unrelated
   flat question answered and splits the scoped pair. That is the
   fabricated-answer failure this plan exists to remove, reintroduced
   by the migration itself (codex caught it on 435a6e3).

   The destination pair must be entirely free — on disk AND among the
   moves already planned. A flat block or answer of the same name is a
   conflict, because a pre-existing flat answer would attach to the
   question the moment it lands. Checking only the filesystem is not
   enough: two active plans each holding `q.md` both see no flat
   `q.md`, both reserve the same destination, and the apply loop
   overwrites the first with the second (codex caught this on 622b67c
   and again on e59a43f). Every planned destination is therefore
   reserved as the plan is built, and a second source claiming a
   reserved name is a conflict. Any conflict aborts the run with
   NOTHING changed, so a partially-migrated repo is never observable.

   Measured on this repo 2026-08-11: 18 `blocks/<plan>/` subdirs
   survive, but 17 are EMPTY (leftover dirs from cleaned pairs).
   Exactly one live plan-scoped block exists —
   `claude/zellij-pane-placement-and-cost/zellij-scope-reduction`,
   and it is moot (its plan was stashed; its question was overtaken
   by the human's decision to set the plan aside).

   So a naive flatten migrates one stale question into a REPO-WIDE
   block and parks every agent on it — the exact failure this plan
   is meant to prevent, shipped by the fix itself. The migration MUST
   therefore:
   - flatten only blocks whose scoped plan is still active,
   - drop (or refuse to migrate, reporting them) blocks whose scoped
     plan is absent from the active set — stashed, purged, finished,
   - remove the empty `blocks/<plan>/` and `unblocks/<plan>/` dirs.

   Run against live state with the freshly-built binary BEFORE
   `cargo install`, and re-run `clank wait --peek` afterward to
   confirm it returns `{"items":[]}` and not a parked repo.

## Required tests

Both must be in-process library tests (no binary spawning):

- **Idle unblocked queue wakes**: no active plan, no block, non-empty
  queue → wake carrying `PromoteFromQueue` for the first item. Assert
  on BOTH the initial pass and the watch-loop pass; the two gates are
  separate code and have drifted apart before.
- **Repo-wide block suppresses that same queue**: identical fixture
  plus one block → parked, no wake, no `PromoteFromQueue`.
- **`b` is global and reachable with no active plan**: fixture with
  zero plans and an empty queue → `b` is dispatched and creates the
  repo-wide `pause`. Guards the availability rule directly; today this
  case has no row and so no key.
- **`b` is gone from the plan row**: `PlanAction` no longer yields
  Block/Unblock, and the plan-row hotkey table does not bind `b`.
  Replaces the existing per-plan assertions at `input.rs:1425` and
  `:1463`, which pin the behavior being removed and must be deleted
  rather than adapted.
- **Migration never splits a pair**: flat `q` plus a scoped `q`
  question+answer pair → the run fails, the answer does NOT land flat,
  and the untouched flat `q` still reads as unanswered. The
  independent-passes bug passed every single-sided test, so the
  regression must exercise both halves at once.
- **Two active plans, one flat name**: two legacy plan dirs for one
  agent both holding `q.md`, no flat `q.md` on disk → the run fails and
  both scoped pairs survive intact. A filesystem-only preflight passes
  this fixture and destroys a question, so the test must assert on
  file CONTENT, not just on the error.
- **Pause round-trips**: `b` with no block creates exactly one
  `pause`; `b` again clears exactly that pair; the repo is unparked
  only after it.
- **Selected-block answering, not blanket**: two pending
  agent-authored blocks from different agents → answer the selected
  one → exactly that pair is written, the other stays pending, and
  `wait` still parks. Asserts no path writes one message across
  distinct blocks.

- **Every unblock path writes the flat answer**: for CLI
  `clank unblock` AND the TUI unblock action, assert the written
  answer is observed by the reader and the block reports answered.
  This is the guard against the step 3/7 split-landing failure.

## Consequence to accept

An unanswered block now parks every agent silently — `wait` returns
nothing rather than spinning. That is the correct failure mode (quiet,
not a livelock), but it means a forgotten block halts the repo with no
wake to remind anyone. `clank status` already lists open blocks
prominently; confirm that is enough before shipping. A `clank block
retract` for withdrawing a moot question is the obvious follow-up and
is OUT of scope here.

## Reverses

- `block-create-explicit-scope` — the scope flags it added are deleted.
- `wfw-block-suppresses-queue-promote` — its surface is deleted.
- `wfw-surfaces-work-around-blocked-plans` — its surface is deleted.

Deliberate. Each solved a real symptom of the scope model; none could
solve the model.

## Out of scope

- `clank block retract`.
- Any latch/edge-trigger in `wait` or the stop hook. With
  `suppress_all` unconditional there is no repeating payload to latch.
- Reviewer-role block semantics beyond what falls out of `suppress_all`.
