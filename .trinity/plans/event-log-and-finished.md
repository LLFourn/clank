# Event-Log Projection and Finished Rule

(Formerly drafted as "Approval-Derived Completion"; renamed when
CLI scope split off to `trinity-cli` and the event-log sans-io
restructure landed as Phase 2.)

## Summary

Delete the `.trinity/plans/done/` directory workflow. A plan cycle
becomes `finished` when reviewer-approval feedback is snapshotted
into a git-tracked `.trinity/finished/` directory by a deliberate
**finalize commit**, not when an agent moves a plan file. Plan files
stay at `.trinity/plans/<stem>.md` for their lifetime.

The core design problem this plan addresses: today plans are
committed to git but feedback lives in the working tree, so a fresh
`git clone` cannot see which plans are finished. The current `done/`
directory IS that signal, but it's one master can forge unilaterally
(just `git mv` the file). The new model puts the finish signal in
git via a deliberate **finalize commit** made by the `trinity finish`
CLI command after the CLI verifies the live gate. The CLI never
cheats — invariant checks pass before the commit lands. A
semi-malicious or confused master agent can still forge a finalize
commit by writing fake APPROVE files directly, but at that point
the operator has bigger problems than this artifact. Trinity is not
trying to be a tamper-proof attestation system, only to make
resolution require explicit, visible action instead of a silent
`git mv`.

Scope this plan owns:

- Snapshot-on-finalize: `.trinity/finished/<stem>/` carries a
  committed copy of the reviewer feedback files at finish time. No
  per-SHA subdirectory — there is one snapshot per plan in HEAD,
  and each new `trinity finish` overwrites it. History is preserved
  by git itself (the commit log of `.trinity/finished/<stem>/`).
- The daemon's finished rule is: a plan file exists at
  `.trinity/plans/<stem>.md` AND `.trinity/finished/<stem>/`
  contains ≥1 file AND every file in that directory has its first
  line starting with `APPROVE`, all in HEAD's tree. That is the
  entire rule. No ordering constraints, no impl-commit requirement,
  no checks on which commits introduced which file. The plan and
  the finished entry can be added in the same commit, in different
  commits, in any order. The daemon doesn't care.
- All sophisticated validation (gate is genuinely approved, working
  tree is clean, reviewer files match real reviewer authors, etc.)
  lives in the `trinity finish` CLI — at mutation time, before
  the commit lands. Nothing in the daemon's read-time projection.
- Deletion of `PlanState::Done`, `CommitKind::DoneMove`,
  `PlanTouchKind::DoneMove`, the `done_move_pending` worktree
  status, the `api_move_to_done` HTTP endpoint, and the SPA's
  "Move plan to done/" button.
- Feedback-path simplification: drop the redundant `commits/`
  segment. Working-tree feedback becomes
  `.trinity/feedback/<stem>/<sha>/<agent>.md` (`<sha>` stays because
  the working-tree gate is keyed per-impl-commit).
- New CLI commands `trinity init` (with `--ignore` to delegate
  to the repo-root `.gitignore` instead of creating
  `.trinity/.gitignore`) and `trinity finish` (with `--purge`,
  `--squash`, `--amend` modes) that own the ceremony.
- New CLI command `trinity purge` for stripping a plan's
  `.trinity/` content from history without a finalize commit
  (abandoned plans, post-fact cleanup). Shares the
  history-rewriting engine with `trinity finish --purge`.
- The daemon does not write `finished/` files; the CLI does, under
  the operator's hand.

The `Phase` enum deletion deferred by `architecture-tech-debt-sweep`
(waiting on `PlanState::Done` removal) lands here as Phase 7.

Source design rationale lives in
`.trinity/stubs/event-log-and-finished.md` — this plan does
not duplicate the rationale, only the implementation.

## Definitions

These need to be unambiguous before the implementation can land
cleanly. They are not in the stub; they are this plan's contribution.

### Plan cycle (display-only)

A **plan cycle** is a historical range of plan-attributed commits
between two consecutive finalize commits (or between the start of
the plan's history and its first finalize commit). Cycles do not
drive any projection state — they're a UI/CLI concept recovered by
walking `git log .trinity/finished/<plan>/`. The daemon's finished
rule (below) doesn't reference cycles.

### Latest reviewable commit (CLI policy)

The most recent commit in `commit_order` whose `CommitKind` is
`PlanOnly`, `CodeOnly`, or `Mixed` for this plan. Used by `trinity
finish` at mutation time to decide which feedback directory to
snapshot. NOT used by the daemon's finished-ness derivation.

### Current gate participant (CLI policy)

Anyone who has written a feedback file under
`.trinity/feedback/<plan-stem>/<target-sha>/` (note: post-simplification
path; the legacy `commits/` segment is gone). Used by the live-gate
view (current working-tree review status) and by `trinity finish`'s
pre-commit invariant check. NOT used by the daemon's
finished-ness derivation.

### Finalize snapshot

A `.trinity/finished/<plan-stem>/` directory in HEAD's tree
containing reviewer feedback files (one per approving reviewer,
verdict + body copied from the working-tree feedback for the
latest impl commit at finalize time). Flat — no per-SHA
subdirectory. The presence of this directory is the only
git-visible signal that a plan cycle was deliberately closed.

The snapshot does not replace the working-tree feedback files; both
coexist. The working-tree files remain the live, async surface;
the snapshot is the archival commitment at the close.

There is one snapshot per plan in HEAD at any time. Each new
finalize overwrites the prior contents. The history of past
finalizes is preserved by git itself — `git log
.trinity/finished/<stem>/` enumerates every finalize commit, and
`git show <sha>:.trinity/finished/<stem>/` recovers any archived
cycle's snapshot.

### Finalize commit

A commit that creates or modifies `.trinity/finished/<plan-stem>/`.
Made by `trinity finish`, under the operator's hand. The finalize
commit is a normal commit: it shows up in the timeline, reviewers
can still post feedback on it. But late feedback does *not*
unfinish the cycle — the snapshot is authoritative once landed.

Trust model: `trinity finish` runs the live-gate invariant check
locally before making the commit, so an honest run never produces
a forged snapshot. A semi-malicious or confused master that
bypasses the CLI and writes fake APPROVE files into `finished/`
directly is out of scope for the trust model. Late reviewers who
notice the discrepancy can flag it via the normal feedback
mechanism, but Trinity does not invalidate the snapshot
automatically.

### Finished plan

A plan is **finished** iff, in HEAD's tree:

1. `.trinity/plans/<stem>.md` exists, and
2. `.trinity/finished/<stem>/` contains at least one file AND
   every file in that directory has its first line starting with
   `APPROVE`.

That's the entire rule. No requirement that an impl commit exists,
no requirement that the snapshot's authors match any prior
`.trinity/feedback/` reviewer set, no content inspection beyond
the first line of each file.

Implementation note (weak preference, pick whichever is easier in
the actual code): the natural HEAD-tree check is "both files
present, all snapshot files start with APPROVE." If the rebuild
path is more naturally framed as event-log mutation (commit X adds
the plan → active; commit Y adds the snapshot → finished), that
yields the same answer in practice with a "finish commit applies
to the plan if the plan already existed at that point in history"
rule. Both produce identical results on any sequence of commits
that landed in chronological order via `trinity finish` — which is
the only way the CLI ever produces a finish commit.

The first-line APPROVE check is the only content inspection: file
bodies after the first line are not parsed, file names don't
matter beyond being readable, file count doesn't matter beyond
non-zero. A directory with any non-APPROVE file present (e.g., a
stray REQUEST_CHANGES dropped in) makes the plan NOT finished —
the rule is "every file is APPROVE," not "any file is APPROVE."
That keeps the operator from accidentally finishing a plan by
leaving a contradictory file in the snapshot dir.

If `.trinity/plans/<stem>.md` is absent, the plan doesn't exist
(no `active`, no `finished`). If the plans file is present but
`.trinity/finished/<stem>/` is absent or contains no valid APPROVE
file, the plan is `active`.

To reverse a finished state: remove `.trinity/finished/<stem>/`
from HEAD (via `trinity purge`, `git rm` + commit, or
`git revert <finalize-sha>`). The daemon reads HEAD and answers
"active" again.

Per-plan scope: every check is keyed on `<stem>`. A finalize for
`foo` says nothing about `bar`.

All sophisticated validation — the live working-tree gate is
genuinely approved, the snapshot authors are real reviewers, the
working tree is clean — lives in `trinity finish` at mutation
time. The daemon does none of it at read time.

### Archived cycle

A cycle whose closer (finalize commit) is not the most recent
finalize commit for the plan. Archived cycles are recovered by
walking the commit log for changes to `.trinity/finished/<plan>/`
and reading each historical revision via `git show`. They appear
in the cycle-history view but do not drive any waiting/gate state.

### Plan lifecycle state (wire)

The wire `state` field, currently `active | done`, becomes one of:

- `active` — no finalize snapshot exists for this plan in HEAD
- `finished` — finalize snapshot exists with at least one APPROVE

`finished` is sticky from the daemon's perspective: once the
snapshot is in HEAD, the plan stays `finished` until the snapshot
is replaced (re-running `trinity finish`) or removed
(`trinity purge` or manual deletion). New plan-attributed commits
after a finalize do NOT auto-transition the plan back to `active`.
The CLI is what catches "the snapshot is now stale relative to new
work" — at the operator's request when they next run `trinity
finish` — not the daemon.

`archived` is **not** a plan-level state. Archived cycles are an
artifact of git history (`git log .trinity/finished/<stem>/`)
surfaced in the cycle-history UI view; they don't appear in the
plan-level `state` wire field.

## Goals

- Lifecycle completion derives from a git-tracked resolution
  snapshot, not from filesystem layout (`done/`) or from
  daemon-only working-tree state.
- A fresh `git clone` can see which plans are finished and who
  approved them, with no working-tree state required.
- `done/` directory has no role in Trinity's workflow.
- `PlanState::Done`, `CommitKind::DoneMove`, `PlanTouchKind::DoneMove`,
  `WaitingReason::{SessionDone, CommitDoneMove, RestoreOrCommitDoneMove}`,
  `PlanWorktreeStatus::DoneMovePending` all deleted.
- `api_move_to_done` HTTP endpoint deleted; SPA "Move plan to done/"
  button deleted.
- Feedback path simplified: `.trinity/feedback/<stem>/<sha>/<agent>.md`
  (no `commits/` segment). The `<sha>` stays in the feedback path
  because the working-tree gate is keyed per-impl-commit. The
  finalize snapshot has a flatter shape —
  `.trinity/finished/<stem>/<agent>.md` — because at finalize
  time there is one current impl, and old finalizes are recovered
  via git history rather than parallel SHA directories in HEAD.
- The CLI tooling (`trinity init`, `trinity finish`,
  `trinity purge`, the history-rewriting engine) is out of scope
  for this plan and owned by `.trinity/stubs/trinity-cli.md`. The
  CLI depends on this plan landing first (finalize-snapshot reader,
  event-log sans-io fold).
- One-time migration path for repos with existing
  `.trinity/plans/done/*.md` files that doesn't lose history.
- One-time migration for in-flight working-tree feedback at the
  old `commits/<sha>/<agent>.md` path.

## Non-Goals

- Renaming `wait_for_work` or its alias work (owned by
  `wfw-alias-and-post-commit-driver.md`).
- Concurrent-review-cycles wake/projection (owned by
  `concurrent-review-cycles.md`).
- Typed wire contracts (owned by `typed-wire-contracts.md`).

## Daemon Responsibilities (CLI in a separate stub)

The CLI tooling (`trinity init`, `trinity finish`, `trinity purge`)
lives in `.trinity/stubs/trinity-cli.md`. Two daemon-side
consequences of that split are in scope here:

- **The daemon never writes Trinity artifacts.** No new
  state-mutating endpoints; the existing `api_move_to_done`
  violation goes away in Phase 5 (Wire/UI cutover). The CLI does
  all git mutations locally; the daemon's watcher picks up changes
  on the next filesystem signal.
- **New HTTP read endpoints expose what the CLI will need.** This
  plan adds at least `GET /api/plan/<id>/finish-preview` (would
  `trinity finish` succeed right now? if not, what's missing?) so
  the CLI doesn't re-derive projection state from disk. Exact
  endpoint surface is sized when the stub is implemented; this plan
  commits to "no parallel projection logic in the CLI" as the
  invariant.

## Touchpoints (audit)

Files and line ranges that need to change. Counts from current
master.

**Core domain**:
- `src/repo_state.rs:341-362` — `PlanState` enum + `from_plan_path`.
- `src/repo_state.rs:393-417` — `PlanWorktreeStatus::DoneMovePending`.
- `src/repo_state.rs:420-435` — `Phase::Done` variant (deferred —
  follow-up).
- `src/repo_state.rs:457-468` — `PlanTouchKind::DoneMove`.
- `src/repo_state.rs:479-516` — `CommitKind::DoneMove`,
  `is_reviewable` predicate.
- `src/repo_state.rs:574` — `Plan.state` field type.
- `src/repo_state.rs:658-664` — `WaitingReason::{SessionDone,
  CommitDoneMove, RestoreOrCommitDoneMove}` variants.
- `src/lifecycle.rs:287-323` — `PlanKey::from_path` (drop the
  `done/<stem>.md` alternative).
- `src/lifecycle.rs:326-365` — `is_done_plan_path` and
  `plan_path_counterpart` deletions.
- `src/projection.rs:35-40` — `done_counterpart_exists` parameter +
  `DoneMovePending` branch.
- `src/projection.rs:42-73` — `phase` and `phase_for` (touch when
  `Phase::Done` deletion lands).
- `src/projection.rs:80-113` — `waiting_on` `is_done`,
  `DoneMovePending`, `MissingActivePlanFile` branches.
- `src/projection.rs:258-292` — `plan_path_at` (collapses to one
  return value per plan, since plan path doesn't change).
- `src/projection.rs:540-605` — `commit_kind_for` `DoneMove` branch.

**Disk surface**:
- `src/disk_snapshot.rs:60-120` — `PlanFile` construction;
  `plan_state` and `plan_path` handling.
- `src/disk_snapshot.rs:380-410` — `done_move` plan-touch
  classification.
- `src/git_io.rs:380-400, 700-720` — rename detection that produces
  `PlanTouchKind::DoneMove`.
- `src/git_io.rs:610-660` — `is_plan_path` accepting the `done/`
  variant.
- `src/fs_watcher.rs:55-95, 175-200` — recognition of
  `.trinity/plans/done/<id>.md` paths and `DoneMovePending`
  detection.

**Server / wire**:
- `src/server/http.rs:31, 441-487` — `api_move_to_done` route +
  handler.
- `src/server/wait.rs:435-475` — `derive_locations` branches for
  `CommitDoneMove`, `RestoreOrCommitDoneMove`, `SessionDone`.
- `src/server/wait.rs` (build_action) — `WorkAction` variants for
  these reasons.
- `src/mcp_response.rs:90-100, 190-200` — `is_done` predicate sites.
- `src/ui_response.rs:90-105, 180-200` — same.

**Disk format / paths**:
- `src/disk_format.rs` — `parse_feedback_path` (drop `commits/`
  segment), `parse_finalize_path` (new — for
  `.trinity/finished/<stem>/<agent>.md`), feedback-path canonical
  string builder (new), finalize-path canonical string builder
  (new).

**New core module**:
- `src/finalize.rs` (or similar) — parser for
  `.trinity/finished/<stem>/<agent>.md` paths (flat, no SHA
  segment) and the `is_finished(&Plan)` check (plan file present
  + at least one well-formed APPROVE in the snapshot dir). No
  commit-ordering logic; finished-ness is a pure read of HEAD's
  tree.

**Frontend**:
- `frontend/src/api.rs:281-299` — `post_move_to_done` +
  `DoneResponse` deleted.
- `frontend/src/components/meta_strip.rs:80-130` — "Move plan to
  done/" button deleted.
- `frontend/src/components/home.rs` — state-chip values
  (`active`/`done` → `active`/`finished`).

**Tests**:
- `tests/end_to_end.rs` — `plan_id_url_stable_across_done_flip`,
  `move_to_done` flows, anything asserting `state: "done"`. New
  tests for `finished/` snapshot rendering and
  snapshot-overrides-live-feedback behaviour.
- `src/disk_snapshot.rs` unit tests — `done_move` rename cases,
  `done_counterpart_exists` cases; new finalize-snapshot ingestion;
  the event-log fold regression suite (all existing tests pass
  unchanged plus monotone-freeze + split-fold equivalence cases
  per Phase 2).
- `src/projection.rs` unit tests — `plan_path_at_*` cases,
  `waiting_*_done_move`, `commit_kind_for` done-move case; new
  per-commit attribution tests that assert frozen plans ignore
  subsequent mutations.
- CLI tests (`tests/cli_*`) are owned by the trinity-cli stub.

## Phases

Recommended order. Each phase compiles and tests pass; no half-done
states between phases.

### Phase 1 — Feedback-path simplification

Drop the `commits/` segment from feedback paths everywhere.

- `disk_format::parse_feedback_path` matches
  `<plan-stem>/<sha>/<agent>.md` instead of
  `<plan-stem>/commits/<sha>/<agent>.md`.
- `disk_format::canonical_feedback_path` (introduce if not present)
  emits the new shape.
- `fs_watcher` recognises the new shape and ignores the old.
- All test fixtures + inline `format!` calls building feedback
  paths updated.
- This phase is purely a path rename. No semantic changes.

Migration note: in-flight working-tree feedback at the old path is
not picked up. Add a one-shot warning when `rebuild_repo` detects
files under the legacy `commits/` segment.

### Phase 2 — Sans-io core: event-log fold restructure

Foundational restructure of `disk_snapshot::derive_state`. The
current implementation is mostly a bulk-pass: it computes plans,
attribution, plan_touches, feedback maps separately, then per-plan
runs `build_commit_gates` to walk commits with carry-along state.

The new shape is a single chronological commit fold:

```text
fn derive_state(snapshot) -> RepoState:
    state = empty
    feedback_by_target = index_feedback_by_target_sha(snapshot.feedback_files)
    plans_from_head = group_plan_files(snapshot.plan_files)
    finished_from_head = group_finalize_snapshots(snapshot.finalize_files)
    state.plans = init plans from plans_from_head and finished_from_head

    for entry in snapshot.history (chronological order):
        apply_commit(state, entry)
        if entry's tree contains a newly-satisfying finalize for any
        plan: mark that plan frozen (sets state.plans[p].frozen_at =
        Some(entry.commit)).
        for fb in feedback_by_target.get(entry.commit) (ingest in
        deterministic order, e.g. sorted by author label):
            if state.plans[fb.plan_key].frozen_at.is_some(): skip
            else: apply_feedback(state, fb)
    return state
```

Per-commit mutations (`apply_commit`):

- Update `commit_order`, `attribution`, `plan_touches`,
  `commit_meta` (same logic as today, just per-commit instead of
  bulk).
- For each plan touched: if frozen, skip — no new attribution
  entry for this plan, no plan_touches entry under this plan's
  key (other plans the same commit touches still get their
  entries). Frozen plans are invisible to subsequent mutations.
- For each gate-relevant commit: extend the carry-along
  participant set for the plan (the logic currently in
  `build_commit_gates`), build the commit's gate entry. Skip
  entirely for frozen plans.

Why this matters:

- **Monotone semantics fall out naturally.** Once `frozen_at` is
  set for a plan, the fold skips it everywhere. No "search for
  the finalize commit, then filter commits after it" — the freeze
  is checked inline and forward-only.
- **Future state-caching becomes trivial.** A cached `RepoState`
  at commit C is just the fold's accumulator after processing
  the first C+1 commits. Resuming a fold from a cached state means
  setting `state = cached` and starting the loop at the next
  commit. This plan does NOT implement caching, but the shape
  must make it a 10-line addition rather than a rewrite.
- **Event-log thinking.** Each commit is an event; the projection
  is a fold over events. Future monotone rules (retired plans,
  plan locks, anything else with "once X, always X" semantics)
  fit the same shape without restructuring.

Scope:

- Rewrite `src/disk_snapshot.rs::derive_state` as the fold above.
- Move `build_commit_gates`'s per-commit walk inline into the
  fold; the existing function becomes `apply_gate_step(&mut
  PlanGateState, &CommitGate-event)` — or its body merges into
  the fold and the public function deletes.
- Pre-index feedback by target SHA at the start of the fold.
- Add `frozen_at: Option<CommitSha>` to `Plan` (recomputed each
  rebuild, not persisted).
- Update `attribution.rs` if it has any bulk-pass that needs to
  become per-commit.
- Preserve every existing observable behaviour: a repo with no
  finalize snapshots produces the same `RepoState` as before. The
  existing unit tests are the regression suite.

Acceptance:

- All existing `disk_snapshot` / `projection` tests pass
  unchanged.
- New tests assert monotone behaviour: frozen plan ignores
  subsequent attribution, plan_touches, feedback for that plan
  (verified via constructed `DiskSnapshot` fixtures).
- New test: the fold can be split — running the fold over commits
  `[c1..cN]` then continuing with `[cN+1..cM]` produces the same
  state as running it over `[c1..cM]` in one pass. (Validates the
  caching property without implementing the cache.)

Risk:

- Largest single mechanical change in this plan. Probably
  300–500 LOC moved/rewritten in `disk_snapshot.rs` and
  `projection.rs`.
- The current bulk-pass + per-plan-gate-walk structure is
  well-tested; care needed to preserve every edge case (the
  existing tests are the spec).
- Could be tempting to ship Phase 3+ before Phase 2 since
  approval-derived-completion's user-facing behaviour doesn't
  *require* the restructure. Don't. The whole point is a clean
  core; bolting the snapshot reader onto the existing bulk-pass
  and restructuring later is exactly the kind of "we'll fix it
  next sprint" that never happens.

### Phase 3 — Finalize snapshot reader (additive)

Introduce the new `.trinity/finished/<stem>/` reader without
removing any existing done-move code.

- New parser `disk_format::parse_finalize_path` recognises
  `.trinity/finished/<stem>/<agent>.md`.
- `disk_snapshot` ingests finalize-snapshot files alongside
  feedback files. Per-plan storage on `Plan`:
  `finalize_snapshot: Option<FinalizeSnapshot>` where
  `FinalizeSnapshot { entries: Vec<FinalizeEntry> }`. No commit
  SHA stored — the snapshot is read from HEAD's tree, full stop.
  The `git log .trinity/finished/<stem>/` walk for archived-cycle
  history is a separate code path used only by display, not by
  `is_finished`.
- Projection: new `is_finished(&Plan) -> bool` that returns true
  iff the plan file exists in HEAD AND `.trinity/finished/<stem>/`
  contains ≥1 file AND every file in that directory starts with
  `APPROVE`. No relationship checks against commits, attribution,
  or live feedback.
- Wire: new `state: "finished"` value in projection output for
  plans where `is_finished` returns true.
- Tests:
  - plan file + dir with 1 APPROVE → finished
  - plan file + dir with 2 APPROVE → finished
  - plan file + dir with 1 APPROVE + 1 REQUEST_CHANGES → active
    (mixed = not finished)
  - plan file + empty dir → active
  - plan file + no dir → active
  - no plan file + dir with APPROVE → plan doesn't exist (not
    finished, not active)
  - plan file + dir with APPROVE + later impl commit on the plan
    → STILL finished (the rule doesn't care about commit ordering)
  - plan file + dir with APPROVE + later REQUEST_CHANGES in
    working-tree feedback → STILL finished (live feedback doesn't
    affect finished-ness)

### Phase 4 — Lifecycle state derivation (additive)

The new `PlanLifecycle` enum exists alongside the legacy
`PlanState::Done`, so behavior can be diffed.

- New enum `PlanLifecycle::{Active, Finished}` in `repo_state.rs`.
  Derived from `is_finished`. Carried as a recomputed field, not
  stored.
- New struct `ArchivedCycleSummary { closer: CommitSha,
  approver_count: u32 }`. Stored on `Plan` as `archived_cycles:
  Vec<ArchivedCycleSummary>` (walked from git log of
  `.trinity/finished/<stem>/`).
- **Cost note**: `archived_cycles` is recomputed every rebuild by
  shelling out to `git log -- .trinity/finished/<stem>/`. This is
  O(history-depth-touching-this-path) per plan per rebuild. At
  Trinity's current scale (small repos, low plan counts, infrequent
  rebuilds triggered by FS signals) this is acceptable. A cache
  would be premature optimization; do not add one without a
  measured problem.
- Response builders gain a `lifecycle` field on wire alongside the
  existing `state` field. Tests assert both are present and
  consistent.

> **CLI work split off.** The original draft had Phases 5–8
> implementing `trinity init`, `trinity finish`, `trinity purge`,
> and the history-rewriting engine. That scope is now owned by
> `.trinity/stubs/trinity-cli.md`, which depends on this plan
> landing first (it needs the finalize-snapshot reader from Phase 3
> and the event-log fold from Phase 2). Splitting keeps this plan
> focused on the daemon-side lifecycle change.

### Phase 5 — Wire/UI cutover

Frontend and daemon flip together. Coordinated breaking change.

- Daemon `state` field emits `active | finished` derived from
  `PlanLifecycle`. Old `done` value never emitted.
- Frontend state-chip CSS classes updated; `state-done` class
  removed. "Move plan to done/" button deleted.
- `api_move_to_done` HTTP route deleted; `post_move_to_done` /
  `DoneResponse` deleted from `frontend/src/api.rs`.
- Tests: existing `plan_id_url_stable_across_done_flip` (and
  cousins) become `plan_id_url_stable_across_finish_flip`,
  asserting that the URL doesn't move when a plan becomes finished
  (because the file doesn't move).

### Phase 6 — Delete `PlanState::Done` and `DoneMove` types

Once the new lifecycle drives every response, the old types are dead.

- Delete `PlanState` entirely (was just `{Active, Done}`; collapses
  to nothing once `Done` goes).
- Delete `PlanTouchKind::DoneMove`, `CommitKind::DoneMove`,
  `PlanWorktreeStatus::DoneMovePending`.
- Rename `PlanWorktreeStatus::MissingActivePlanFile` →
  `PlanFileMissing`. Swap the action to a generic "restore the
  plan file from HEAD or revert your local change."
- Delete `WaitingReason::{SessionDone, CommitDoneMove,
  RestoreOrCommitDoneMove}` and their `derive_locations` branches.
- Delete `lifecycle::is_done_plan_path` and
  `lifecycle::plan_path_counterpart`.
- Delete the `done/<stem>.md` branch in `PlanKey::from_path`.
- Delete rename-detection paths in `git_io` that produce
  `DoneMove`.
- Delete `disk_snapshot` test fixtures for done-move and `done/`
  plan files.

### Phase 7 — Delete the `Phase` enum

Picked up from `architecture-tech-debt-sweep`, which deferred this
piece pending the `PlanState::Done` deletion that lands in Phase 6
of this plan. The deferral was sequencing, not scope; both halves
land in this sweep.

- Delete `Phase::Done` and the whole `Phase` enum.
- Replace `phase_for` callers with a `current_posture(&Plan)`
  helper: `PlanOnly | Mixed → Planning`, `CodeOnly → Implementing`.
- Wire `phase` field becomes a function of the latest reviewable
  commit's `CommitKind`, not a stored enum.
- Frontend `phase: "done"` rendering is already moot after Phase 5
  (the wire never emits it once `PlanState::Done` is gone); this
  phase deletes the type as well.

## Migration

Two migrations: stale `done/` files and the feedback-path rename.

### Stale `.trinity/plans/done/` files (working tree)

A repo upgrading past Phase 6 will have a `.trinity/plans/done/`
directory with files the daemon can no longer parse as plans. The
daemon must not silently drop these from the visible plan set.

Approach:
- On `rebuild_repo`, detect any `.trinity/plans/done/*.md` files in
  the working tree.
- For each such file, emit a `PlanConflict` (already a thing in
  `RepoState.plan_conflicts`) with a message: "this file is a
  leftover from the pre-approval-derived-completion workflow. Move
  it back to `.trinity/plans/<stem>.md` if you want it tracked,
  delete it if it's purely historical, or run `trinity finish` on
  the active equivalent if you want a proper finalize snapshot."
- The UI surfaces this in the conflicts section it already renders.

### Git history with `done_move` renames

The daemon walks history for plan-touch derivation. Historical
`done/<stem>.md` paths in commits before this plan lands need
read-side handling:

- `git_io` keeps a one-time reader for historical commits that
  produced `done_move` renames. These commits get categorised as
  `Unattributed` (current proxy for "commit doesn't drive this
  plan's lifecycle") so they show up in the timeline without
  driving the gate.
- This is read-only history compatibility, not a writeable code
  path.
- After 2 release cycles, the reader drops — no active repos
  should be introducing new pre-cutover plans by then.

### Feedback path rename (`commits/` segment removed)

Phase 1 changes the feedback path from
`.trinity/feedback/<plan>/commits/<sha>/<agent>.md` to
`.trinity/feedback/<plan>/<sha>/<agent>.md`. Working-tree feedback
files at the old path are not picked up after the rename.

Approach:
- `rebuild_repo` detects files under the legacy `commits/` segment
  and emits a one-shot warning ("in-flight feedback at legacy
  path; move or commit before continuing").
- No automatic migration — let the operator `git mv` or rename. If
  the feedback wasn't committed (default), `mv` in the working
  tree is enough.
- After 2 release cycles, the legacy-path detector drops.

## Risks and Tensions

Real ambiguities in the design. Reviewers should weigh each.

The pre-snapshot model had T1 (cycle boundaries reversible) and T2
(late feedback unfinishes a plan) as live concerns. Both are
resolved by the snapshot model: the finalize commit is a git fact,
not a derived state, and late working-tree feedback cannot
retroactively invalidate it. They're not in this list anymore.

### T1 — `Archived` is a per-cycle state, not a plan-level state

The stub lists `Archived` alongside `Active`/`Finished` in the UI
state-chip vocabulary, but archived describes an older cycle, not
the plan as a whole. A plan with one archived cycle and an active
new cycle is `active` (with `archived_cycles: [cycle-1]`), not
`archived`.

This plan resolves the ambiguity by:
- Plan-level wire `state`: `active | finished` only.
- Cycle list on the wire: `archived_cycles:
  Vec<ArchivedCycleSummary>` (derived from `git log
  .trinity/finished/<stem>/`).
- UI's "Archived" chip is per-cycle in the cycle history view,
  not in the main plan list.

If the user wants `archived` as a plan-level state (e.g., for
plans the operator wants hidden from the dashboard after they're
finished and not being iterated on), this is a separate concept
("retired plan") that should get its own mechanism.

### T2 — `PlanWorktreeStatus::MissingActivePlanFile` semantics shift

Today this status fires when the plan file is gone AND the done
counterpart is also gone. With `done/` gone, it just means "plan
file deleted from worktree." This is still a meaningful state —
something's wrong, master should know — but the
`RestoreOrCommitDoneMove` action loses its second leg.

Resolution: rename `MissingActivePlanFile` → `PlanFileMissing`,
keep the watcher state, swap the action to a generic "restore the
plan file from HEAD or revert your local change."

After `--purge` (which deletes the plan file), this status will
fire if the daemon is still watching the plan as active. The
daemon should detect "this plan's last commit was a finalize that
removed the plan file" and treat that as `finished` rather than
`PlanFileMissing`. Open question: does the daemon need a separate
"`finished-and-purged`" recognition, or is this handled by the
existing finished detection (presence of
`.trinity/finished/<stem>/`)?

### T3 — Finalize commit forgery

`trinity finish` validates the live gate locally before making the
finalize commit, so an honest run never produces a forged
snapshot. But the snapshot itself is just files in a git tree:
master can write fake APPROVE files directly into
`.trinity/finished/<stem>/` and commit them, bypassing the CLI's
checks.

Trinity is not trying to be tamper-proof; the goal is to make
unilateral resolution require explicit, visible action instead of
a silent `git mv`. A forged finalize commit is visible in the
timeline; reviewers can spot it on their next wake. If you have a
master agent maliciously forging finalize commits, you have a
bigger problem than this plan can solve in code.

Worth naming in the docs so users understand the threat model.

### T4 — `--purge` history rewriting changes SHAs

`trinity finish --purge` rewrites commit history to strip
`.trinity/` content from mixed commits. This changes the SHAs of
every rewritten commit. Anyone who has the pre-rewrite branch
checked out, has open PRs against those commits, or has built CI
artifacts referencing those SHAs will see breakage.

This is intentional — `--purge` is a deliberate operation. The
CLI should warn loudly before doing the rewrite, and refuse on
protected branches without explicit opt-in.

### T5 — `--squash` with interleaved foreign commits

`--squash` refuses when foreign commits are interleaved between
the plan's earliest commit and HEAD. The "interleaved" check is
strict: any non-plan-attributed commit in the range is foreign.

Operators may find this restrictive. The escape hatch is to
rebase the plan's commits to be contiguous before squashing, or
to just `git reset --soft` + `git commit` manually.

Open question: should `--squash` have a `--allow-foreign-commits`
mode that places foreign commits before or after the squashed
result? The plan tentatively says no — keep it strict, push the
user to use plain git for messy cases.

### T6 — Wire compatibility window

The cutover removes `state: "done"`, `phase: "done"`, the
`api_move_to_done` endpoint, and a frontend button. Any external
consumer (LSP integrations, third-party scripts, the MCP shim if
it doesn't share types) sees the breaking change at once.

Trinity is single-tenant and the SPA is co-versioned. The MCP shim
already shares types with the daemon. Acceptable risk.

### T7 — `--purge` keeps finalize snapshot but loses plan body

Per case 7 in the `--purge` test list, plain `--purge` strips the
plan file and the working-tree feedback but keeps the finalize
snapshot. A cloner sees `.trinity/finished/<stem>/<agent>.md` with
APPROVE verdicts but no `.trinity/plans/<stem>.md` to know what
the plan was about.

For most use cases this is fine — the finalize commit's message
typically references the plan stem, and the archived feedback
files mention the work done. For users who want richer
provenance, `--squash` (without `--purge`) preserves the plan
body in the squashed commit's message via the original plan-file
contents.

Mention in docs; don't restrict.

## Acceptance Criteria

### Lifecycle and snapshot

1. No code path creates or expects `.trinity/plans/done/`.
2. No HTTP endpoint accepts "move to done"; no SPA "Move to done"
   button.
3. `PlanState::Done`, `PlanTouchKind::DoneMove`,
   `CommitKind::DoneMove`, `PlanWorktreeStatus::DoneMovePending`,
   `WaitingReason::{SessionDone, CommitDoneMove,
   RestoreOrCommitDoneMove}` removed from the codebase.
4. `lifecycle::is_done_plan_path`,
   `lifecycle::plan_path_counterpart` removed.
5. `PlanKey::from_path` rejects `.trinity/plans/done/<stem>.md`.
6. Wire `state` field emits one of `active | finished`.
   Plan-level `archived` is not emitted.
7. `archived_cycles` field on plan detail wire carries per-cycle
   metadata sourced from `git log .trinity/finished/<stem>/`.
8. **Daemon finished rule is exactly the file checks** (regression
   test): the daemon returns `state: "finished"` iff
   `.trinity/plans/<stem>.md` exists in HEAD AND
   `.trinity/finished/<stem>/` in HEAD contains ≥1 file AND every
   file in that directory has its first line starting with
   `APPROVE`. Tests assert:
   - presence of impl commit doesn't matter (plan-only finish
     works)
   - post-finalize REQUEST_CHANGES in working-tree feedback →
     still finished
   - post-finalize impl commit → still finished
   - mixed-verdict directory (≥1 APPROVE + ≥1 non-APPROVE) →
     active, NOT finished
   - removing `.trinity/finished/<stem>/` → back to active
   - removing `.trinity/plans/<stem>.md` → plan doesn't exist
   - the pathological "snapshot committed before plan" history
     case is unspecified; the CLI never produces it, and either
     "finished" or "active" is acceptable depending on whether
     the implementation reads HEAD or replays events. Test
     coverage skips this case.
9. Plan inference excludes finished plans. `wait_for_work`,
   `get_context`, and any other surface that resolves "the current
   plan" when no `plan_id` is supplied treats only plans with
   `state == active` as candidates. A repo with one active plan
   and any number of finished plans still resolves
   unambiguously to the active one; a `state: finished` plan is
   visible in `list_plans` but never picked up as the inferred
   default.

### Feedback path

10. Working-tree feedback at `.trinity/feedback/<stem>/<sha>/<agent>.md`
    is parsed; old `commits/<sha>/<agent>.md` is not.
11. Legacy-path files in the working tree surface as a warning,
    not a silent drop.

### Migration

12. Existing repos with `.trinity/plans/done/*.md` files surface
    those files in `plan_conflicts` with a migration message; no
    silent drop.
13. `tests/end_to_end.rs` includes a regression test that boots a
    repo containing `.trinity/plans/done/legacy.md`, verifies the
    conflict surfaces, and verifies the legacy file is not picked
    up as an active plan.
14. Working-tree feedback at the legacy `commits/` path surfaces a
    one-shot warning per rebuild.

### `Phase` enum

15. The `Phase` enum is removed from the codebase (Phase 7).
    Production code computes posture from `CommitKind` via the
    `current_posture` helper rather than reading a stored variant.
    No `Phase::Done` consumer remains.

### CLI work (separate stub)

16. The CLI commands (`trinity init`, `trinity finish`,
    `trinity purge`), their flags, the history-rewriting engine,
    and the 14 `--purge` edge-case regression tests live in
    `.trinity/stubs/trinity-cli.md`. That stub's acceptance
    criteria are not gating this plan; this plan delivers the
    daemon-side machinery the CLI will read.

## Out of Scope (Explicit)

- `wait_for_work` CLI alias (`trinity wfw`) and timeout changes
  (separate stub).
- Concurrent-review-cycles wake/projection (separate stub).
- Renaming or restructuring `.trinity/feedback/` layout for any
  other reason.
- Multi-plan archival UI (showing all archived cycles across all
  plans) — current plan only.
- Retired-plan workflow (plans operator wants hidden from the
  dashboard without finishing them).
