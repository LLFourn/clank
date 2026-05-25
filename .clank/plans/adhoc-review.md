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

1. **`derive_work` doesn't inspect `ad_hoc`** — it only looks
   at `PlanView`s. Need to add an ad-hoc branch.

2. **No ad-hoc feedback path** — feedback files live under
   `.clank/agents/<author>/feedback/<plan>/<sha>.md`. Ad-hoc
   commits have no plan. Need a feedback target for them.

3. **`clank feedback write` can't target ad-hoc commits** —
   `--plan` parses into `PlanKey` which rejects non-plan values.

## Design decisions

### Feedback target for ad-hoc commits

Use the existing `FeedbackTarget::AdHoc` from `disk_format.rs`,
which renders as the `_` segment. Feedback path:
`.clank/agents/<author>/feedback/_/<sha>.md`.

Already defined, parsed, and tested in `disk_format.rs:36-40`,
`disk_format.rs:154-159`, `disk_format.rs:237-253`.

### WaitItem shape

Add `WaitItem::AdHocReview` and `WaitItem::AdHocRevise` variants
rather than smuggling a sentinel through `PlanKey`:

```rust
AdHocReview {
    sha: CommitSha,
    feedback_path: String,
},
AdHocRevise {
    sha: CommitSha,
},
```

### `clank feedback write` for ad-hoc

Add `--adhoc` flag (mutually exclusive with `--plan`). When set,
the commit ref resolves against `state.ad_hoc` SHAs. Writes to
the `_` feedback directory.

### Gate computation

For each `AdHocEvent`, scan feedback at
`.clank/agents/*/feedback/_/<sha>.md`. Compute gate:
- No feedback → unreviewed → reviewer work
- All approve → done
- Any request_changes → master revise work

Use the existing `evaluate` logic from `plan_view` or a
simplified version (ad-hoc has no `waiting_on` variants beyond
review/revise).

## Implementation surface

### `crates/core/src/wait.rs`

- Add `AdHocReview` and `AdHocRevise` to `WaitItem`.
- Ad-hoc work derivation lives in the **CLI layer** (not in
  `clank_core::wait::derive_work`) because it depends on
  `ReviewConfig` which is a CLI type. The CLI's wfw module
  calls `derive_work` for plan items as today, then separately
  derives ad-hoc items using `state.ad_hoc` + feedback scan +
  config. Both sets are concatenated into the final work items.
- When `ad_hoc_reviewers` is `Some(list)`, only those agents
  are eligible reviewers for ad-hoc commits. When `None`,
  any reviewer is eligible.

### `crates/cli/src/cli/wfw.rs`

- Load config via `config::load(repo)`.
- Pass `config.review.force_review_on_misc_commits` and ad-hoc
  feedback into `derive_work`.
- Render `AdHocReview` / `AdHocRevise` in emit/JSON.

### `crates/cli/src/cli/stop_hook.rs`

- Render `AdHocReview` / `AdHocRevise` in the continuation
  prompt.

### Lifecycle hooks

Ad-hoc work items skip `firings_from_items` — they have no
`PlanKey` so `HookFiring` can't be constructed. The
`master-work` / `reviewer-work` hooks only fire for plan items.
This is acceptable: ad-hoc reviews are a lightweight side-channel,
not a plan lifecycle event.

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
