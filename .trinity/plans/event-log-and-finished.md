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
git via a deliberate **finalize commit** — a commit that adds files
under `.trinity/finished/<stem>/`. How those files get there
(operator workflow, CLI tool, future automation) is out of scope
for this plan; the daemon trusts what's in HEAD. A semi-malicious
or confused master agent can still forge a finalize commit by
writing fake APPROVE files directly, but at that point
the operator has bigger problems than this artifact. Trinity is not
trying to be a tamper-proof attestation system, only to make
resolution require explicit, visible action instead of a silent
`git mv`.

Scope this plan owns:

- Snapshot-on-finalize: `.trinity/finished/<stem>/` carries a
  committed copy of the reviewer feedback files at finish time. No
  per-SHA subdirectory — there is one snapshot per plan in HEAD,
  and each new finalize commit overwrites it. History is preserved
  by git itself (the commit log of `.trinity/finished/<stem>/`).
- The daemon's finished rule is event-log truth: walk commits in
  chronological order; the first commit whose tree contains the
  plan file at `.trinity/plans/<stem>.md` AND a
  `.trinity/finished/<stem>/` directory with ≥1 file all starting
  with `APPROVE` freezes the plan. Once frozen, the plan stays
  finished. Subsequent commits — including ones that modify the
  plan file, add code, or remove files from
  `.trinity/finished/<stem>/` — do not affect the plan's state.
  Monotone: finished is a one-way transition under git operations.
  No ordering, ceremony, or content checks beyond those two file
  presences and the first-line APPROVE rule.
- All sophisticated validation (gate is genuinely approved,
  working tree is clean, reviewer files match real reviewer
  authors, etc.) lives in `trinity finish` (the CLI, in a
  separate stub) — at mutation time, before the commit lands.
  Nothing in the daemon's read-time projection.
- The sans-io core (`disk_snapshot::derive_state`) restructures
  from its current bulk-pass into a single chronological commit
  fold. The finished-rule freeze is one event the fold handles
  per commit. The restructure also makes future state-caching
  trivial (a cached `RepoState` at commit C is the fold's
  accumulator at that point) and sets up event-log thinking for
  any future monotone rule.
- Deletion of `PlanState::Done`, `CommitKind::DoneMove`,
  `PlanTouchKind::DoneMove`, the `done_move_pending` worktree
  status, the `api_move_to_done` HTTP endpoint, and the SPA's
  "Move plan to done/" button.
- Feedback-path simplification: drop the redundant `commits/`
  segment. Working-tree feedback becomes
  `.trinity/feedback/<stem>/<sha>/<agent>.md` (`<sha>` stays
  because the working-tree gate is keyed per-impl-commit).
- CLI work (`trinity init`, `trinity finish`, `trinity purge`)
  is **out of scope** for this plan — owned by
  `.trinity/stubs/trinity-cli.md`. That stub depends on this plan
  landing first. The daemon never writes `.trinity/finished/`
  files; the CLI does, under the operator's hand.

The `Phase` enum deletion deferred by `architecture-tech-debt-sweep`
(waiting on `PlanState::Done` removal) lands here as Phase 6.

Source design rationale lives in
`.trinity/stubs/event-log-and-finished.md` — this plan does
not duplicate the rationale, only the implementation.

## Definitions

These need to be unambiguous before the implementation can land
cleanly. They are not in the stub; they are this plan's contribution.

### Plan cycle (display-only)

A **plan cycle** is a historical range of plan-attributed commits
ending at a real freeze event. Cycles do not drive any projection
state — they're a UI/CLI concept derived from the same event-log
fold that decides `frozen_at`. During replay the fold records each
commit at which a plan freezes (the same commit it would write to
`frozen_at` if seen for the first time at HEAD); the cycle list is
the sequence of those freeze events. Raw `git log
.trinity/finished/<stem>/` over-counts — it enumerates every
commit that touched the snapshot path, including post-freeze
edits, deletions, reverts, and phantom re-finalizes that are not
cycle boundaries. The daemon's finished rule (below) doesn't
reference cycles.

### Latest reviewable commit (CLI policy)

The most recent commit in `commit_order` whose `CommitKind` is
`PlanOnly`, `CodeOnly`, or `Mixed` for this plan. Used by `trinity
finish` at mutation time to decide which feedback directory to
snapshot. NOT used by the daemon's finished-ness derivation.

### Current gate participant

Anyone who has written a feedback file under
`.trinity/feedback/<plan-stem>/<target-sha>/` (note: post-simplification
path; the legacy `commits/` segment is gone). Used by the daemon's
live-gate view (current working-tree review status for the latest
reviewable commit). NOT used by the daemon's finished-ness
derivation.

### Finalize snapshot

A `.trinity/finished/<plan-stem>/` directory in HEAD's tree
containing reviewer feedback files — one per approving reviewer,
verdict + body. Flat: no per-SHA subdirectory. The presence of
this directory is the only git-visible signal that a plan cycle
was deliberately closed. How the files got there is out of scope
for this plan; the daemon trusts what's in HEAD.

The snapshot does not replace the working-tree feedback files; both
coexist. The working-tree files remain the live, async surface;
the snapshot is the archival commitment at the close.

There is one snapshot per plan in HEAD at any time. Each new
finalize commit overwrites the prior contents. The history of
past finalize commits is recovered from the event-log fold's
recorded freeze events: each freeze event's commit SHA is what
`git show <sha>:.trinity/finished/<stem>/` reads to render that
cycle's snapshot. The raw `git log .trinity/finished/<stem>/`
walk is not the source of truth — it includes non-freeze touches
(deletions, phantom re-finalizes, edits) that the fold has
already discarded.

### Finalize commit

A commit that creates or modifies `.trinity/finished/<plan-stem>/`.
The finalize commit is a **lifecycle boundary**, not a review
target:

- It is visible in the timeline as a clickable `commit_finalize`
  row.
- It is NOT a review target — Trinity does NOT create a
  `CommitGate` for it and live `.trinity/feedback/...` writes
  targeting its SHA are dropped (`upsert_feedback` refuses
  non-reviewable events). The finalize event always has
  `gate: None`.
- It does not feed `waiting_on`. Once the plan freezes, the
  master/reviewer waiting machinery is sealed end-to-end.
- Clicking it shows the `.trinity/finished/<stem>/` approval
  snapshot from the freeze commit's tree (read at render time
  via `git show <frozen_at>:.trinity/finished/<stem>/...`), not
  live feedback.

`.trinity/feedback/<stem>/<sha>/<agent>.md` is live review
feedback for reviewable plan/impl commits.
`.trinity/finished/<stem>/<agent>.md` is the finalized approval
snapshot. They are never conflated in gate logic.

Trust model: the daemon does not validate snapshot contents at
read time beyond the file checks in §Finished plan. Any process
with commit access can write a snapshot directly into git;
ensuring those files reflect genuine reviewer approval is the
job of whichever tool produces the finalize commit (see
`trinity-cli` stub). Late reviewers who notice a discrepancy
must address it via a future plan (e.g. open a new plan to
re-examine the work) — they cannot rescind the freeze.

### Finished plan

A plan is **finished** iff the event-log fold's per-plan
`frozen_at: Option<CommitSha>` is `Some(_)` at HEAD. The fold sets
`frozen_at` on the first commit in chronological order whose tree
satisfies the finalize rule:

1. `.trinity/plans/<stem>.md` exists in that commit's tree, and
2. `.trinity/finished/<stem>/` in that commit's tree contains at
   least one file AND every file in that directory has its first
   line starting with `APPROVE`.

Once `frozen_at` is set, the fold treats the plan as sealed for
every subsequent commit: no new attribution, no new plan_touches
entries under this plan's key, no new gate entries, no lifecycle
change. The plan stays `finished` even if later commits remove
files from `.trinity/finished/<stem>/`, modify the plan file, add
code under `Implements: <stem>`, or write new working-tree
feedback. The projection is monotone: finished is a one-way state
transition under git operations.

**Display-only feedback on a frozen plan's history.** Sealing
covers state transitions, not disk-state rendering. Working-tree
feedback files at `.trinity/feedback/<stem>/<sha>/` for any `<sha>`
in the plan's history are still parsed and displayed in the
timeline — they're just current disk state, not events that change
the plan's lifecycle or gate-waiting computations. A reviewer who
writes a late `REQUEST_CHANGES` targeting a pre-freeze commit
leaves a visible note in the timeline; the plan remains
`finished`, no waiting-for-work is generated, no new gate state
is computed.

The first-line APPROVE check is the only content inspection: file
bodies after the first line are not parsed, file names don't
matter beyond being readable, file count doesn't matter beyond
non-zero. A directory with any non-APPROVE file present (e.g., a
stray REQUEST_CHANGES dropped in) makes the rule NOT satisfied at
that commit — the fold doesn't freeze. If a later commit cleans
up the non-APPROVE file and the rule then holds, that later
commit is where freeze happens.

A plan exists if (a) `.trinity/plans/<stem>.md` is present in
HEAD, or (b) the plan was ever frozen (the fold recorded a freeze
event for it). Case (b) is the monotone-finished anchor: once
frozen, the plan persists in `list_plans` even if HEAD later
loses the plan file. Plan body for case (b) is rendered from
`git show <frozen_at>:.trinity/plans/<stem>.md`, not HEAD.

If the plan file exists in HEAD and the fold never reached a
commit where the finalize rule held, the plan is `active`. If the
plan file is absent from HEAD and the plan was never frozen, the
plan doesn't exist at all (no historical anchor).

**Daemon's perspective on transitioning a frozen plan**: there
is exactly one git-state transition that can un-finish a frozen
plan:

- **Remove the finalize commit from reachable history** (via any
  history rewrite — `git reset`, `git rebase -i`, a CLI rewriter,
  etc.). On a fresh rebuild, the fold no longer sees the freeze
  event and the plan is `active` again (or doesn't exist, if the
  plan-file blob is also unreachable post-rewrite).

Deleting `.trinity/plans/<stem>.md` from HEAD does **not**
un-finish the plan. The freeze event is still in reachable
history; the fold replays it and sets `frozen_at`. The plan stays
in `list_plans`, body renders from the freeze commit.

`git revert` of the finalize commit does NOT un-finish the plan.
The revert is a new commit that removes the snapshot directory
from the tree, but the original freeze event is still in history
at the original commit. The fold replays from the beginning of
history and freezes at the original finalize. Trinity does not
parse commit messages or otherwise treat reverts specially — a
revert is just another commit whose patch undoes earlier tree
content.

Equivalently, starting a fresh plan with a different name (a new
`<stem>.md` file) sidesteps the question entirely: a new fold
state, no finalized predecessor to inherit. The original frozen
plan stays as a historical record.

**Snapshot content read after freeze.** The fold remembers
`frozen_at: CommitSha` per frozen plan. When the SPA or any UI
needs to render the finalize snapshot's contents (which reviewers
approved? what did they write?), it reads from
`git show <frozen_at>:.trinity/finished/<stem>/`, not from HEAD's
tree. This makes the rendered snapshot stable against post-freeze
tree edits — if a later commit deletes or modifies the snapshot
files, the daemon still shows the plan as `finished` (per the
rule), and the UI still shows the original APPROVE files. HEAD's
`.trinity/finished/<stem>/` contents are not authoritative for
display once the plan is frozen; the freeze-time content is.

**Post-freeze tree changes that look like new finalizes.** A
later commit that touches `.trinity/finished/<stem>/` (adds,
modifies, or removes files) does not trigger anything. The fold
ignores frozen plans for the rest of the walk; no new freeze
event, no projection update. The same applies to any other
post-freeze *commit* attributable to the plan (`Implements:`
trailer, plan-file edit). All such commits land as
`CommitKind::Unattributed` from the projection's perspective —
identical to today's behaviour for commits whose attribution
trailer points at an unknown plan. They're visible in the repo's
overall commit history but don't appear in the frozen plan's
timeline, gate, or attribution.

Working-tree feedback files (under `.trinity/feedback/<stem>/<sha>/`)
are not commits and follow a different rule: they're applied at
their target SHA during the fold's per-commit step. If that target
SHA is in the plan's pre-freeze history, the feedback is rendered
in the timeline (display only — see §Finished plan's
"display-only feedback" paragraph). If the target SHA is post-freeze
(or doesn't exist), the fold's frozen-plan skip path drops it
and no gate or attribution is computed.

Per-plan scope: every check is keyed on `<stem>`. A finalize for
`foo` says nothing about `bar`.

All sophisticated validation — the live working-tree gate is
genuinely approved, the snapshot authors are real reviewers, the
working tree is clean — is whoever-produces-the-finalize-commit's
problem (see `trinity-cli` stub). The daemon does none of it at
read time.

### Archived cycle

A cycle whose closer (freeze event) is not the most recent freeze
event for the plan. Archived cycles are recovered from the fold's
`freeze_events: Vec<CommitSha>` side-output — every entry except
the last one is an archived cycle. Each archived cycle's snapshot
content is read via `git show <freeze-sha>:.trinity/finished/<stem>/`.
They appear in the cycle-history view but do not drive any
waiting/gate state. Raw `git log` over the snapshot path is not
used; it over-counts (post-freeze deletions, phantom re-finalizes,
hand edits all touch the path but are not cycle boundaries).

### Plan lifecycle state (wire)

The wire `state` field, currently `active | done`, becomes one of:

- `active` — the fold has not frozen this plan (no prior chronological
  commit satisfied the finalize rule)
- `finished` — the fold has frozen this plan (`frozen_at: Some(_)`)

Plans absent from `state.plans` (no plan file in HEAD) don't
appear in `list_plans` at all — there's no third "deleted" wire
state.

`finished` is sticky: once the fold sets `frozen_at`, the plan
stays `finished` for the life of the branch. Only a history
rewrite that removes the freeze event, or deleting the plan file
from HEAD (which removes the plan entirely), changes the state.
The CLI is what catches "the snapshot is now stale relative to new
work" — at the operator's request when they next run `trinity
finish` — not the daemon.

`archived` is **not** a plan-level state. Archived cycles are
derived from the fold's recorded freeze events surfaced in the
cycle-history UI view; they don't appear in the plan-level `state`
wire field.

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
  because the working-tree gate is keyed per-reviewable-commit.
  The finalize snapshot has a flatter shape —
  `.trinity/finished/<stem>/<agent>.md` — because at finalize
  time there is one current reviewable commit being snapshotted,
  and old finalizes are recovered via git history rather than
  parallel SHA directories in HEAD.
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
  violation goes away in Phase 4 (Wire/UI cutover). The CLI does
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
  segment) and the `finalize_rule_now_satisfied(tree, plan)`
  helper the Phase 2 fold uses on each commit. The fold sets
  per-plan `frozen_at: Option<CommitSha>` when the rule first
  holds in chronological order; `is_finished(&Plan) -> bool` is
  just `plan.frozen_at.is_some()`. No HEAD-tree-only check —
  finished-ness is determined by the fold's history walk, not by
  re-inspecting HEAD's tree at read time.

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

Operators with in-flight feedback at the old path move or
re-create the files; the daemon doesn't warn or migrate. The
old shape is forgotten end-to-end.

### Phase 2 — Sans-io core: event-log fold + finalize snapshot reader

Foundational restructure of `disk_snapshot::derive_state` together
with the new `.trinity/finished/<stem>/` reader. The two land as one
phase because the fold's per-commit step has to know whether a given
commit makes a plan finished — that requires the finalize-snapshot
parser. Splitting them creates a Phase 2 that can't be implemented
or tested in isolation.

The current `derive_state` is mostly a bulk-pass: it computes plans,
attribution, plan_touches, feedback maps separately, then per-plan
runs `build_commit_gates` to walk commits with carry-along state.

The new shape is a single chronological commit fold:

```text
fn derive_state(snapshot) -> RepoState:
    state = empty
    feedback_by_target = index_feedback_by_target_sha(snapshot.feedback_files)
    finalize_by_plan_per_commit = index_finalize_files(snapshot.finalize_files)
    state.plans = init plans from snapshot.plan_files

    for entry in snapshot.history (chronological order):
        apply_commit(state, entry)
        for plan_key in plans potentially affected by entry's tree:
            if finalize_rule_now_satisfied(state, plan_key, entry):
                mark plan frozen (state.plans[plan_key].frozen_at = Some(entry.commit))
        for fb in feedback_by_target.get(entry.commit) (ingest in
        deterministic order, e.g. sorted by author label):
            if state.plans[fb.plan_key].frozen_at.is_some(): skip
            else: apply_feedback(state, fb)
    return state
```

The finalize reader is the supporting parser + tree-checker used by
`finalize_rule_now_satisfied`:

- New `disk_format::parse_finalize_path` recognises
  `.trinity/finished/<stem>/<agent>.md` (flat, no SHA segment).
- `disk_snapshot` ingests finalize-snapshot files alongside
  feedback files into the per-commit index used by the fold.
- `finalize_rule_now_satisfied(state, plan_key, entry)` returns true
  iff, at this entry's tree: the plan file exists AND
  `.trinity/finished/<stem>/` contains ≥1 file AND every file
  starts with `APPROVE`. The fold only checks the rule when the
  current commit could plausibly have changed the answer (the
  commit's tree-diff touched either `.trinity/plans/<stem>.md` or
  `.trinity/finished/<stem>/`); other commits don't trigger the
  check.
- The fold emits a per-plan `freeze_events: Vec<CommitSha>` side
  output (one entry per commit where that plan transitioned to
  frozen). Archived-cycle display reads from this list, not from a
  raw `git log` over the snapshot path; raw `git log` includes
  non-freeze touches the fold has already discarded.

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
- Finalize-snapshot rule tests (the projection state at HEAD is
  the cumulative fold result; each case constructs a `DiskSnapshot`
  whose history produces the named scenario):
  - plan added at c1 + 1 APPROVE file added at c2 → finished
  - plan added at c1 + 2 APPROVE files added at c2 → finished
  - plan added at c1 + 1 APPROVE + 1 REQUEST_CHANGES at c2 →
    active (mixed = not finished)
  - plan added at c1 + empty finished dir at c2 → active
  - plan added at c1 + no finished dir → active
  - **finalize file added at c1, plan file added at c2** →
    finished (the fold checks the rule at c2 when the plan first
    exists; both files coexist in HEAD's tree)
  - **plan added c1, finished c2, later impl commit at c3 →
    finished** (frozen at c2; c3's plan_attribution skipped)
  - **plan added c1, finished c2, later plan-only revision at
    c3 → finished** (frozen at c2; c3's plan_touches skipped for
    this plan)
  - **plan added c1, finished c2, later REQUEST_CHANGES written
    to working-tree feedback** → finished (live feedback is in
    `snapshot.feedback_files` for non-frozen plans only; frozen
    plans skip feedback application)
  - finalize commit revert (c1 plan, c2 finalize, c3 reverts c2
    removing the finalize dir) → **STILL finished**. The revert
    is just another commit whose patch undoes earlier tree
    content; the fold sees the freeze event at c2 and never
    un-freezes. Trinity does not parse commit messages or
    otherwise treat reverts specially. The only way to make the
    plan `active` again is to remove the finalize commit from
    reachable history (any history rewrite — `git reset`,
    `git rebase -i`, etc.) so a fresh rebuild's fold never sees
    the freeze event.

Risk:

- Largest single mechanical change in this plan. Probably
  300–500 LOC moved/rewritten in `disk_snapshot.rs` and
  `projection.rs`.
- The current bulk-pass + per-plan-gate-walk structure is
  well-tested; care needed to preserve every edge case (the
  existing tests are the spec).
- Could be tempting to ship the lifecycle / wire / delete phases
  before Phase 2 since
  approval-derived-completion's user-facing behaviour doesn't
  *require* the restructure. Don't. The whole point is a clean
  core; bolting the snapshot reader onto the existing bulk-pass
  and restructuring later is exactly the kind of "we'll fix it
  next sprint" that never happens.


### Phase 3 — Lifecycle state derivation (additive)

The new `PlanLifecycle` enum exists alongside the legacy
`PlanState::Done`, so behavior can be diffed.

- New enum `PlanLifecycle::{Active, Finished}` in `repo_state.rs`.
  Derived from `is_finished`. Carried as a recomputed field, not
  stored.
- New struct `ArchivedCycleSummary { closer: CommitSha,
  approver_count: u32 }`. Stored on `Plan` as `archived_cycles:
  Vec<ArchivedCycleSummary>`. Sourced from the fold's recorded
  freeze events (the fold emits a `freeze_events:
  Vec<CommitSha>` side-output during replay; one entry per commit
  where a plan transitioned to frozen). Approver count for each
  cycle is read from `git show <freeze-sha>:.trinity/finished/<stem>/`.
- **Cost note**: `archived_cycles` is recomputed from the fold's
  freeze-event list plus one `git show` per cycle. This is
  O(freeze-events-for-this-plan) per rebuild — typically 0 or 1.
  No separate `git log` walk is needed.
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

### Phase 4 — Wire/UI cutover

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

### Phase 5 — Delete `PlanState::Done` and `DoneMove` types

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

### Phase 6 — Delete the `Phase` enum

Picked up from `architecture-tech-debt-sweep`, which deferred this
piece pending the `PlanState::Done` deletion that lands in Phase 5
of this plan. The deferral was sequencing, not scope; both halves
land in this sweep.

- Delete `Phase::Done` and the whole `Phase` enum.
- Replace `phase_for` callers with a `current_posture(&Plan)`
  helper: `PlanOnly | Mixed → Planning`, `CodeOnly → Implementing`.
- Wire `phase` field becomes a function of the latest reviewable
  commit's `CommitKind`, not a stored enum.
- Frontend `phase: "done"` rendering is already moot after Phase 4
  (the wire never emits it once `PlanState::Done` is gone); this
  phase deletes the type as well.

## Migration

Two migrations: stale `done/` files and the feedback-path rename.

### Stale `.trinity/plans/done/` files (working tree)

A repo upgrading past Phase 5 will have a `.trinity/plans/done/`
directory with files the daemon can no longer parse as plans. The
daemon must not silently drop these from the visible plan set.

Approach:
- On `rebuild_repo`, detect any `.trinity/plans/done/*.md` files in
  the working tree.
- For each such file, emit a `PlanConflict` (already a thing in
  `RepoState.plan_conflicts`) with a message: "this file is a
  leftover from the pre-approval-derived-completion workflow. Move
  it back to `.trinity/plans/<stem>.md` if you want it tracked,
  delete it if it's purely historical, or commit a finalize
  snapshot at `.trinity/finished/<stem>/` for the active
  equivalent."
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
files at the old path are silently ignored (the parser drops them
along with any other unrecognised shape). The daemon has no
knowledge of the legacy layout; operators handle in-flight files
themselves (move or delete).

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
  Vec<ArchivedCycleSummary>` (derived from the fold's
  `freeze_events` side-output, not from raw `git log` of the
  snapshot path).
- UI's "Archived" chip is per-cycle in the cycle history view,
  not in the main plan list.

If the user wants `archived` as a plan-level state (e.g., for
plans the operator wants hidden from the dashboard after they're
finished and not being iterated on), this is a separate concept
("retired plan") that should get its own mechanism.

### T2 — `PlanWorktreeStatus::MissingActivePlanFile` semantics shift

Today this status fires when the plan file is gone from the
worktree AND the done counterpart is also gone. With `done/` gone,
it just means "plan file deleted from worktree but still in HEAD."
This is still a meaningful state — operator made an uncommitted
deletion — but the `RestoreOrCommitDoneMove` action loses its
second leg.

Resolution: rename `MissingActivePlanFile` → `PlanFileMissing`,
keep the watcher state, swap the action to a generic "restore the
plan file from HEAD or commit the deletion."

If the plan file is deleted from HEAD outright (committed
deletion, not a worktree-only edit), the plan ceases to exist
per the Finished plan section — absent from `list_plans`, no
state. The `PlanFileMissing` worktree status doesn't fire because
the plan isn't being tracked anymore.

### T3 — Wire compatibility window

The cutover removes `state: "done"`, `phase: "done"`, the
`api_move_to_done` endpoint, and a frontend button. Any external
consumer (LSP integrations, third-party scripts, the MCP shim if
it doesn't share types) sees the breaking change at once.

Trinity is single-tenant and the SPA is co-versioned. The MCP shim
already shares types with the daemon. Acceptable risk.

CLI-side tensions (forgery, history-rewrite SHA churn, squash
with foreign commits, plan-body retention after purge) live in
`.trinity/stubs/trinity-cli.md`. They don't affect daemon
behaviour.

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
   metadata sourced from the fold's `freeze_events` side-output
   (one entry per commit at which a plan transitioned to frozen),
   not from raw `git log` over the snapshot path. Regression test
   constructs a frozen plan, then commits a post-freeze deletion
   of `.trinity/finished/<stem>/`, then commits a phantom
   re-finalize (snapshot files written again). Assert
   `archived_cycles.len() == 1` — only the original freeze counts.
8. **Daemon finished rule is event-log truth** (regression test):
   the daemon returns `state: "finished"` iff the fold's
   `frozen_at: Option<CommitSha>` is `Some(_)` at HEAD. The fold
   freezes a plan on the first chronological commit whose tree
   contains the plan file at `.trinity/plans/<stem>.md` AND a
   `.trinity/finished/<stem>/` directory with ≥1 file all
   starting with `APPROVE`. Tests assert:
   - presence of impl commit doesn't matter (plan-only finish
     works)
   - finalize file added before plan file (commit order: finished
     dir at c1, plan file at c2) → finished at c2 (rule first
     holds at c2 when both files coexist)
   - post-finalize REQUEST_CHANGES in working-tree feedback →
     still finished
   - post-finalize impl commit → still finished
   - post-finalize plan-only revision → still finished
   - mixed-verdict directory at the freeze candidate (≥1 APPROVE
     + ≥1 non-APPROVE) → rule does not hold → NOT frozen at that
     commit; if a later commit cleans up the non-APPROVE file,
     freeze happens there
   - removing `.trinity/finished/<stem>/` in a later commit
     (e.g. `git revert <finalize-sha>`) → STILL finished, NOT
     un-frozen (monotone). The plan-detail wire still renders
     the snapshot contents read from `git show <frozen_at>:`
   - modifying `.trinity/finished/<stem>/<agent>.md` in a later
     commit (e.g. operator hand-edits the APPROVE files) → STILL
     finished; the wire renders the freeze-time contents, not
     HEAD's
   - removing `.trinity/plans/<stem>.md` from HEAD AFTER a freeze
     → STILL finished. The plan stays in `list_plans` with
     `frozen_at = Some(_)`; body renders from
     `git show <frozen_at>:.trinity/plans/<stem>.md`, not HEAD.
     (Monotone-finished applies to plan-file deletion too: once
     frozen, no subsequent commit can hide or unfinish the plan.)
   - removing `.trinity/plans/<stem>.md` from HEAD with NO freeze
     event ever recorded → plan doesn't exist (no historical
     anchor; absent from `list_plans`).
9. **Sealing invariant (gating)**: once frozen, the plan is sealed
   end-to-end. Regression tests construct a repo with a freeze
   event at commit C and then add commits c1..cN after C that
   would normally affect the plan (plan-file edits, code attributed
   via `Implements:`, working-tree feedback files targeting
   commits in c1..cN). Assert that:
   - the plan's `attribution` map gains no entries for c1..cN
   - the plan's `plan_touches` entries for c1..cN are absent
   - no new gate entries land on the plan
   - `wait_for_work` does not surface the plan for any role
   - the plan is excluded from omitted-`plan_id` inference
   - the frozen plan's slice of the projection (its `Plan`
     record: `attribution`, `plan_touches`, gates, `frozen_at`,
     `lifecycle`) is byte-identical to the same plan's slice
     produced by a fold that stopped at C (validates the freeze
     semantically equals "the plan ends at C from the projection's
     perspective"). Other plans in the same repo legitimately
     differ between the two folds; the invariant is scoped to the
     frozen plan.

   **Late working-tree feedback on pre-freeze commits is displayed,
   not sealed out.** Sealing covers state transitions (attribution,
   plan_touches, gates, lifecycle), not disk-state rendering. A
   regression test writes a working-tree feedback file at
   `.trinity/feedback/<stem>/<pre-freeze-sha>/<agent>.md` AFTER the
   finalize commit lands. Assert: the plan remains `state:
   "finished"`, the feedback appears in the plan's timeline render
   for that commit (because it's just disk state at that path), no
   new gate entry is computed for the frozen plan, and
   `wait_for_work` still does not surface the plan. This is the
   correct semantics for working-tree feedback as a *location* (a
   note about commit X) rather than as an *event* with a wall-clock
   ordering: the fold has no clock for working-tree files, only git
   does, and the file's logical position is the SHA it targets.
10. Plan inference excludes finished plans. `wait_for_work`,
   `get_context`, and any other surface that resolves "the current
   plan" when no `plan_id` is supplied treats only plans with
   `state == active` as candidates. A repo with one active plan
   and any number of finished plans still resolves
   unambiguously to the active one; a `state: finished` plan is
   visible in `list_plans` but never picked up as the inferred
   default.

### Feedback path

11. Working-tree feedback at `.trinity/feedback/<stem>/<sha>/<agent>.md`
    is parsed; the legacy `commits/<sha>/<agent>.md` shape is
    silently ignored (drops to the same fate as any unrecognised
    file under `.trinity/feedback/`). No warning, no detector —
    Trinity has forgotten the legacy layout existed.

### `Phase` enum

16. The `Phase` enum is removed from the codebase (Phase 6 of
    this plan).
    Production code computes posture from `CommitKind` via the
    `current_posture` helper rather than reading a stored variant.
    No `Phase::Done` consumer remains.

### CLI / daemon contract (a few items the daemon owes the CLI)

17. **Re-finalize refusal exposed.** The daemon's plan-detail
    response exposes `state: "finished"` so the CLI can refuse
    re-finalize against an already-frozen plan. A `finish-preview`
    endpoint (or equivalent — exact shape during implementation)
    returns "would `trinity finish` succeed" + a structured
    refusal reason when not, with `already_finished` as one
    enumerated reason. Test: a frozen plan's `finish-preview`
    returns the refusal reason; an active plan with a fully
    approved gate returns "would succeed."
18. **Snapshot rendering reads from `frozen_at`.** The daemon's
    plan-detail and per-cycle endpoints render finalize-snapshot
    file contents from `git show <frozen_at>:.trinity/finished/<stem>/`,
    not from HEAD's tree. Test: with a frozen plan whose snapshot
    files have been deleted from HEAD, the API still returns the
    snapshot contents (read from `frozen_at`).
19. **Post-freeze `Implements:` commits surface as `Unattributed`.**
    A commit with `Implements: <frozen-plan>` after that plan
    froze does not appear in the frozen plan's timeline or
    attribution map. It's visible in any general-purpose commit
    listing (where the projection emits it as
    `CommitKind::Unattributed`), but the frozen plan's wire view
    does not include it. Test: construct the scenario; assert
    the frozen plan's timeline excludes the commit and its
    attribution map does not contain a key for it.

### CLI work (separate stub)

20. The CLI commands (`trinity init`, `trinity finish`,
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
