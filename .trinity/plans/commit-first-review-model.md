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

## Commit-title convention

Repo-scoped wfw needs a clear signal for which commits belong
to which plan. The model uses a commit-title prefix as a hint;
when the prefix is missing, attribution falls back through
touched plan files, then unambiguous active/last-touched plan
context, and only classifies the commit as `AdHoc` when no
plan context can be inferred. See "Attribution algorithm
(final)" below for the full order.

### Prefix grammar

- `[plan-name] subject` — hint that this commit belongs to
  `plan-name`. Validated against existing plans in the repo.
- `[plan-one,plan-two] subject` — explicit multi-plan list
  for the (rare) commit that intentionally spans plans.
- `[misc] subject` — explicit out-of-plan marker. Classifies
  the commit as `AdHoc` even if it incidentally touches a
  plan file (e.g. a docs typo fix).
- `[<unknown>] subject` — name doesn't match any plan.
  Classified as `AdHoc` by default, with a warning surfaced
  on the next master wake ("commit `abc1234` titled
  `[foo] …`; `foo` is not a known plan; treated as ad
  hoc — amend or add `[misc]` to silence"). NOT a hard
  model error.
- No prefix → fall back through the ordered no-prefix
  chain documented in "Attribution algorithm (final)":
  touched plan files first, then unambiguous
  active/last-touched plan context, then `AdHoc` only when
  no plan context can be inferred. Warn on ambiguity
  (file-side and any prefix disagree).

### Strict mode

Config knob `review.require_commit_prefix` (bool, default
`false`):

- `false` (default) — convention is advisory. Missing or
  unrecognized prefixes warn but never block.
- `true` — convention is mandatory. A commit landing
  without a valid `[plan-name]` / `[misc]` / `[plans-list]`
  prefix gets a synthetic master work item:
  `WorkAction::FixCommitTitle { sha, suggested_prefix }`.
  The master is expected to `git commit --amend` (or
  rebase) to add the prefix; reviewers do not wake on the
  commit until the prefix lands. This is the only path
  where strict mode blocks the workflow; the model never
  rewrites history on the caller's behalf.

### Attribution algorithm (final)

The order preserves today's "implementation commits inherit
the active plan" behavior; the prefix is a stronger signal
when present.

For each commit during the fold:

1. **Explicit prefix wins.** Parse the title for `[…]`.
   - `[misc]` → `AdHoc`. Overrides incidental plan-file
     touches (e.g. a typo fix on a plan file titled `[misc]`
     stays AdHoc).
   - `[name]` / `[name1,name2,…]` with every name matching
     a known plan → `Plan(_)` / `MultiPlan(_)`.
   - `[name…]` with at least one unknown name → `AdHoc` +
     warning attached to the gate. NOT a hard error.
2. **No prefix, touches plan files** → `Plan(_)` /
   `MultiPlan(_)` from the touched files. Non-strict mode
   warns ("commit `abc1234` touches plan-a but no `[plan-a]`
   prefix; consider amending the title").
3. **No prefix, touches no plan file, unambiguous
   active/last-touched plan context** → attributed to that
   plan. This is today's behavior; impl commits in the
   middle of a plan keep inheriting the plan they belong
   to. "Unambiguous" = exactly one plan is currently active
   (visible, not frozen) AND/OR the most recently touched
   plan in the chain is clearly the working context. The
   exact predicate is the one the existing attribution
   module uses today; the rename to `CommitAttribution`
   doesn't change it.
4. **No prefix, no plan-file touch, ambiguous or missing
   context** → `AdHoc`. The commit is genuinely not tied
   to any plan and the fold has no way to infer one.

### Strict mode (`require_commit_prefix=true`)

Strict mode does NOT erase the fallback logic — it shifts
the consequence. The classifier still runs (1–4) to compute
the inferred attribution and an explanation, then:

- Step 2 fallback (touched plan files, no matching prefix) →
  `FixCommitTitle { sha, suggested_prefix:
  "[<touched-plans>]" }`.
- Step 3 fallback (active/last-touched plan inferred) →
  `FixCommitTitle { sha, suggested_prefix: "[<that-plan>]" }`.
- Step 4 (genuine ad hoc, unambiguous) → `FixCommitTitle`
  with `suggested_prefix: "[misc]"`.
- Ambiguous case (multiple plausible plans, no prefix) →
  master work explaining the ambiguity and asking for
  `[plan]`, `[plan-one,plan-two]`, or `[misc]`. No
  suggested_prefix; the model can't pick.

Reviewers do not wake on any commit while a
`FixCommitTitle` is outstanding for it.

The warning + the `FixCommitTitle` synthetic work item are
the only NEW outputs this section adds; everything else is
classification logic on inputs the fold already has.

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
    "ad_hoc_reviewers": null,
    "require_commit_prefix": false
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
- `require_commit_prefix` (bool, default `false`) — when
  `true`, commits without a valid `[plan]` / `[misc]` /
  `[plans-list]` title prefix synthesize a master
  `FixCommitTitle` work item instead of being classified.
  Reviewers do not wake on those commits until the master
  amends the title. See "Commit-title convention" above.

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

### Phase 5 — commit-title convention + strict mode

- `attribution::classify` learns the title-prefix grammar.
- Unknown plan names in a prefix → `AdHoc` + warning attached
  to the gate. Surfaced on the next master wake; non-blocking.
- New synthetic `WorkAction::FixCommitTitle { sha,
  suggested_prefix }`. Emitted only under
  `require_commit_prefix=true` for commits failing the
  classification check. The matcher emits it ahead of any
  per-plan review work so the master fixes the title first.
- Tests: prefix parsing (single / multi / misc / unknown),
  attribution priority (prefix > file inference), strict mode
  emits `FixCommitTitle`, warnings ride along on non-strict
  wakes.

### Phase 6 — `start_plan` lifts the start-of-plan restriction

- Today `start_plan` requires that the cwd-repo has no other
  active plan in the same basename, etc. With ad hoc commits
  in play, this stays unchanged — `start_plan` still creates
  one plan file per call.
- Verify nothing in `start_plan` assumes "only plans
  produce commits."

## Tests

- Unit: `attribution::classify` returns `AdHoc` for a
  no-plan-touching commit ONLY when there is no
  unambiguous active/last-touched plan context to fall back
  on (the genuine ad-hoc case); participant derivation
  pulls from branch feedback history.
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
- Unit: title-prefix parsing for `[plan]`, `[plan-one,
  plan-two]`, `[misc]`, `[<unknown>]`, no prefix. Each maps
  to the documented attribution outcome.
- Unit: prefix > file inference. A commit prefixed `[misc]`
  that incidentally touches a plan file classifies as
  `AdHoc`; a commit prefixed `[plan-a]` that touches only
  unrelated files classifies as `Plan(plan-a)`.
- Unit: unprefixed impl commit (no plan-file touch) with
  exactly one active/last-touched plan → `Plan(that-plan)`.
  This guards today's behavior.
- Unit: unprefixed commit, no plan-file touch, no plan
  context (fresh repo or all plans frozen) → `AdHoc`.
- Unit: unprefixed commit with ambiguous plan context →
  non-strict mode warns; strict mode surfaces master work
  asking for an explicit `[plan]` / `[plan-list]` /
  `[misc]` (no `suggested_prefix` in the ambiguous case).
- Integration: an unknown-plan prefix produces an
  `AdHoc`-classified gate AND a warning that rides along on
  the next master wake. NOT a hard fold error.
- Integration: `require_commit_prefix=true` →
  - unprefixed commit with inferred active plan →
    `FixCommitTitle { suggested_prefix: "[that-plan]" }`;
  - unprefixed commit with no plan context →
    `FixCommitTitle { suggested_prefix: "[misc]" }`;
  - reviewers do NOT wake on the commit until the title
    is amended.
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
- All config knobs (`force_review_on_misc_commits`,
  `force_review_on_plan_commits`, `ad_hoc_reviewers`,
  `require_commit_prefix`) are read from
  `~/.trinity/config.json` + `<repo>/.trinity/config.json`
  with documented layering.
- The title-prefix convention is honored: prefixes drive
  attribution; unknown plans degrade to `AdHoc` with a
  warning; strict mode emits `FixCommitTitle` master work.
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
