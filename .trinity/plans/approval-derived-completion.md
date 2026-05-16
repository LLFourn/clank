# Approval-Derived Completion

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
- A `.trinity/finished/<stem>/` directory containing at least one
  APPROVE file is authoritative: the cycle is finished. Later
  working-tree feedback (including REQUEST_CHANGES) does not
  unfinish a finished cycle. The snapshot is only invalidated by a
  newer impl-bearing commit on this plan landing AFTER the finalize
  commit — in which case the cycle reopens and needs a new finalize.
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
(waiting on `PlanState::Done` removal) lands here as Phase 11.

Source design rationale lives in
`.trinity/stubs/approval-derived-completion.md` — this plan does
not duplicate the rationale, only the implementation.

## Definitions

These need to be unambiguous before the implementation can land
cleanly. They are not in the stub; they are this plan's contribution.

### Plan cycle

A **plan cycle** is a range of plan-attributed commits ending at a
finalize commit (the cycle's "closer") or at HEAD if no finalize
has happened yet (the open / current cycle).

Concretely, walking `commit_order` newest-first for a given `PlanKey`:

- The current (open) cycle is everything between the last finalize
  commit (exclusive) and HEAD. If no finalize commit has ever
  touched `.trinity/finished/<plan>/`, the current cycle is the
  entire plan history.
- An **archived cycle** is the range between two consecutive
  finalize commits (or between the start of the plan's history
  and its first finalize commit). The closer of an archived cycle
  is the finalize commit at its newer end.

Cycle boundaries are stable once a finalize commit lands: a
finalize commit only goes away by being un-committed (revert /
reset / rebase), not by feedback files changing in the working
tree. This is the main reason the snapshot lives in git rather
than as live working-tree state.

### Latest implementation-bearing commit for a cycle

The most recent commit in the cycle's range whose `CommitKind` is
`CodeOnly` or `Mixed`. If the cycle has none, the cycle is "no
implementation yet" — not finished, not active in the impl sense,
just open and awaiting implementation.

### Current gate participant

Anyone who has written a feedback file under
`.trinity/feedback/<plan-stem>/<target-sha>/` (note: post-simplification
path; the legacy `commits/` segment is gone).

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

### Finished cycle

A cycle is **finished** iff:

- `.trinity/finished/<plan-stem>/` exists in HEAD's tree, and
- the directory contains at least one file with verdict APPROVE,
  and
- the most recent commit that touched `.trinity/finished/<plan-stem>/`
  is at or after the latest impl-bearing commit for this plan in
  `commit_order`.

That's the entire rule. No checks against the live working-tree
gate. No participant-count check. No post-finalize feedback
override. A reopened cycle (new impl after the latest finalize)
makes the snapshot stale and the cycle is `active` again until a
new finalize lands.

Plan-only commits never finish a cycle. A plan-only-approved cycle
is `ready_to_start_implementation`, not finished.

### Archived cycle

A cycle whose closer (finalize commit) is not the most recent
finalize commit for the plan. Archived cycles are recovered by
walking the commit log for changes to `.trinity/finished/<plan>/`
and reading each historical revision via `git show`. They appear
in the cycle-history view but do not drive any waiting/gate state.

### Plan lifecycle state (wire)

The wire `state` field, currently `active | done`, becomes one of:

- `active` — current cycle has no finalize commit yet
- `finished` — current cycle has a finalize snapshot with at least
  one APPROVE

`archived` is **not** a plan-level state — it's a per-cycle property
in the cycle history. A plan whose latest cycle is `finished` and
then receives a new plan commit transitions back to `active` (with
the previous cycle now archived in the cycle history).

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
  (no `commits/` segment). The mirror `.trinity/finished/` shares
  the shape.
- New `trinity init` command scaffolds `.trinity/` for a new repo
  (directory layout + ignore rule). `--ignore` mode appends the
  ignore rule to the repo-root `.gitignore` instead of creating
  `.trinity/.gitignore`, for projects where the maintainer is
  willing to add an ignore line but doesn't want a `.trinity/`-owned
  config file in their tree.
- New `trinity finish` command makes the finalize commit. Supports
  `--purge` (also git-rm Trinity artifacts for this plan to hide
  Trinity's use), `--squash <message>` (collapse plan-attributed
  commits into one), and `--amend` (re-do the most recent finalize
  with different options).
- New `trinity purge` command shares the history-rewriting engine
  with `trinity finish --purge` but skips the finalize ceremony.
  Strips a plan's `.trinity/` content from history at the
  operator's request, no gate check.
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

**New CLI module**:
- `src/cli/` — new module tree for `trinity init` and
  `trinity finish` subcommands. Wire into the existing CLI dispatch
  in `src/main.rs` (or wherever `trinity serve` is parsed).
- `src/cli/init.rs` — scaffolds `.trinity/plans/`,
  `.trinity/.gitignore`, optionally registers the repo with a
  running daemon.
- `src/cli/finish.rs` — invariant checks, snapshot copy, commit
  creation, `--purge` / `--squash` / `--amend` flag handling.

**New core module**:
- `src/resolution.rs` (or similar) — parser for
  `.trinity/finished/<stem>/<sha>/<agent>.md` paths, gate-check on
  snapshot contents, dispute detection (working-tree REQUEST_CHANGES
  on finalize commit).

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
  tests for `trinity finish` invariant rejection, `finished/`
  snapshot rendering, snapshot-overrides-live-feedback behavior,
  `--purge` / `--squash` / `--amend` modes.
- `src/disk_snapshot.rs` unit tests — `done_move` rename cases,
  `done_counterpart_exists` cases; new finalize-snapshot ingestion.
- `src/projection.rs` unit tests — `plan_path_at_*` cases,
  `waiting_*_done_move`, `commit_kind_for` done-move case; new
  cycle-derivation tests including post-finalize feedback
  (must not unfinish).

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

### Phase 2 — Finalize snapshot reader (additive)

Introduce the new `.trinity/finished/<stem>/` reader without
removing any existing done-move code.

- New parser `disk_format::parse_finalize_path` recognises
  `.trinity/finished/<stem>/<agent>.md`.
- `disk_snapshot` ingests finalize-snapshot files alongside
  feedback files. Per-plan storage on `Plan`:
  `finalize_snapshot: Option<FinalizeSnapshot>` where
  `FinalizeSnapshot { entries: Vec<FinalizeEntry>, finalize_commit:
  CommitSha }`. The `finalize_commit` is the most recent commit in
  the plan's history that touched `.trinity/finished/<stem>/`.
- Projection: new `is_finished(&Plan, &RepoState) -> bool` per the
  Finished Cycle rule (snapshot present, ≥1 APPROVE, finalize
  commit not preceded by a newer impl commit).
- Wire: new `state: "finished"` value in projection output for
  plans where `is_finished` returns true.
- Tests: snapshot present + APPROVE → finished; snapshot present +
  no APPROVE → still active; snapshot present + later impl commit
  → reopened (active); snapshot directory empty → active.

### Phase 3 — Lifecycle state derivation (additive)

The new `PlanLifecycle` enum exists alongside the legacy
`PlanState::Done`, so behavior can be diffed.

- New enum `PlanLifecycle::{Active, Finished}` in `repo_state.rs`.
  Derived from `is_finished`. Carried as a recomputed field, not
  stored.
- New struct `ArchivedCycleSummary { closer: CommitSha,
  approver_count: u32 }`. Stored on `Plan` as `archived_cycles:
  Vec<ArchivedCycleSummary>` (walked from git log of
  `.trinity/finished/<stem>/`).
- Response builders gain a `lifecycle` field on wire alongside the
  existing `state` field. Tests assert both are present and
  consistent.

### Phase 4 — `trinity init` CLI

Scaffold a new repo for Trinity use.

- New subcommand `trinity init [--repo <path>] [--ignore]`. Default
  is cwd.
- Always creates `.trinity/plans/`. The `.trinity/feedback/`
  directory is created lazily by the daemon on first feedback
  write; `trinity init` doesn't materialise it.
- Default mode (no `--ignore`): creates `.trinity/.gitignore`
  containing:
  ```
  # Working-tree feedback is local until `trinity finish` snapshots
  # it into .trinity/finished/.
  feedback/
  ```
- `--ignore` mode: instead of creating `.trinity/.gitignore`,
  appends `.trinity/feedback/` to the repo-root `.gitignore`
  (creating the root file if absent, deduping if the line is
  already present). This is the "maintainer is willing to ignore
  some Trinity working state but doesn't want a `.trinity/.gitignore`
  file in their tree" mode — useful for soft-introducing Trinity
  into a project whose owner isn't adopting it wholesale.
- Either mode refuses to overwrite an existing
  `.trinity/.gitignore` with different contents (clear error
  prompts the user to delete or merge manually).
- Either mode warns if the repo-root `.gitignore` already contains
  a line that ignores all of `.trinity/` — this would hide
  Trinity's tracked artifacts (plans, finished snapshots) too, and
  the operator probably didn't intend it.
- Optionally calls the daemon (if running on localhost) to register
  the repo, so `start_plan` works immediately after.
- Exits non-zero with a clear message if the path is not a git
  worktree.

### Phase 5 — `trinity finish` CLI (no flags)

The core finalize ceremony. No `--purge`/`--squash`/`--amend` yet.

- New subcommand `trinity finish <plan-id-or-stem>`. Argument is
  optional if invoked from a directory under a Trinity-registered
  repo with exactly one in-flight plan.
- Invariant checks (refuse to proceed if any fail; each failure
  prints what's missing):
  - Repo has a Trinity-registered plan with this id.
  - The plan has at least one impl-bearing commit.
  - The live working-tree gate for the latest impl-bearing commit
    is fully approved (≥1 APPROVE, 0 REQUEST_CHANGES, no
    unmarked/ambiguous, every participant who has voted has
    approved).
  - Working tree is clean enough to commit
    (`.trinity/finished/<stem>/` writes won't conflict with
    uncommitted changes).
- If checks pass:
  - Remove any existing `.trinity/finished/<stem>/` contents.
  - Copy each working-tree feedback file from
    `.trinity/feedback/<stem>/<latest-impl-sha>/*.md` into
    `.trinity/finished/<stem>/<agent>.md`. The copy contains only
    the verdict + body (Trinity's parsed feedback format) — no SHA
    metadata.
  - `git add .trinity/finished/<stem>/`.
  - `git commit -m "Finish <plan-stem>"` (commit message
    customizable via `-m`).
- Idempotent: re-running `trinity finish` on an already-finished
  plan with no new impl commits is a no-op (prints "already
  finished"). With new impl commits, it makes a new finalize
  commit.

### Phase 6 — `trinity finish` flags: `--amend`, `--squash --purge`

These two are tractable: `--amend` rewrites a single commit at HEAD,
and `--squash --purge` collapses history into one commit and
removes Trinity artifacts in the same operation (no per-commit
history rewrite needed).

**`--amend`**: HEAD must be a finalize commit (i.e., it most
recently touched `.trinity/finished/<stem>/`). `git commit --amend`
carrying the same logic as Phase 5 plus any combined flags. Used
when you ran `trinity finish` and realised you wanted `--squash`
or `--purge` after all.

**`--squash "<message>"`**: walk back to find the earliest commit
attributed to this plan. Verify no foreign (other-plan /
unattributed) commits are interleaved between that commit and HEAD;
refuse with a clear error if there are. Soft-reset to the parent of
the earliest plan-attributed commit and re-commit the entire diff
with the supplied message. The result is one commit containing all
plan-attributed work.

**`--squash --purge`**: same as `--squash`, but the recommitted tree
omits everything under `.trinity/` for this plan (no `<stem>.md`,
no `.trinity/finished/<stem>/`, no feedback artifacts). Net effect:
one commit containing only the code changes the plan produced, with
no externally visible trace that Trinity was used.

### Phase 7 — `trinity finish --purge` (history rewriting)

`--purge` without `--squash` is the hard mode: preserve the
per-commit history of the plan, but rewrite each plan-attributed
commit to strip its `.trinity/` content. Mixed commits (code +
plan-revision in one commit) need special handling — keep the code,
drop the `.trinity/` paths, preserve the commit message and author.

Implementation approach (call out as a deliberate decision):

- Walk `commit_order` for the range from the earliest plan-attributed
  commit to HEAD.
- For each commit in the range, decide one of:
  - **Drop**: commit's tree only changed `.trinity/` paths for this
    plan. Skip the commit entirely (parent chain skips over it).
  - **Keep verbatim**: commit didn't touch `.trinity/` for this plan
    (foreign commit, or pure code commit). Reuse as-is.
  - **Rewrite**: commit touched both `.trinity/` (for this plan)
    and other paths. Build a new tree omitting the plan's
    `.trinity/` entries, but keeping everything else. Reuse the
    author, message, and timestamp. New parent is the previous
    rewritten commit (or its dropped predecessor).
- Implementation via libgit2 (already in `git_io`): construct new
  tree + commit objects, then update the branch ref. Avoid
  `git filter-branch` (deprecated) and `git filter-repo` (external
  dep).

Refuse to proceed (clear error message in each case) if:
- The range contains a merge commit (handling merges correctly
  through a tree rewrite is out of scope).
- The working tree is dirty.
- The current branch is not the same as where the plan was started
  (we don't want to rewrite shared branches).
- The branch is protected (operator opt-in via
  `--allow-rewrite-protected` is fine; default is refuse).

#### `--purge` edge cases (mandatory test coverage in Phase 7)

The complexity of `--purge` justifies a dedicated test list. Every
case below must have an explicit regression test. Each test
constructs a small repo with the named commit shape, runs
`trinity finish --purge`, and asserts the final history matches
expectation.

1. **Pure plan-only commit**: commit touched only
   `.trinity/plans/<stem>.md`. Result: dropped from history.
2. **Pure code commit, plan-attributed**: commit touched only
   non-`.trinity/` paths but was attributed to this plan via
   `Implements: <stem>` trailer (or equivalent). Result: kept
   verbatim, included in the post-purge history.
3. **Mixed commit (code + plan revision)**: commit touched both
   `.trinity/plans/<stem>.md` and code. Result: rewritten — code
   changes preserved, plan-file change removed. Commit message,
   author, timestamp preserved. SHA changes (necessarily).
4. **Multiple mixed commits in sequence**: c1 mixed, c2 mixed, c3
   pure-code. Result: c1 and c2 each rewritten independently; c3
   kept verbatim; parent chain links c1' → c2' → c3' correctly.
5. **Interleaved foreign commit**: c1 plan, c2 (foreign, touches
   unrelated code), c3 mixed. Result: c1 dropped, c2 kept (foreign
   commits are never rewritten), c3 rewritten. New chain: c2 →
   c3'. The foreign commit's parent is c1's parent (since c1 was
   dropped).
6. **Foreign commit between plan-only and finalize**: c1 plan, c2
   foreign, c3 impl, c4 finalize. Result: c1 dropped, c2 kept, c3
   kept verbatim (pure code, no .trinity touches), c4 kept
   verbatim (the finalize snapshot is preserved under `--purge`;
   only `--squash --purge` strips it — see case 7).
7. **The finalize commit at HEAD**: commit touched only
   `.trinity/finished/<stem>/`. Under `--purge`, the finalize
   snapshot is the durable record and is kept. Under
   `--squash --purge`, the finalize snapshot is dropped along with
   everything else under `.trinity/` for this plan, leaving one
   code-only commit. This split is the plan's intentional choice:
   `--purge` preserves auditability of the resolution;
   `--squash --purge` is the "Trinity used internally, externally
   invisible" mode.
8. **Multiple plans in the repo**: plan `foo` and plan `bar` both
   active. Running `trinity finish foo --purge` only rewrites
   commits that touched `.trinity/plans/foo.md` or
   `.trinity/feedback/foo/` or `.trinity/finished/foo/`. Commits
   that only touched `bar`'s artifacts are foreign (kept as-is).
   Mixed `foo`+`bar` commits are rewritten to strip foo but keep
   bar.
9. **Empty `git commit --allow-empty` impl commit**: a deliberately
   empty commit attributed to this plan (used in the no-impl-
   workflow workaround). Result: kept verbatim (still attributable,
   no .trinity to strip).
10. **Plan intro commit also creates the `.trinity/plans/`
    directory**: the first plan commit ever. Result: drop the
    plan file; the directory itself stays in the working tree as
    long as some other artifact references it.
11. **Working tree dirty**: refuse before any rewriting begins.
    Original branch state untouched.
12. **Merge commit in range**: refuse with clear message
    ("`--purge` cannot rewrite history containing merge commits;
    rebase first or use `--squash` instead").
13. **Re-running `--purge` after `--purge`**: idempotent. If the
    plan's history already has no .trinity touches, the rewrite is
    a no-op.
14. **`--amend --purge` on a finalize commit**: amend HEAD finalize
    to also strip the most-recent plan-touching commit if it's
    HEAD's parent. Otherwise, refuse — `--amend` only rewrites
    HEAD.

### Phase 8 — `trinity purge` standalone command

The `--purge` machinery from Phase 7 is also useful outside the
finalize context. Operators may want to strip a plan's Trinity
artifacts from history without making a finalize commit:

- the plan was abandoned mid-flight and shouldn't leave a trace
- the operator decided Trinity wasn't the right tool for this
  particular work after all
- post-finalize cleanup that the original `trinity finish` didn't
  ask for

Subcommand: `trinity purge <plan-id-or-stem>`. Same history-
rewriting engine as `trinity finish --purge`; just no finalize
commit and no gate check. The semantics are "remove every commit's
.trinity/<this-plan>/ content from history; preserve code changes
on mixed commits."

Flags:

- `--squash "<message>"`: collapse plan-attributed commits into one
  (same interleaving rules as `trinity finish --squash`).
- `--amend`: amend HEAD if HEAD already touches this plan's
  artifacts.
- No `--purge` flag — that's the command's whole purpose.

Safety:

- Refuse on dirty working tree.
- Refuse on protected branches without `--allow-rewrite-protected`.
- Refuse if `.trinity/finished/<stem>/` exists in HEAD and
  `--squash` is not supplied — purging without `--squash` would
  preserve the finalize snapshot but leave it pointing at history
  that no longer has the impl commits, which is confusing. Either
  also use `--squash` (which strips everything in one commit) or
  add `--drop-finalize` (explicit opt-in to remove the finalize
  snapshot too).
- Prompt for confirmation by default; `--yes` skips the prompt
  for scripted use.

All 14 `--purge` edge cases from Phase 7 apply identically. The
test suite at `tests/cli_purge.rs` re-runs every case against
`trinity purge` to confirm the engine behaves the same standalone
as it does behind `trinity finish --purge`.

### Phase 9 — Wire/UI cutover

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

### Phase 10 — Delete `PlanState::Done` and `DoneMove` types

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

### Phase 11 — Delete the `Phase` enum

Picked up from `architecture-tech-debt-sweep`, which deferred this
piece pending the `PlanState::Done` deletion that lands in Phase 10
of this plan. The deferral was sequencing, not scope; both halves
land in this sweep.

- Delete `Phase::Done` and the whole `Phase` enum.
- Replace `phase_for` callers with a `current_posture(&Plan)`
  helper: `PlanOnly | Mixed → Planning`, `CodeOnly → Implementing`.
- Wire `phase` field becomes a function of the latest reviewable
  commit's `CommitKind`, not a stored enum.
- Frontend `phase: "done"` rendering is already moot after Phase 9
  (the wire never emits it once `PlanState::Done` is gone); this
  phase deletes the type as well.

## Migration

Two migrations: stale `done/` files and the feedback-path rename.

### Stale `.trinity/plans/done/` files (working tree)

A repo upgrading past Phase 10 will have a `.trinity/plans/done/`
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

### T1 — Plans without implementation never finish

The Finished Rule requires the snapshot to be taken against an
impl-bearing commit. A "decide on X" plan with only plan-only
commits cannot finish without first making an empty
`git commit --allow-empty` impl commit.

Is that the intent? Two readings:

**(a)** Plans should produce implementations; a plan with no impl
isn't really "done," even if all the analysis is captured.
Operator adds an empty impl commit if they want to mark it
finished. This is the stub's apparent intent.

**(b)** Plan-only-approved plans should also be able to finish.
Allow `trinity finish` to target the latest plan-touching commit
when no impl exists, and surface the resulting state distinctly.

(a) is cleaner and matches the stub. (b) accommodates a real
workflow (analysis-only plans) at the cost of two completion
predicates. This plan tentatively goes with (a); call out if (b)
is wanted.

### T2 — `Archived` is a per-cycle state, not a plan-level state

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

### T3 — `PlanWorktreeStatus::MissingActivePlanFile` semantics shift

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

### T4 — Finalize commit forgery

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

### T5 — `--purge` history rewriting changes SHAs

`trinity finish --purge` rewrites commit history to strip
`.trinity/` content from mixed commits. This changes the SHAs of
every rewritten commit. Anyone who has the pre-rewrite branch
checked out, has open PRs against those commits, or has built CI
artifacts referencing those SHAs will see breakage.

This is intentional — `--purge` is a deliberate operation. The
CLI should warn loudly before doing the rewrite, and refuse on
protected branches without explicit opt-in.

### T6 — `--squash` with interleaved foreign commits

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

### T7 — Wire compatibility window

The cutover removes `state: "done"`, `phase: "done"`, the
`api_move_to_done` endpoint, and a frontend button. Any external
consumer (LSP integrations, third-party scripts, the MCP shim if
it doesn't share types) sees the breaking change at once.

Trinity is single-tenant and the SPA is co-versioned. The MCP shim
already shares types with the daemon. Acceptable risk.

### T8 — `--purge` keeps finalize snapshot but loses plan body

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
8. Plan-only approval never produces lifecycle `finished` —
   `wait_for_work` returns `ready_to_start_implementation` for
   that plan if the current cycle has no impl yet.

### Feedback path

9. Working-tree feedback at `.trinity/feedback/<stem>/<sha>/<agent>.md`
   is parsed; old `commits/<sha>/<agent>.md` is not.
10. Legacy-path files in the working tree surface as a warning,
    not a silent drop.

### CLI: `trinity init`

11. `trinity init` in a git worktree creates `.trinity/plans/`,
    `.trinity/feedback/`, and `.trinity/.gitignore` containing the
    `feedback/` ignore rule.
12. `trinity init` refuses if `.trinity/` already exists with
    conflicting contents.

### CLI: `trinity finish` (core)

13. `trinity finish <plan>` refuses with a clear message when the
    live gate is not fully approved.
14. `trinity finish <plan>` on a fully approved gate creates
    `.trinity/finished/<stem>/<agent>.md` for each approver and
    commits the change.
15. `trinity finish <plan>` is idempotent on an already-finished
    plan with no new impl commits.

### CLI: `trinity finish` flags

16. `--amend` re-runs the finalize on HEAD when HEAD is a finalize
    commit; refuses otherwise.
17. `--squash <msg>` collapses contiguous plan-attributed commits
    into one with the supplied message; refuses on interleaved
    foreign commits.
18. `--squash --purge <msg>` collapses AND strips `.trinity/`
    content for this plan in the resulting commit.
19. `--purge` (without `--squash`) rewrites each plan-attributed
    commit to remove `.trinity/` content for this plan, preserving
    code changes, author, message, timestamp.
20. `--purge` keeps `.trinity/finished/<stem>/` in HEAD (proof of
    resolution); `--squash --purge` strips it.

### `--purge` edge cases (mandatory regression tests)

21. Every case 1–14 in the Phase 7 `--purge` edge cases section
    has an explicit test in `tests/cli_finish_purge.rs` (or
    equivalent module).

### Migration

22. Existing repos with `.trinity/plans/done/*.md` files surface
    those files in `plan_conflicts` with a migration message; no
    silent drop.
23. `tests/end_to_end.rs` includes a regression test that boots a
    repo containing `.trinity/plans/done/legacy.md`, verifies the
    conflict surfaces, and verifies the legacy file is not picked
    up as an active plan.
24. Working-tree feedback at the legacy `commits/` path surfaces a
    one-shot warning per rebuild.

### `Phase` enum

25. The `Phase` enum is removed from the codebase (Phase 11).
    Production code computes posture from `CommitKind` via the
    `current_posture` helper rather than reading a stored variant.
    No `Phase::Done` consumer remains.

### CLI: `trinity purge`

26. `trinity purge <plan>` removes that plan's `.trinity/` content
    from history without making a finalize commit. Same history-
    rewriting semantics as `trinity finish --purge`.
27. `trinity purge --squash <msg>` collapses plan-attributed
    commits into one with the supplied message, stripping
    `.trinity/` content for the plan.
28. `trinity purge` refuses without explicit confirmation on
    protected branches and on dirty working trees, and refuses to
    leave an orphan finalize snapshot behind without `--squash` or
    `--drop-finalize`.

### Acceptance: all `--purge` edge cases apply to both subcommands

29. The 14 `--purge` edge cases in the Phase 7 list apply to
    `trinity purge` identically; `tests/cli_purge.rs` mirrors
    every case from the `trinity finish --purge` suite.

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
