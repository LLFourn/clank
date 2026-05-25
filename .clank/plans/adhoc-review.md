# adhoc-review

## Summary

Wire the existing `review.force_review_on_misc_commits` config
into `derive_work` so ad-hoc commits actually surface as wfw
work items. The config and two-layer loading already exist in
`crates/cli/src/cli/config.rs` — it just isn't connected to
the wait surface yet.

## What already exists

`config.rs` has:
- `ReviewConfig.force_review_on_misc_commits: bool` (default
  `true`)
- `ReviewConfig.ad_hoc_reviewers: Option<Vec<AgentLabel>>`
- Two-layer loader: `~/.clank/config.json` → `.clank/config.json`
- Tests for defaults, overrides, malformed JSON

`RepoState.ad_hoc: Vec<AdHocEvent>` tracks ad-hoc commits in
the fold. `LogEvent::AdHoc` was just added for `clank log`.

## What's missing

1. **`derive_work` takes `&[PlanView]`** — it should take
   `RepoState` + a trait for feedback lookup, so it can derive
   work for both plans and ad-hoc commits in one pass.

2. **No ad-hoc feedback path wired** — `FeedbackTarget::AdHoc`
   and the `_` directory already exist in `disk_format.rs` but
   nothing scans or writes ad-hoc feedback yet.

3. **`clank feedback write` can't target ad-hoc commits**.

## Design: `derive_status` + `ReviewLookup` trait

### The trait

```rust
pub trait ReviewLookup {
    fn reviews_for(&self, sha: &CommitSha) -> Vec<ReviewEntry>;
    fn worktree_status(&self, plan: &PlanKey) -> PlanWorktreeStatus;
}

pub struct ReviewEntry {
    pub author: AgentLabel,
    pub verdict: Verdict,
}
```

Lives in `crates/core`. The CLI implements it by scanning
feedback files from disk and comparing worktree to HEAD. Core
never does I/O.

### `RepoState::derive_status`

```rust
impl RepoState {
    pub fn derive_status(
        &self,
        reviews: &impl ReviewLookup,
        policy: &ReviewPolicy,
    ) -> WorkStatus { ... }
}
```

Computes the objective work state for every plan + ad-hoc
commit in one pass. No per-agent filtering — that comes after.

### `ReviewPolicy`

```rust
pub struct ReviewPolicy {
    pub force_review_on_misc_commits: bool,
    pub ad_hoc_reviewers: Option<Vec<AgentLabel>>,
}
```

### `WorkStatus`

```rust
pub struct WorkStatus {
    pub plans: Vec<PlanWorkState>,
    pub ad_hoc: Vec<AdHocWorkState>,
}

pub struct PlanWorkState {
    pub plan: PlanKey,
    pub sha: CommitSha,
    pub gate: CommitGateState,
    pub waiting_on: WaitingOn,
}

pub struct AdHocWorkState {
    pub sha: CommitSha,
    pub gate: CommitGateState,
}

impl WorkStatus {
    pub fn work_for(
        &self,
        author: &AgentLabel,
        role: Role,
    ) -> Vec<WaitItem> {
        // cheap filter over the precomputed state
    }
}
```

`derive_status` computes gate states:
- For each active plan: get reviewable SHAs from timeline,
  call `reviews.reviews_for(sha)` on the latest reviewable.
  Gate: any approve → approved, any request_changes →
  changes_requested, else unreviewed. Call
  `reviews.worktree_status(plan)` for commit routing.
- For each ad-hoc commit (when `policy.force_review_on_misc_commits`):
  call `reviews.reviews_for(sha)`. Same gate rule: any approve
  → approved.

Gate rule is intentionally simple: a single approve is enough
to unblock. Cumulative participant tracking is deferred.

`work_for` filters the objective state by role + author to
produce actionable `WaitItem`s. This is the same data `clank
status` can also render — one computation serves both commands.

### WaitItem shape

Add to `WaitItem`:

```rust
AdHocReview {
    sha: CommitSha,
    feedback_path: String,
},
AdHocRevise {
    sha: CommitSha,
},
```

### Feedback target

Use the existing `FeedbackTarget::AdHoc` from `disk_format.rs`
(the `_` segment). Path:
`.clank/agents/<author>/feedback/_/<sha>.md`.

### `clank feedback write` for ad-hoc

Add `--adhoc` flag (mutually exclusive with `--plan`). When set,
the commit ref resolves against `state.ad_hoc` SHAs. Writes to
the `_` feedback directory.

## Implementation surface

### `crates/core`

- New `ReviewLookup` trait, `ReviewEntry`, `ReviewPolicy`,
  `WorkStatus`, `PlanWorkState`, `AdHocWorkState`.
- `RepoState::derive_status` method.
- `WorkStatus::work_for` method.
- `PlanView` projection kept for `clank status` display but
  no longer used by work derivation. Can be simplified or
  migrated to use `WorkStatus` over time.

### `crates/cli`

- `FsReviewLookup` implements `ReviewLookup` over the
  filesystem (scan feedback files, check worktree).
- `wfw.rs`: load config → build `ReviewPolicy` → construct
  `FsReviewLookup` → call `state.derive_status(reviews, policy)`
  → call `status.work_for(author, role)`.
- `feedback.rs`: add `--adhoc` flag.
- `stop_hook.rs`: render `AdHocReview` / `AdHocRevise`.
- Lifecycle hooks: ad-hoc items skip `firings_from_items`.

## Tests

- `force_review_on_misc_commits=true`: unreviewed ad-hoc →
  reviewer wfw returns work.
- Reviewer approves → no more work.
- Reviewer request_changes → master wfw returns revise.
- `force_review_on_misc_commits=false`: no ad-hoc work items.
- `ad_hoc_reviewers=["codex"]`: only codex gets ad-hoc work.
- `clank feedback write --adhoc --commit <sha>` writes to
  `_` path.
- Ad-hoc items don't fire lifecycle hooks.
- Existing plan-based derive_work tests still pass (the trait
  impl returns the same data the old PlanView projection did).

## Acceptance criteria

- `derive_work` takes `RepoState` + `ReviewLookup` trait.
- Ad-hoc commits surface as wfw work items when config enables.
- Reviewer and master flows work for ad-hoc commits.
- `clank feedback write --adhoc` works.
- Default `force_review_on_misc_commits` is true.

### `crates/cli/src/cli/feedback.rs`

- Add `--adhoc` flag to `FeedbackWriteArgs`.
- When set, resolve commit against `state.ad_hoc` SHAs.
- Write to `_` feedback directory.

### `crates/cli/src/feedback_scan.rs`

- Extend to scan `_` directory for ad-hoc feedback.

## Tests

- `force_review_on_misc_commits=true` (default): unreviewed
  ad-hoc commit → reviewer wfw returns work.
- Reviewer approves → no more work.
- Reviewer request_changes → master wfw returns revise.
- `force_review_on_misc_commits=false`: no ad-hoc work items.
- `clank feedback write --adhoc --commit <sha>` writes to
  `_` path.
- `ad_hoc_reviewers=["codex"]`: only codex gets ad-hoc review
  work; alice (not in list) gets none.

## Acceptance criteria

- Ad-hoc commits surface as wfw work items when
  `force_review_on_misc_commits` is true.
- Reviewer and master flows work for ad-hoc commits.
- Feedback lives at `_/<sha>.md`.
- `clank feedback write --adhoc` works.
- Default is true (existing config default). Set to true for
  this repo (already the default — no config change needed).
