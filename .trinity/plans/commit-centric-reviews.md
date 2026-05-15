# Commit-Centric Reviews

## Summary

Collapse Trinity's `planning` / `implementing` phase split into a single
commit-keyed review model. Every reviewable unit is a commit; feedback
addresses the commit's SHA; "plan review" vs "implementation review"
becomes a UI/agent prompt derived from a `commit_kind` field, not a
storage axis.

The plan-record (`Plan` in memory, the markdown file on disk, `PlanId`
on the wire) survives unchanged as the anchor — it groups a sequence
of commits and stays the unit of identity. What goes away is the
separate `plan_feedback` / `impl_feedback` maps, the separate
`plan_gate` / `impl_gate` derivations, the held-feedback queue, and
the two parallel sets of `waiting_on` reasons that mirror each other.

## Problem

Trinity today encodes the plan/impl distinction in five places:

1. **Storage.** `Plan` carries `plan_feedback: BTreeMap<…>` and
   `impl_feedback: BTreeMap<…>` as separate maps, keyed by `(CommitSha,
   AgentLabel)` but indexed by which subdirectory the file lived in.
2. **Disk layout.** `.trinity/feedback/<stem>/{plan,impl}/<sha>/<author>.md`
   — the `plan`/`impl` segment exists only to tell the parser which
   map to insert into.
3. **Projection.** `phase_for(plan_path, plan_key, attribution) →
   {Planning, Implementing, Done}` is derived from "is the plan in
   `done/`?" plus "are there code-changing commits attributed to this
   plan?"
4. **Gates.** `plan_gate_for` and `impl_gate_for` each fold their own
   feedback map over the latest plan-touching / impl commit.
5. **Wait reasons.** Twelve `WaitingReason` variants where six are
   pairwise mirrors: `PlanNeedsInitialReview` / `ImplNeedsInitialReview`,
   `PlanNeedsRereview` / `ImplNeedsRereview`,
   `AddressPlanRequestChanges` / `AddressImplRequestChanges`.

A grep for the dichotomy (`plan_feedback|impl_feedback|FeedbackPhase|ReviewPhase|Phase::`)
returns >150 hits across `repo_state.rs`, `projection.rs`,
`runtime.rs`, `ui_response.rs`, `mcp_response.rs`, `server/wait.rs`,
the frontend store, and tests. Every new feature asks "which side is
this?" — for no reason that the storage model captures intrinsically.

The dichotomy is a label, not a structural fact. A commit either
touches the plan file, touches code, or both. That's what `git_io`
already records via `PlanTouchKind` and `has_code_changes`. We're
discarding that information at storage time, then reconstructing a
weaker version of it ("plan phase" vs "impl phase") on every read.

## Target Model

A plan is a record. The plan file at `.trinity/plans/<stem>.md` and
the move to `done/` stay the lifecycle anchors. Within a plan's
history, every commit gets a derived `commit_kind`:

- `plan_only` — touches exactly the plan file and nothing else.
- `code_only` — touches non-plan files attributed to this plan.
- `mixed` — touches the plan file *and* code.
- `done_move` — the rename into `.trinity/plans/done/`.
- `multi_plan` — touches more than one plan file in this repo.
- `unattributed` — nothing relevant to this plan.

Feedback lives at one path:

```
.trinity/feedback/<stem>/commits/<sha>/<author>.md
```

The first line is still `APPROVE` or `REQUEST_CHANGES`. The target
SHA tells you what was reviewed; the parser doesn't need a
plan/impl hint.

Per-commit review gate:

- `changes_requested` — any current feedback for this SHA opens with
  `REQUEST_CHANGES`, OR any current feedback is ambiguous/unmarked.
- `approved` — every plan-wide participant has responded to this SHA,
  zero responses are `REQUEST_CHANGES` or ambiguous/unmarked, and at
  least one reviewer has responded with `APPROVE`.
- `unreviewed` — no reviewer has responded to this SHA yet, OR at
  least one plan-wide participant is missing on this SHA (re-review
  pending).

### Cumulative-participant rule

The plan-wide participant set is **cumulative**: any agent who has
left feedback on any earlier commit in this plan's history is part of
the participant set for every later commit's gate. That is the
important behavior change from today's phase-split model: a reviewer
who participated in plan review is still expected to respond when the
next code commit lands. "Plan reviewer" and "implementation reviewer"
are display labels only.

Worked example:

```
commit A  plan_only   ← codex APPROVE
commit B  code_only   ← alice REQUEST_CHANGES
commit C  code_only   ← alice APPROVE       (codex hasn't voted on C)
   → gate(C): participants = {codex, alice}, approvers = {alice},
              missing = {codex}, state = Unreviewed
              waiting_on = reviewers (codex)
```

Later when codex APPROVEs C:

```
   → gate(C): approvers = {alice, codex}, missing = {}, state = Approved
              waiting_on = master (ReadyToMoveForward, kind = code_only)
```

### Session-level `waiting_on`

Walk `commit_order` newest-first and pick the first commit whose kind
is relevant to this plan (`plan_only` | `code_only` | `mixed` |
`done_move`; skip `multi_plan` and `unattributed`). Branch on that
commit's gate state and kind:

- gate is `unreviewed` (target ≠ `done_move`) → reviewers' turn.
- gate is `changes_requested` (target ≠ `done_move`) → master's turn (address).
- gate is `approved` and kind ∈ {`plan_only`, `code_only`, `mixed`} → master's turn (`ReadyToMoveForward`).
- target is `done_move` → `SessionDone` (terminal; `done_move`
  does not require a review gate — it's master-only post-approval
  bookkeeping).
- Plan-file under `done/` and `done_move` commit observed → `SessionDone`.

That's the entire decision matrix. No `Phase` enum. No `Plan.phase`.
The UI can still display chips ("Plan", "Code", "Mixed") computed
from `commit_kind`; agents still get prompt hints scoped to the
commit kind. None of it is in the storage model.

### Worktree-status preemption

`PlanWorktreeStatus::{Clean, BodyDirty, DoneMovePending,
MissingActivePlanFile}` survives unchanged. It always preempts
gate-driven reasons (today and in the new model). The worktree
states are "the operator has uncommitted intent" — orthogonal to
review gates.

## Goals

1. One feedback path per commit; no `plan/` vs `impl/` directories.
2. One `Plan.commits: BTreeMap<CommitSha, CommitGate>` map; no
   `plan_feedback` / `impl_feedback` split.
3. `WaitingReason` enum collapsed (see table below).
4. `wait_for_work` returns `{plan_id, target_sha, commit_kind,
   work, prompt_hint, locations}` for every review and address case.
5. Agent prompt distinctions preserved: a `plan_only` commit prompts
   "review the approach"; a `code_only` commit prompts "review the
   diff."
6. UI distinctions preserved: timeline rows labelled by `commit_kind`,
   feedback cards attached to the commit they reviewed, plan-vs-code
   styling kept as a display layer.

## Non-Goals

- **Changing the plan-record identity.** `PlanId = <repo>/<stem>.md`
  stays exactly as it is. `Plan.state` (active/done) stays.
- **Multi-plan workflows.** `multi_plan` commits get classified so
  the timeline can show them, but no new orchestration is added for
  cross-plan reviews — they don't drive `waiting_on` or generate
  review work.
- **Backwards compatibility for old feedback files.** This is a
  one-shot cutover; pre-migration files in `plan/` and `impl/`
  subdirectories are ignored. Trinity-on-trinity historical reviews
  will be invisible to the daemon after the cut. (Acceptable — the
  reviews already lived their lives in the development workflow.)
- **Removing the plan file's special status.** The plan markdown
  remains the anchor: `start_plan` still creates it, watcher still
  detects its move, basename/stem still derive `PlanId`.

## Design

### `commit_kind` classification

`git_io` already produces `PlanTouchKind` (`Intro` / `Revision` /
`DoneMove`) plus `has_code_changes: bool`. The new classifier folds
these per-commit:

```rust
pub enum CommitKind {
    PlanOnly,    // plan touch ∧ ¬has_code_changes ∧ single-plan
    CodeOnly,    // ¬plan touch ∧ has_code_changes ∧ attributed
    Mixed,       // plan touch ∧ has_code_changes ∧ single-plan
    DoneMove,    // plan_touch == DoneMove
    MultiPlan,   // ≥2 distinct plans touched
    Unattributed,
}
```

`AttributionResult::Attributed` carries `kind: CommitKind` (the
existing `plan_touch` and `has_code_changes` fields can stay on
the type for legibility OR collapse into `kind` — caller's choice
at impl time; either is fine since `kind` is derived from them).

Commits that only mutate `.trinity/feedback/` files (i.e. review
writes) classify as `unattributed` — they're a side-effect of the
review pipeline, not work to be reviewed. Same rule applies to
edits under `.trinity/cache/`.

### `CommitGate`

Per-commit folded review state:

```rust
pub struct CommitGate {
    pub state: CommitGateState,           // Unreviewed | Approved | ChangesRequested
    pub participants: Vec<AgentLabel>,    // cumulative across plan history
    pub approvers: Vec<AgentLabel>,       // on this SHA
    pub requesters: Vec<AgentLabel>,      // on this SHA
    pub ambiguous: Vec<AgentLabel>,       // on this SHA, Unmarked verdict
    pub missing: Vec<AgentLabel>,         // participant set minus responders
    pub feedback: BTreeMap<AgentLabel, Feedback>,
}
```

`Plan.commits: BTreeMap<CommitSha, CommitGate>` replaces both
`plan_feedback` and `impl_feedback`. Iteration order is SHA-lex; the
existing `commit_order: Vec<CommitSha>` field on `RepoState` keeps
chronological order.

`CommitGate` is derived with a cumulative-participant fold over
`commit_order`: before evaluating a target commit, collect every
agent who has left feedback on any earlier commit for the plan, then
union with agents who responded on the target commit itself. The
target commit is `Approved` only when that full set has non-ambiguous,
non-request-changes feedback on the target AND at least one of those
votes is `APPROVE`.

Only commits relevant to this plan AND reviewable get a `CommitGate`
entry. The single invariant: **`done_move`, `multi_plan`, and
`unattributed` never have a `CommitGate`**, never appear in
`Plan.commits`, never contribute to the cumulative-participant set,
and are never returned by `wait_for_work` as a review target. They
appear in the timeline (so the UI can render them) but as commit
events only, never with an attached review gate.

The session reaches `SessionDone` via two independent signals: the
plan path is under `.trinity/plans/done/` AND a `done_move` commit
has been observed on the plan's history. No gate concept is involved.

### Feedback path parsing

`.trinity/feedback/<stem>/commits/<sha>/<author>.md`. The
`parse_feedback_path` function loses its `FeedbackPhase` branch and
returns `FeedbackPath { plan_key, target_sha, author }` (no phase).

Held feedback (today's `held_plan_feedback`) goes away as a separate
concept. Today reviewers could drop a flat-drop file before a target
SHA existed (e.g. while the plan-body was dirty) and the runtime held
it until the revision committed, then renamed it under `<sha>/`. In
the commit-centric model the plan necessarily already has a SHA
before any review can target it — `wait_for_work` always returns a
SHA, and a flat-drop file with no SHA in its path is now just a
malformed feedback file that the parser rejects.

The watcher's auto-organize sweep in `runtime.rs` (currently
~line 487, draining `held_plan_feedback` after a plan revision
commits) is deleted outright. The `HeldFeedback` struct, the
`held_plan_feedback` field on `Plan`, the timeline's `HeldFeedback`
event, and the corresponding watcher branches all go away.

### Projection collapse

```rust
fn waiting_on(commits: &BTreeMap<CommitSha, CommitGate>,
              commit_order: &[CommitSha],
              kinds: &BTreeMap<CommitSha, CommitKind>,
              worktree_status: PlanWorktreeStatus,
              plan_state: PlanState) -> WaitingOn
```

Worktree-status preempts as today (`BodyDirty` → commit plan
revision; `DoneMovePending` → commit done move; etc.). Then walk
`commit_order` newest-first and return based on the latest relevant
commit's `CommitGate`.

### `WaitingReason` collapse

Twelve → seven:

| Old                                                                      | New                          |
|--------------------------------------------------------------------------|------------------------------|
| `PlanNeedsInitialReview` / `ImplNeedsInitialReview`                      | `CommitNeedsReview`          |
| `PlanNeedsRereview` / `ImplNeedsRereview`                                | `CommitNeedsReview` (same — re-review is the same gate state with more participants) |
| `AddressPlanRequestChanges` / `AddressImplRequestChanges`                | `AddressCommitChanges`       |
| `ReadyToImplement` / `ReadyToFinish`                                     | `ReadyToMoveForward` (description prose disambiguates by `commit_kind`) |
| `CommitPlanRevision`                                                     | (unchanged — worktree-status driven) |
| `CommitDoneMove`                                                         | (unchanged — worktree-status driven) |
| `RestoreOrCommitDoneMove`                                                | (unchanged — worktree-status driven) |
| `SessionDone`                                                            | (unchanged)                  |

Final variants: `SessionDone`, `CommitDoneMove`,
`RestoreOrCommitDoneMove`, `CommitPlanRevision`,
`AddressCommitChanges`, `ReadyToMoveForward`, `CommitNeedsReview`.

`description_for(role, reason, agents, commit_kind)` continues to
disambiguate via the commit_kind in its prose (e.g. "Plan revision
awaiting review from codex." vs "Implementation commit awaiting
review from codex." — same underlying reason, different rendered
text). `commit_kind` becomes a new parameter to `description_for`.

`expected_action` map collapses correspondingly:

```rust
SessionDone => "none",
CommitDoneMove => "commit_done_move",
RestoreOrCommitDoneMove => "restore_or_commit_done_move",
CommitPlanRevision => "commit_plan_revision",
AddressCommitChanges => "address_commit_changes",
ReadyToMoveForward => "move_forward",
CommitNeedsReview => "review_commit",
```

### `wait_for_work` cutover

`wait.rs::compute_match` collapses: today it picks plan_gate or
impl_gate based on session_phase. New model picks the latest
relevant commit's gate directly. `caller_already_voted` becomes a
single lookup into the latest relevant commit's gate with this
explicit rule:

- Author has `APPROVE` on latest relevant SHA → re-wake suppressed
  (their vote stands; remaining wait is on someone else).
- Author has `REQUEST_CHANGES` on latest relevant SHA → re-wake
  suppressed (their vote stands; address-side moves to master).
- Author has `Unmarked`/ambiguous feedback on latest relevant SHA
  → re-wake NOT suppressed. The gate still treats them as missing
  (their file is malformed and needs the marker fixed); they should
  re-poll and notice.
- Author has no feedback on latest relevant SHA → re-wake NOT
  suppressed (they're a cumulative participant from an earlier
  commit and the gate is asking for their vote).

Concretely: `caller_already_voted` returns true iff
`commits[latest_relevant].approvers.contains(author) ||
commits[latest_relevant].requesters.contains(author)`. The
`ambiguous` and `missing` sets do NOT suppress re-wake.

`derive_locations` collapses similarly:
- `CommitNeedsReview` → `[commits/<sha>/<author>.md]` (one path)
- `AddressCommitChanges` → `[every RC feedback file for <sha>]`
  plus the plan file if `commit_kind` ∈ `{plan_only, mixed}`
  (because addressing plan-side RCs means revising the plan file)
- `ReadyToMoveForward` → `[<plan file>]`
- worktree-status variants → `[<plan file>]` (unchanged)
- `SessionDone` → `[]`

### Optional `plan_id` inference

The new `wait_for_work` / `get_context` contract makes `plan_id`
optional. Resolution order:

1. **Explicit `plan_id`** — if the caller passes one, use it
   verbatim. Always overrides inference.
2. **Explicit `repo`** — if the caller passes `repo` (basename or
   absolute path), scope the inference to that repo.
3. **Caller cwd** — otherwise resolve cwd via `git rev-parse
   --show-toplevel` (same path the MCP shim already uses for
   `start_plan`).

Within the resolved repo, count active plans (`state == active`):

- **Exactly one active plan** → use it. Single-active inference
  wins; no recency heuristics, no fuzzy matching.
- **Zero active plans** → return `{timed_out: true, no_active_plans:
  true}` (same shape family as today's timeout). The caller has
  nothing to do.
- **Multiple active plans** → return an `ambiguous_plan` error with
  the candidate list:

  ```json
  {
    "error": "ambiguous_plan",
    "message": "multiple active plans; pass plan_id explicitly",
    "candidates": [
      {
        "plan_id": "trinity/foo.md",
        "current_path": ".trinity/plans/foo.md",
        "state": "active",
        "waiting_on": { "role": "master", "reason": "...", "description": "..." }
      },
      { "plan_id": "trinity/bar.md", ... }
    ]
  }
  ```

  Recency is NOT a tiebreaker — Trinity does not pick for the
  caller. The ambiguity error tells the caller to choose.

`tools.rs` schemas for `wait_for_work` and `get_context` drop
`plan_id` from the `required` array, add an optional `repo` field,
and document the inference rules in the tool description. The MCP
shim does not cache `plan_id` (today's behavior preserved). HTTP
`/api/wait_for_work` accepts the same optional shape.

### MCP wire shape

`wait_for_work` response gains `target_sha`, `commit_kind`, and
`prompt_hint`:

```json
{
  "plan_id": "trinity/foo.md",
  "repo": "/Users/llfourn/src/trinity",
  "work": "review_commit",
  "target_sha": "abc123…",
  "commit_kind": "plan_only",
  "prompt_hint": "This commit only changes the plan. Read the plan file at .trinity/plans/foo.md and review the proposed approach.",
  "locations": [".trinity/feedback/foo/commits/abc123/codex.md"]
}
```

`get_context` returns `commits: [{sha, kind, gate, feedback[]}]` in
chronological order, plus `latest_relevant_commit` to point at the
one that drives `waiting_on`. The old `plan_feedback` / `impl_feedback`
arrays are gone.

### Producer checklist (phase 2)

Every place that produces, advertises, or asserts on a `plan/`-or-
`impl/`-segmented feedback path must be touched in phase 2. This
list is the punch list — if it grows during impl, add to this list
before merging.

- `src/tools.rs`: tool descriptions for `wait_for_work` (the `work`
  vocabulary and `locations` examples) and `get_context` (the
  feedback-shape paragraph). Schemas updated for optional `plan_id`
  + `repo`.
- `src/server/wait.rs`: `derive_locations`, `feedback_path`,
  `rc_feedback_paths`, `caller_already_voted`, `Candidate`
  (replace `plan_gate`/`impl_gate`/`plan_target`/`impl_target` with
  one `latest_relevant_commit_gate` and `latest_relevant_target`).
- `src/server/mcp.rs`: any tool-dispatch surfaces that mention the
  phase axis.
- `src/server/http.rs`: route handlers + wire tests under
  `wire_tests`; the `/api/wait_for_work` test fixtures that assert
  on the old `work` vocabulary.
- `src/mcp_response.rs`: `feedback_entries`, `pr_hint_value`,
  `review_target`, `write_feedback`, `timeline_value`, `gate_value`
  — all currently use the plan/impl axis.
- `src/ui_response.rs`: `feedback_entries`, `held_feedback_entries`
  (deleted), `timeline_value`, `plan_page_with_reader`, plans-list
  shape (`/api/plans`).
- `src/projection.rs`: `phase`/`phase_for`/`plan_gate_for`/
  `impl_gate_for`/`waiting_from_gate`/`expected_action`/
  `description_for`/`last_activity_ts_for` (the held_feedback
  parameter goes away).
- `src/runtime.rs`: `upsert_feedback`, `upsert_feedback_at_target`,
  `FlatDropSnapshot`, `latest_attributed_commit`, the
  held-feedback-organize sweep (delete), all `FeedbackPhase`
  imports.
- `src/disk_format.rs`: `parse_feedback_path` + `FeedbackPhase`
  enum + all unit tests.
- `src/disk_snapshot.rs`: feedback ingest paths; the disk-snapshot
  rebuilder.
- `src/runtime_snapshot.rs`: `PlanSnapshot` fields (`plan_feedback`/
  `impl_feedback`/`held_plan_feedback` → `commits`), `to_repo_state`.
- `src/repo_state.rs`: `Plan` struct fields, `TimelineEvent::Review`
  (drop `phase`), `TimelineEvent::HeldFeedback` (delete),
  `TimelinePhase` (delete), `Phase` (delete), `HeldFeedback`
  (delete), `WaitingReason` (collapse), `digest` (update digest
  fields).
- `src/review_state.rs`: `ReviewPhase` enum (delete),
  `ReviewGateDecision.phase` field (delete).
- `frontend/src/api.rs`: `PlanDetail`, `TimelineEvent::Review`
  (drop `phase`), `TimelineEvent::HeldFeedback` (delete),
  `FeedbackRow`, `WaitingOn` reason strings.
- `frontend/src/components/timeline.rs`: chip-rendering moves to
  commit row; review-row phase formatting deleted.
- `frontend/src/components/feedback.rs` (or equivalent):
  per-commit feedback cards; phase-keyed CSS classes renamed to
  commit-kind-keyed.
- `frontend/style.css`: rename phase-keyed classes to
  commit-kind-keyed; delete `held-feedback*` blocks.
- `tests/end_to_end.rs`: every JSON assertion on `plan_feedback`,
  `impl_feedback`, `held_plan_feedback`, `phase`, `review_target`'s
  `phase` field, `wait_for_work`'s old `work` vocabulary. Rewrite
  against `commits[]` and the new wire shape.

### HTTP / SPA

- `/api/plan/{repo}/{stem_md}` returns the new shape (one
  `commits[]`, no `plan_feedback`/`impl_feedback`/`held_plan_feedback`).
- Revision route and commit-diff route are unchanged structurally
  — they're already commit-SHA-addressed.
- Frontend timeline already renders commits + reviews in order;
  the wire change is: `TimelineEvent::Review` loses its `phase` field
  (the phase concept is dead); chips render on the commit row, not
  the review row.
- `TimelineEvent::HeldFeedback` variant is deleted.
- Frontend store gains a per-commit-kind chip palette; existing
  `timeline-commit-plan`/`timeline-commit-impl`/`timeline-commit-mixed`
  CSS classes survive, plus a new `timeline-commit-done-move` for the
  `done_move` variant.

### Readiness rule edge cases

After a `plan_only` commit is approved, the master is in
`ReadyToMoveForward` (description: "Plan approved — start
implementation, revise further, or move to done."). There's no
explicit "start implementing" event — the act of committing code IS
the event.

What about code commits that land before any plan commit is approved?
Today that's not strictly enforced (the `phase` derivation flips to
Implementing once any code commit lands regardless of plan-approval),
but the workflow customarily blocks until plan approval. In the new
model:

- If a `code_only` commit lands while the latest plan commit was
  unreviewed, the latest relevant commit is the `code_only` — gate
  `Unreviewed`. Reviewers see it as work; the prompt_hint says "this
  commit is implementation work on a plan that hasn't been approved
  yet" so reviewers can choose to defer or push back.
- A reviewer who wants to push back via "the plan itself isn't
  approved yet" leaves `REQUEST_CHANGES` on the code commit with
  prose, OR retroactively votes on the earlier `plan_only` commit.
  Both are valid; the gate recomputes on either signal.
- This is looser than today's "plan must be approved first" implicit
  gate. The trade-off is honest: Trinity stops enforcing workflow and
  surfaces it instead. Tighter enforcement can be added later as a
  separate `wait_for_work` policy flag, not a storage axis.

## Phases

Two phases. Phase 1 lands a pure typed refactor with no disk-format
or wire change; phase 2 cuts disk + wire + retires the dead code in
one shot. We do not implement a read-old/write-new bridge because
"we don't care about backwards compat" — operators with pre-cut
disk state lose the historical reviews but keep their plans and
commits.

### Phase 1 — Internal `commit_kind` + `CommitGate` (additive)

Files: `src/repo_state.rs`, `src/git_io.rs` (or `attribution.rs` if
present), `src/disk_snapshot.rs`, `src/projection.rs`.

- Add `CommitKind` enum + classifier function (pure, over
  `(plan_touches[sha], attribution[sha])`).
- Add `CommitGate` struct + `commits: BTreeMap<CommitSha, CommitGate>`
  field on `PlanSnapshot` / `Plan`. Populate it from the **existing**
  `plan_feedback` / `impl_feedback` maps (union, keyed by SHA) plus
  the cumulative-participant fold.
- Add `commit_kind_for(sha, plan_key, plan_touches, attribution) →
  CommitKind` for use by the future projection.
- Disk paths unchanged. Wire unchanged. Frontend untouched.
- Tests for `CommitKind` classification of every variant and for
  the cumulative-participant `CommitGate` fold.

After phase 1, `Plan.commits` is the same information that
`plan_feedback ∪ impl_feedback` represented, but reshaped per-commit
with the new gate fold. Nothing reads it yet at runtime. This phase
is a no-op for callers and exists to land the types and the gate
algorithm in isolation so they can be tested without touching the
hot path.

### Phase 2 — Cutover (disk + wire + retire)

Files: `src/disk_format.rs`, `src/runtime.rs`, `src/server/wait.rs`,
`src/server/mcp.rs`, `src/server/http.rs`, `src/tools.rs`,
`src/mcp_response.rs`, `src/ui_response.rs`, `src/repo_state.rs`,
`src/projection.rs`, `frontend/src/api.rs`, `frontend/src/store.rs`,
`frontend/src/components/*.rs`, `tests/end_to_end.rs`.

In one phase:

- `parse_feedback_path` reads only `<stem>/commits/<sha>/<author>.md`.
  `FeedbackPhase` enum is deleted.
- `Plan.plan_feedback` and `Plan.impl_feedback` are deleted;
  `Plan.commits` is now the authoritative storage.
- `Plan.held_plan_feedback`, the `HeldFeedback` struct, and the
  watcher's auto-organize sweep are deleted.
- `Phase` enum and `phase_for` are deleted. `ReviewPhase` is deleted.
- `WaitingReason` collapses per the table above (twelve → seven).
- `plan_gate_for` / `impl_gate_for` are deleted; replaced by a
  single `latest_relevant_commit_gate(plan, state) → Option<&CommitGate>`.
- `wait_for_work` response shape updated: `target_sha`,
  `commit_kind`, `prompt_hint` added; `work` vocabulary updated.
- `get_context` / `plan_detail` JSON returns `commits[]` instead of
  split `plan_feedback` / `impl_feedback` / `held_plan_feedback`.
- Frontend consumes new shape: `TimelineEvent::Review.phase` field
  removed; chip-rendering moves to the commit row.
- All tests that asserted on `plan_feedback` / `impl_feedback` /
  `held_plan_feedback` JSON are rewritten against `commits[]`.

The cutover is one phase deliberately. The "land cutover in green,
then retire in a separate PR" pattern would require keeping
`plan_feedback` / `impl_feedback` alive through the cutover — which
just doubles the work because nothing reads them after the cutover
finishes. Skip the half-step.

## Acceptance Criteria

1. `grep -rn "plan_feedback\|impl_feedback\|FeedbackPhase\|ReviewPhase\|held_plan_feedback" src/ frontend/`
   returns zero hits (test fixtures excluded).
2. `Phase` enum and `phase_for` deleted from `repo_state.rs` and
   `projection.rs`.
3. `WaitingReason` enum has seven variants
   (`SessionDone`, `CommitDoneMove`, `RestoreOrCommitDoneMove`,
   `CommitPlanRevision`, `AddressCommitChanges`, `ReadyToMoveForward`,
   `CommitNeedsReview`).
4. `find .trinity -path '*/plan/*' -o -path '*/impl/*'` finds no
   live-traffic feedback files; only the `commits/<sha>/` layout is
   used.
5. `wait_for_work` returns `commit_kind` + `prompt_hint` on every
   work response.
6. `get_context` returns `commits[]` (with per-commit gate +
   feedback) instead of split `plan_feedback` / `impl_feedback`.
7. Frontend timeline renders the same UX as today: commit rows with
   plan/code/mixed/done labels, feedback cards under each commit.
   Chip-rendering happens on the commit row only; review rows carry
   no phase indicator.
8. Tests cover:
   - `commit_kind` classification of every variant
     (`plan_only`, `code_only`, `mixed`, `done_move`, `multi_plan`,
     `unattributed`, plus the `.trinity/feedback`-only edge case);
   - cumulative-participant gate fold (the worked example above
     plus its `REQUEST_CHANGES` and `Unmarked` permutations);
   - readiness rule walking newest-first, skipping `multi_plan` and
     `unattributed`;
   - `wait_for_work` prompt-hint shape for each kind;
   - `done_move` short-circuits to `SessionDone` without a review gate;
   - collapsed `WaitingReason` coverage.
9. End-to-end test: `start_plan → commit plan-only → review APPROVE
   → commit code → review REQUEST_CHANGES → commit fix → review
   APPROVE → done-move → committed`. Assert `waiting_on` transitions
   match the new readiness rule at each step. Add a cumulative-
   participant variant: a second reviewer joins at the second
   `code_only` commit and is then expected on subsequent commits.
10. Optional `plan_id` inference covered by tests:
    - `wait_for_work` with no `plan_id` and exactly one active plan
      in the resolved repo → resolves to that plan.
    - `wait_for_work` with no `plan_id` and zero active plans →
      returns `{timed_out: true, no_active_plans: true}`.
    - `wait_for_work` with no `plan_id` and multiple active plans
      → returns `ambiguous_plan` error with a candidate list whose
      entries carry `plan_id`, `current_path`, `state`, `waiting_on`.
    - Explicit `plan_id` always overrides inference (test passes a
      `plan_id` while multiple actives exist; the named one resolves
      without an ambiguity error).
11. `caller_already_voted` semantics test: re-wake is suppressed
    for `APPROVE` and `REQUEST_CHANGES` on the latest relevant SHA,
    but NOT for `Unmarked`/ambiguous (the caller is woken so they
    can fix the verdict marker).

## Decisions (resolved open questions)

- **Started but unfinished plan reviews.** A reviewer who wants to
  push back on "the plan itself isn't approved yet" can either
  `REQUEST_CHANGES` on the code commit (with prose explaining the
  reason) or retroactively vote on the earlier `plan_only` commit.
  Both are valid; the gate is per-commit so backfill votes recompute
  naturally.
- **`multi_plan` commits.** Classified for timeline rendering but
  never drive `waiting_on` or generate `wait_for_work` items. They
  appear as a special row in each affected plan's timeline ("touched
  multiple plans — review the diff if relevant"). No per-plan review
  files are auto-generated. If an operator wants a multi-plan review,
  they manually drop feedback at `commits/<sha>/<author>.md` under
  one or more plans.
- **Ordering enforcement.** Looser by default. The new model surfaces
  "code commit before plan approval" via prompt-hint prose but does
  not block it. Tighter enforcement can be added later as a per-plan
  policy flag, not a storage axis.
- **`done_move` review.** Not required. The act of moving the plan
  file into `done/` is master-only post-approval bookkeeping; once
  the latest non-`done_move` relevant commit is approved, the
  `done_move` commit short-circuits to `SessionDone`.
