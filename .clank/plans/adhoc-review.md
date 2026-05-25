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

## Design: `derive_work` with a feedback trait

### The trait

```rust
pub trait ReviewLookup {
    fn reviews_for(&self, target: ReviewTarget) -> Vec<ReviewEntry>;
}

pub enum ReviewTarget {
    Plan { plan: PlanKey, sha: CommitSha },
    AdHoc { sha: CommitSha },
}

pub struct ReviewEntry {
    pub author: AgentLabel,
    pub verdict: Verdict,
}
```

Lives in `crates/core`. The CLI implements it by scanning
feedback files from disk. Core never does I/O.

### New `derive_work` signature

```rust
pub fn derive_work(
    state: &RepoState,
    reviews: &impl ReviewLookup,
    author: &AgentLabel,
    role: Role,
    config: &ReviewPolicy,
) -> Vec<WaitItem>
```

`ReviewPolicy` is a small core struct:

```rust
pub struct ReviewPolicy {
    pub force_review_on_misc_commits: bool,
    pub force_review_on_plan_commits: bool,
    pub ad_hoc_reviewers: Option<Vec<AgentLabel>>,
}
```

`derive_work` iterates `state.plans` (computing gate state
via `reviews.reviews_for(Plan { .. })`) and `state.ad_hoc`
(via `reviews.reviews_for(AdHoc { .. })`), emitting work items
for both. The `PlanView` projection step is absorbed into
`derive_work` — it computes `waiting_on` internally from the
fold timeline + reviews.

### WaitItem shape

Add `AdHocReview` and `AdHocRevise` to `WaitItem`:

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

- New `ReviewLookup` trait + `ReviewTarget` + `ReviewEntry` +
  `ReviewPolicy` types.
- `derive_work` rewritten to take `&RepoState` + `&impl
  ReviewLookup` instead of `&[PlanView]`. Computes gate state
  internally. Handles both plans and ad-hoc commits.
- `PlanView` projection may be simplified or kept for `status`
  display (it still needs worktree facts which come from I/O).

### `crates/cli`

- Implement `ReviewLookup` over the filesystem (scan feedback
  files, return entries). Replaces the current `scan_feedback`
  → `PlanView` projection → `derive_work` pipeline with a
  single `derive_work(&state, &fs_reviews, ...)` call.
- `wfw.rs`: load config, build `ReviewPolicy`, construct the
  filesystem `ReviewLookup`, call `derive_work`.
- `feedback.rs`: add `--adhoc` flag.
- `stop_hook.rs`: render `AdHocReview` / `AdHocRevise`.
- Lifecycle hooks: ad-hoc items skip `firings_from_items` (no
  `PlanKey`).

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
