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

Use `FeedbackTarget::AdHoc` (check if this already exists in
`disk_format.rs`). Feedback path:
`.clank/agents/<author>/feedback/_adhoc/<sha>.md`.

The `_adhoc` directory name is safe (starts with underscore,
can't collide with plan stems which are `[a-z0-9][a-z0-9._-]*`).

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
the `_adhoc` feedback directory.

### Gate computation

For each `AdHocEvent`, scan feedback at
`.clank/agents/*/feedback/_adhoc/<sha>.md`. Compute gate:
- No feedback → unreviewed → reviewer work
- All approve → done
- Any request_changes → master revise work

Use the existing `evaluate` logic from `plan_view` or a
simplified version (ad-hoc has no `waiting_on` variants beyond
review/revise).

## Implementation surface

### `crates/core/src/wait.rs`

- Add `AdHocReview` and `AdHocRevise` to `WaitItem`.
- `derive_work` gains `ad_hoc: &[AdHocEvent]` + ad-hoc feedback
  + `force_review: bool`. When force_review is true, emits work
  items for unreviewed/changes-requested ad-hoc commits.

### `crates/cli/src/cli/wfw.rs`

- Load config via `config::load(repo)`.
- Pass `config.review.force_review_on_misc_commits` and ad-hoc
  feedback into `derive_work`.
- Render `AdHocReview` / `AdHocRevise` in emit/JSON.

### `crates/cli/src/cli/stop_hook.rs`

- Render `AdHocReview` / `AdHocRevise` in the continuation
  prompt.

### `crates/cli/src/cli/feedback.rs`

- Add `--adhoc` flag to `FeedbackWriteArgs`.
- When set, resolve commit against `state.ad_hoc` SHAs.
- Write to `_adhoc` feedback directory.

### `crates/cli/src/feedback_scan.rs`

- Extend to scan `_adhoc` directory for ad-hoc feedback.

## Tests

- `force_review_on_misc_commits=true` (default): unreviewed
  ad-hoc commit → reviewer wfw returns work.
- Reviewer approves → no more work.
- Reviewer request_changes → master wfw returns revise.
- `force_review_on_misc_commits=false`: no ad-hoc work items.
- `clank feedback write --adhoc --commit <sha>` writes to
  `_adhoc` path.

## Acceptance criteria

- Ad-hoc commits surface as wfw work items when
  `force_review_on_misc_commits` is true.
- Reviewer and master flows work for ad-hoc commits.
- Feedback lives at `_adhoc/<sha>.md`.
- `clank feedback write --adhoc` works.
- Default is true (existing config default). Set to true for
  this repo (already the default — no config change needed).
