# commit-first-review-model

The reviewable unit is the COMMIT. A plan is metadata attached to
commits — it links related commits, gives reviewers context, and
lets a working agent stay inside a plan until it's finished. But
plan membership should not define whether a commit can be
reviewed, and `wait_for_work` should not require a plan id.

This plan is the precursor to `wfw-local`. It changes the Trinity
model so:

- `trinity wfw --role reviewers --author codex` waits **at repo
  scope** for the next reviewable commit codex has not reviewed,
  whether that commit belongs to plan A, plan B, or no plan.
- `trinity wfw <plan> --role reviewers --author codex` is an
  optional filter that narrows to commits associated with that
  plan.
- `trinity wfw --role master --author claude` waits for the next
  master action across the repo: address-changes on any plan, a
  pending ad hoc review, a stuck state — whichever fires first.
- **Ad hoc commits** (commits touching no plan file) are
  reviewable as a first-class category. Today's `Unattributed`
  classification silently drops them from review entirely; the
  new model gives them a gate and a feedback path.

## Why

Codex's architectural critique against `wfw-local` (file
`.trinity/feedback/wfw-local/9f5a052d.../codex.md`): "The
reviewable unit is a commit. Plan membership is additional
context attached to a commit." Today's Trinity disagrees: every
`CommitGate` lives on a `PlanTimelineEvent` of one specific
`Plan`, so every `wait_for_work` call requires a `plan_id`. Out-
of-plan work is invisible to the review system.

This model leaks in concrete ways:

- An agent who lands a one-off code change cannot ask for review
  through Trinity. Useful out-of-plan work is unreviewable.
- The `MultiPlan` / `Unattributed` paths exist precisely because
  commits resist being squeezed into one plan; the fold has
  special cases for them but no review path.
- `trinity wfw` requires the operator to know which plan they
  care about — but the natural ask in a loop is "what's next for
  me to review in this repo," not "what's next in plan X."

The commit-first model collapses this: every reviewable commit
gets a gate, plans are pointer-collections over commits, and
`wait_for_work` walks repo-wide.

## Target model

### Data shape (conceptual)

```
RepoState {
  // The chronological commit stream, indexed by SHA. Every reviewable
  // commit lives here with its gate. Plans are *views* over this map.
  commits: BTreeMap<CommitSha, CommitNode>,

  // Plan metadata. Each plan owns its file path + body hash + frozen
  // state, but the per-commit gates live in `commits`. The plan stores
  // a chronological pointer list back into the commit map.
  plans: BTreeMap<PlanKey, Plan>,
  ...
}

CommitNode {
  sha,
  parents,
  kind: CommitKind,
  attribution: CommitAttribution,   // plan attribution category
  plans: BTreeSet<PlanKey>,         // plans this commit is attributed to
                                    // (often 0 or 1; >1 for MultiPlan)
  gate: Option<CommitGate>,         // None for non-reviewable
}

enum CommitAttribution {
  Plan(PlanKey),                    // attributed to exactly one plan
  MultiPlan(BTreeSet<PlanKey>),     // touches >1 plan; non-reviewable
  AdHoc,                            // touches no plan (NEW: reviewable)
  Finalize(PlanKey),                // freeze commit; non-reviewable
  ...
}

Plan {
  id, plan_path, body_hash, frozen state, ...
  // Pointers into RepoState.commits, chronological. Empty for plans
  // that have only a plan_intro on disk.
  commits: Vec<CommitSha>,
}
```

The wire `WorkPayload` already returns `plan_id` as a singular
string. Post-pivot, the field becomes `plans: Vec<String>`
(possibly empty for ad hoc work). The `target_sha` field is
unchanged — it was already the canonical identity.

### Matcher contract

`wait_for_work(args)` accepts an OPTIONAL `plan_id` filter.

- `plan_id` present → behavior unchanged: walk that plan's
  commits, return the first that needs the caller's role.
- `plan_id` absent → walk all visible commits in chronological
  order, return the first reviewable commit gate that needs the
  caller's role and that the caller has not voted on.

Both branches share the same per-commit gate logic — only the
candidate stream differs.

## Ad hoc commit reviewability

Ad hoc commits become reviewable. Two design questions need
explicit answers in this plan:

### Q1. Who reviews an ad hoc commit?

The default participant set has nowhere to inherit from (no plan
history). The contract:

- **Default participant set is "every label that has authored
  feedback in this repo on the current branch's history."**
  Cheap to compute from the fold and matches the implicit
  "active reviewer set" the operator is already working with.
- The set can be empty in a fresh repo. Empty participant set
  → gate is `NoParticipants` and the commit is non-blocking;
  master can continue without review.
- A repo-level config (below) can pin an explicit reviewer
  list, overriding the auto-derived set.

### Q2. Where does feedback live on disk?

Today: `.trinity/feedback/<plan-key>/<sha>/<author>.md`.

Ad hoc commits have no plan key. The new path uses a reserved
key segment:

```
.trinity/feedback/_/<sha>/<author>.md
```

`_` is reserved as the "no plan" key. `PlanKey::parse("_")`
rejects it; `disk_format` parses `_` into a dedicated
`FeedbackTarget::AdHoc { sha, author }` variant alongside the
existing `FeedbackTarget::Plan { plan_key, sha, author }`.

## Config

Two layers, deep-merged:

- `~/.trinity/config.json` — user-level defaults.
- `<repo>/.trinity/config.json` — repo-level overrides
  (committed; visible to all collaborators).

Schema (v1):

```json
{
  "review": {
    "force_review_on_misc_commits": true,
    "force_review_on_plan_commits": true,
    "ad_hoc_reviewers": null
  }
}
```

- `force_review_on_misc_commits` (bool, default `true`) —
  whether master is blocked on ad hoc commit review.
- `force_review_on_plan_commits` (bool, default `true`) —
  whether master is blocked on plan-commit review (today's
  implicit behavior). Setting `false` lets a working agent
  continue without waiting for reviewers; reviewer wakes still
  fire so reviews CAN happen, just not blocking.
- `ad_hoc_reviewers` (`null` or `["alice", "bob"]`) — explicit
  reviewer set for ad hoc commits. `null` → derive from the
  branch's feedback authors (Q1).

`force_review_on_misc_commits=false` short-circuits ad hoc
gates to `NoParticipants` regardless of config, so the
working agent never blocks on them. Reviewers can still
write feedback — it just doesn't gate.

## Implementation outline

This is a real model refactor, not a CLI tweak. The plan
intentionally separates "model" from "consumers" so the
refactor can land in one chunk and the wire changes follow.

### Phase 1 — RepoState carries `commits` map

- Add `RepoState::commits: BTreeMap<CommitSha, CommitNode>`.
- Add `CommitNode` + `CommitAttribution` types.
- The fold (`disk_snapshot::derive_base_state`) populates
  `commits` alongside the existing per-plan timelines.
- `Plan` keeps its current `timeline` field — Phase 1 is
  ADDITIVE so the daemon's existing matcher keeps working
  unchanged. The new `commits` map shadows it.
- Attribution classifies commits as `Plan(_)`, `MultiPlan(_)`,
  `AdHoc`, or `Finalize(_)`. `Unattributed` is replaced by
  `AdHoc` (semantic rename: it was always "no plan" — now
  that category is first-class).

### Phase 2 — gates on commits, not on timeline events

- `CommitNode.gate: Option<CommitGate>` is the authoritative
  gate.
- `PlanTimelineEvent`'s gate field becomes a `CommitSha`
  pointer; `Plan::event_for(sha)` returns the SHA + a borrow
  into `RepoState.commits[sha].gate`.
- `disk_snapshot::rebuild_plan_gates` is rewritten to walk
  the repo's commit stream once, computing cumulative
  participants and gate state per commit. Plan timelines
  become read-only views over those commits.

### Phase 3 — repo-scoped matcher

- `wait::WaitArgs.plan_id` becomes `Option<String>`.
- New `compute_match_repo_scope` walks `RepoState.commits`
  in chronological order, returns the first commit that
  needs the caller's role + the caller hasn't voted on.
- `compute_match` (the existing per-plan path) becomes a
  thin filter: walk only commits whose `CommitNode.plans`
  contains the requested key.
- Wire surface (`WorkPayload`):
  - Add `plans: Vec<String>` field (empty for ad hoc work).
  - `plan_id` field stays for back-compat during the
    transition; populated as `plans[0]` when present, `None`
    otherwise (still serialized as `Option<String>`).
  - Once `wfw-local` lands, the singular `plan_id` field on
    the wire is dropped.

### Phase 4 — ad hoc reviewability + config

- `disk_format::parse_feedback_path` learns `_` as the
  reserved ad-hoc key.
- `notify_bridge` / `fs_watcher` route `.trinity/feedback/_/...`
  to ad hoc updates.
- Default participant set for ad hoc gates is derived from
  the branch's feedback authors (excluding the commit's
  author).
- Config file loader at `cli::config` reads the two-layer
  JSON, applies to gate construction.
- `force_review_on_misc_commits=false` → ad hoc gates emit
  `NoParticipants` so master doesn't block.
- `force_review_on_plan_commits=false` → plan gates emit
  `NoParticipants`-equivalent for master matching (reviewer
  wakes still fire; master just doesn't wait).

### Phase 5 — `start_plan` lifts the start-of-plan restriction

- Today `start_plan` requires that the cwd-repo has no other
  active plan in the same basename, etc. With ad hoc commits
  in play, this stays unchanged — `start_plan` still creates
  one plan file per call.
- Verify nothing in `start_plan` assumes "only plans
  produce commits."

## Tests

- Unit: `attribution::classify` returns `AdHoc` for a
  no-plan-touching commit; participant derivation pulls from
  branch feedback history.
- Unit: matcher repo-scope returns the chronologically
  earliest reviewable commit needing the caller's role.
- Unit: matcher per-plan-scope returns only commits whose
  attribution includes that plan.
- Unit: config layering — repo overrides user; missing keys
  fall through to defaults.
- Integration: write a no-plan commit; `wait_for_work` with
  no `plan_id` wakes for an eligible reviewer; the resulting
  feedback path is `.trinity/feedback/_/<sha>/<author>.md`.
- Integration: `force_review_on_misc_commits=false` →
  master is not blocked by an ad hoc commit with pending
  reviewers; reviewer wakes still fire.
- Regression: every existing per-plan `wait_for_work` test
  continues to pass with `plan_id` supplied.

## Acceptance

- `wait_for_work(plan_id: None, role, author)` returns the
  next reviewable commit needing the caller's role anywhere
  in the repo.
- `wait_for_work(plan_id: Some(p), ...)` returns commits
  filtered to that plan, identical to today's behavior.
- A commit touching no plan files appears in repo-scoped
  reviewer waits, with a feedback path under
  `.trinity/feedback/_/<sha>/<author>.md`.
- Both config knobs are read from
  `~/.trinity/config.json` + `<repo>/.trinity/config.json`
  with documented layering.
- Tests above all green.

## Out of scope

- Lifting the wire-level `plan_id` singular to `plans` list
  on every other endpoint (`list_plans`, `work_context`,
  etc). They keep their per-plan-row shape; only
  `WorkPayload` and `WaitArgs` change.
- Backfilling ad hoc gates for commits that existed before
  this plan landed. The fold creates them on first
  encounter; nothing rewrites history.
- UI/frontend changes. Frontend stays plan-scoped for now;
  it can land ad hoc surface in a follow-up.
- Removing `MultiPlan` non-reviewability. A commit touching
  multiple plans is still ambiguous; reviewers per-plan
  would conflict. MultiPlan commits remain
  reviewability-disabled; a future plan can decide whether
  to introduce explicit cross-plan review.
- Sequencing: this plan lands BEFORE `wfw-local`.
  `wfw-local` becomes the first consumer of the repo-scoped
  matcher.
