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
`plan_gate` / `impl_gate` derivations, and the two parallel sets of
`waiting_on` reasons that mirror each other.

## Problem

Trinity today encodes the plan/impl distinction in five places:

1. **Storage.** `Plan` carries `plan_feedback: BTreeMap<…>` and
   `impl_feedback: BTreeMap<…>` as separate maps, keyed by `(CommitSha,
   AgentLabel)` but indexed by which subdirectory the file lived in.
2. **Disk layout.** `.trinity/feedback/<slug>/{plan,impl}/<sha>/<author>.md`
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

Touch one piece of plan/impl semantics and you usually touch all five.
A grep for the dichotomy returns 134 hits across `repo_state.rs`,
`projection.rs`, `runtime.rs`, `ui_response.rs`, `mcp_response.rs`,
`server/wait.rs`, the frontend store, and tests. Every new feature
asks "which side is this?" — for no reason that the storage model
captures intrinsically.

The dichotomy is a label, not a structural fact. A commit either
touches the plan file, touches code, or both. That's what `git_io`
already records via `PlanTouchKind` and `has_code_changes`. We're
discarding that information at storage time, then reconstructing a
weaker version of it ("plan phase" vs "impl phase") on every read.

## Target Model

A plan is a record. The plan file at `.trinity/plans/<slug>.md` and
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
.trinity/feedback/<slug>/commits/<sha>/<author>.md
```

The first line is still `APPROVE` or `REQUEST_CHANGES`. The target
SHA tells you what was reviewed; the parser doesn't need a
plan/impl hint.

Per-commit review gate:

- `changes_requested` — any current feedback for this SHA opens with
  `REQUEST_CHANGES`, OR any current feedback is ambiguous/unmarked,
  OR any plan-wide participant has not responded to this SHA yet.
- `approved` — every plan-wide participant has responded to this SHA,
  zero responses are `REQUEST_CHANGES` or ambiguous/unmarked, and at
  least one reviewer has responded with `APPROVE`.
- `unreviewed` — no reviewer has responded to this SHA yet.

Plan-wide participants are cumulative: any agent who has left feedback
on any earlier or current commit in this plan's history is part of the
participant set for later commit gates. That is the important behavior
change from today's phase-split model: a reviewer who participated in
plan review is still expected to respond when the next code commit
lands. "Plan reviewer" and "implementation reviewer" are display
labels only; participation is attached to the plan's commit stream.

Session-level "what next" is a fold over commits walked newest-first:

- Latest commit is `unreviewed` → reviewers' turn.
- Latest commit is `changes_requested` → master's turn (address).
- Latest commit is `approved` and `code_only`/`mixed` → master's turn
  (continue, or finish by `done_move`).
- Latest commit is `approved` and `plan_only` → master's turn (start
  implementation OR move to done if the plan is now done-shaped).
- Plan file under `done/` and the move commit is approved → terminal.

That's the entire decision matrix. No "phase" enum. No `Plan.phase`.
The UI can still display chips ("Plan", "Code", "Mixed") computed
from `commit_kind`; agents still get prompt hints scoped to the
commit kind. None of it is in the storage model.

## Goals

1. One feedback path per commit; no `plan/` vs `impl/` directories.
2. One `Plan.commits: BTreeMap<CommitSha, CommitGate>` map; no
   `plan_feedback` / `impl_feedback` split.
3. One `WaitingReason` set keyed on `(commit_kind, gate_state)`
   instead of two mirrored sets.
4. MCP `wait_for_work` returns `{plan_id, target_sha, commit_kind,
   work, prompt_hint, locations}` for every review and address-action
   case.
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
  cross-plan reviews.
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

Stored on `AttributionResult::Attributed { kind: CommitKind, … }`
(the existing `plan_touch` and `has_code_changes` fields collapse
into `kind` since the kind subsumes them).

### `CommitGate`

Per-commit folded review state:

```rust
pub struct CommitGate {
    pub state: CommitGateState,           // Unreviewed/Approved/ChangesRequested
    pub participants: Vec<AgentLabel>,
    pub approvers: Vec<AgentLabel>,
    pub requesters: Vec<AgentLabel>,
    pub ambiguous: Vec<AgentLabel>,
    pub missing: Vec<AgentLabel>,
    pub feedback: BTreeMap<AgentLabel, Feedback>,
}
```

`Plan.commits: BTreeMap<CommitSha, CommitGate>` replaces both
`plan_feedback` and `impl_feedback`. Iteration order is SHA-lex; the
existing `commit_order: Vec<CommitSha>` field on `RepoState` keeps
chronological order.

`CommitGate` is derived with a cumulative participant fold over
`commit_order`: before evaluating a target commit, collect every
agent who has left feedback on any earlier commit for the plan, then
include any agents who responded on the target commit itself. The
target commit is ready only when that full set has non-ambiguous,
non-request-changes feedback on the target.

### Feedback path parsing

`.trinity/feedback/<slug>/commits/<sha>/<author>.md`. The
`parse_feedback_path` function loses its `FeedbackPhase` branch and
returns `FeedbackPath { plan_key, target_sha, author }` (no phase).
Held feedback (today's `held_plan_feedback`) goes away as a separate
concept — a feedback file written before the target commit exists is
just an unmatched SHA; we keep it under `commits/<sha>/` and the
gate becomes `Unreviewed` once the commit lands. No queue, no
re-write.

Actually: with SHA-targeted writes there's nothing to "hold" — the
author either knows the SHA they're reviewing (and writes the file)
or doesn't (and shouldn't be writing feedback). Today's held-feedback
mechanism exists because reviewers could drop a plan review before
the plan was committed; in the commit-centric model the plan
necessarily already has a SHA before any review can target it.

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
`commit_order` newest-first for the first commit whose kind is
relevant to this plan and return based on its `CommitGate`.

### `WaitingReason` collapse

Twelve → six:

| Old (plan/impl pair)                                                    | New                          |
|--------------------------------------------------------------------------|------------------------------|
| `PlanNeedsInitialReview` / `ImplNeedsInitialReview`                      | `CommitNeedsReview`          |
| `PlanNeedsRereview` / `ImplNeedsRereview`                                | (same — re-review is the same gate state with prior participants) |
| `AddressPlanRequestChanges` / `AddressImplRequestChanges`                | `AddressCommitChanges`       |
| `ReadyToImplement`                                                       | (drop — implied by approved `plan_only` + master's choice to start coding) |
| `ReadyToFinish`                                                          | `ReadyToMoveToDone`          |
| `CommitPlanRevision` / `CommitDoneMove` / `RestoreOrCommitDoneMove`      | (unchanged — worktree-status driven) |
| `SessionDone`                                                            | (unchanged)                  |

`waiting_on.description` continues to disambiguate via the
commit_kind in its prose (e.g. "Plan revision awaiting review from
codex." vs "Implementation commit awaiting review from codex." — same
underlying reason, different rendered text).

### MCP wire shape

`wait_for_work` response gains `target_sha`, `commit_kind`, and
`prompt_hint`; the `locations` field still resolves to writeable
paths but always under `commits/<sha>/`:

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
one drives `waiting_on`. The old `plan_feedback` / `impl_feedback`
arrays are gone.

### HTTP / SPA

- `/api/plan/{repo}/{stem_md}` returns the new shape (one
  `commits[]`, no `plan_feedback`/`impl_feedback`).
- Revision route and commit-diff route are unchanged structurally
  — they're already commit-SHA-addressed.
- Frontend timeline already renders commits + reviews in order;
  the change is just consuming the unified shape and rendering
  chips by `commit_kind`.

### Readiness rule edge cases

The stub asks "should `plan_only` approvals be interpreted as
'ready to implement' automatically?" The answer in this plan: no.
After a `plan_only` commit is approved, the master is `ReadyToFinish
| StartImplementation`-style state; making the first `code_only` /
`mixed` commit is their next action. There's no explicit "start
implementing" event — the act of committing code IS the event.

What about code commits that land before any plan commit is approved?
Today that's blocked by the impl gate being unable to fire until
plan_intro exists with attribution. In the new model:

- If a `code_only` commit lands while the only previous commit was
  an unreviewed `plan_only`, the latest relevant commit is the
  `code_only` — gate `Unreviewed`. Reviewers see it as work; the
  prompt_hint says "this commit is implementation work on an
  unreviewed plan" so reviewers can choose to defer or push back.
- This is looser than today's "plan must be approved first" implicit
  gate. The trade-off is honest: Trinity stops enforcing workflow and
  surfaces it instead. Tighter enforcement can be added later as a
  separate `wait_for_work` policy flag, not a storage axis.

### Worktree-status preemption

`PlanWorktreeStatus::{Clean, BodyDirty, DoneMovePending,
MissingActivePlanFile}` survives unchanged. It always preempts
gate-driven reasons (today and in the new model). The worktree
states are "the operator has uncommitted intent" — orthogonal to
review gates.

## Phases

Three phases, each independently shippable. Phase 1 lays the
groundwork without changing the wire; phase 2 cuts the daemon over;
phase 3 retires the dead paths.

### Phase 1 — Internal classification + dual-write

Files: `src/repo_state.rs`, `src/disk_snapshot.rs`, `src/attribution.rs`,
`src/git_io.rs`, `src/projection.rs`, `src/runtime.rs`.

Add `CommitKind` enum and `CommitGate` struct. Populate
`Plan.commits` alongside `plan_feedback` / `impl_feedback` during
rebuild — both views derive from the same disk feedback files. The
new `commits` map is keyed on every commit with attribution to this
plan (whether or not feedback exists).

Read the new commit-keyed feedback path (`commits/<sha>/<author>.md`)
in addition to the legacy `plan/<sha>/` and `impl/<sha>/` paths. The
file watcher learns the new pattern. Feedback writes from
`wait_for_work`'s `locations` still go to the legacy paths (no
behavior change to callers yet).

No wire change in this phase. Frontend untouched. Tests for the new
classifier added but the old tests stay green.

### Phase 2 — Daemon cutover

Files: `src/server/wait.rs`, `src/server/mcp.rs`, `src/server/http.rs`,
`src/tools.rs`, `src/mcp_response.rs`, `src/ui_response.rs`,
`frontend/src/api.rs`, `frontend/src/store.rs`,
`frontend/src/components/*.rs`.

Switch all reads and writes to `commits/<sha>/`. Update tool
descriptions and `wait_for_work` response shape. Update
`get_context` / `plan_detail` JSON. Frontend consumes new shape.

`WaitingReason` enum trimmed per the table above. `expected_action`
mapping updated.

Legacy feedback paths are still parsed at startup so that on-disk
data from before the cut still informs gate state — but new writes
go only to the new path. This is the read-old / write-new bridge,
not backwards compatibility (no callers can write to the old path).

### Phase 3 — Retire phase code

- Drop `plan_feedback` / `impl_feedback` from `Plan`; replace with
  the now-populated `commits` map.
- Drop `Phase` enum and `phase_for`. Anywhere that displayed phase
  (UI chips, MCP response) reads from `commit_kind` of the latest
  relevant commit.
- Drop legacy feedback-path parsing (the `FeedbackPhase` enum, the
  `plan` / `impl` segments in `parse_feedback_path`).
- Drop `held_plan_feedback` and the normalization sweep that
  rewrites flat-drop reviews.
- Drop the six mirror `WaitingReason` variants and `plan_gate_for` /
  `impl_gate_for`.
- Sweep tests; rewrite anything still asserting on `plan_feedback` /
  `impl_feedback` to use `commits[sha].feedback`.

## Acceptance Criteria

1. `grep -rn "plan_feedback\|impl_feedback" src/ frontend/` is empty
   outside `tests/` (and ideally there too).
2. `find .trinity -path '*/plan/*' -o -path '*/impl/*'` finds no
   live-traffic feedback files; only the `commits/<sha>/` layout is
   used.
3. `Phase` enum removed; `phase_for` removed.
4. `WaitingReason` enum has six variants (down from twelve).
5. `wait_for_work` returns `commit_kind` + `prompt_hint` on every
   work response.
6. `get_context` returns `commits[]` (with per-commit gate +
   feedback) instead of split `plan_feedback` / `impl_feedback`.
7. Frontend timeline renders the same UX as today: commit rows with
   plan/code/mixed/done labels, feedback cards under each commit.
8. Tests cover: `commit_kind` classification of every variant; gate
   roll-up with cumulative plan-wide participants; ambiguous/unmarked
   feedback blocking readiness like `REQUEST_CHANGES`; readiness rule
   walking newest-first; `wait_for_work` prompt-hint shape for each
   kind; collapsed-`WaitingReason` coverage.
9. End-to-end test: start_plan → commit plan-only → review APPROVE
   → commit code → review REQUEST_CHANGES → commit fix → review
   APPROVE → done-move → committed; assert `waiting_on` transitions
   match the new readiness rule at each step.

## Open Questions

- **Started but unfinished plan reviews.** If a user opens
  `wait_for_work` as reviewer, sees a `code_only` commit on an
  unreviewed plan, and wants to push back via "the plan itself isn't
  approved yet" — what's the affordance? A `REQUEST_CHANGES` on the
  code commit with prose? Or a backfill `APPROVE`/`REQUEST_CHANGES`
  on the earlier `plan_only` commit, expecting the daemon to
  recompute? I lean toward the latter — the gate is per-commit, so
  reviews can target any SHA in the plan's history.
- **`multi_plan` commits.** Cross-plan commits exist today (one
  commit touching multiple plan files). The stub says "classify
  them"; the question is whether `wait_for_work` should surface
  them as a single piece of work per plan (one review file per plan)
  or once collectively. Defer: classify them, render in each plan's
  timeline, but don't generate review work for them automatically —
  the operator manually triggers a multi-plan review if they want
  one.
- **Whether `plan_only` and `code_only` interleaving needs ordering
  enforcement.** Today the implicit rule is "plan approved before
  impl begins." The new model surfaces violations as "reviewer was
  asked about an impl commit before the plan was approved" but
  doesn't block them. Is "looser by default, opt-in stricter via a
  per-plan policy flag" the right call? I'd say yes — the
  enforcement was always advisory in practice.
