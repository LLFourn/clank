# adhoc-review

## Summary

Add a setting that makes `clank wfw` require review on ad-hoc
(non-plan) commits. When enabled, master's wfw treats unreviewed
ad-hoc commits like unreviewed plan commits — it waits for
reviewer feedback. REQUEST_CHANGES on an ad-hoc commit produces
a master work item to address it.

Off by default. Configurable in `~/.clank/config.json` (user
default) shadowed by `.clank/config.json` (repo override).

## Motivation

Not every commit needs a full plan cycle, but some repos want
all commits reviewed — even small operational changes, formatting
fixes, or one-off bug fixes that don't warrant a plan. This
setting extends clank's review surface to cover the gaps between
plans.

## Config

New field `review_adhoc: bool` in the repo-level config
(`.clank/config.json`) and user-level config
(`~/.clank/config.json`). Repo shadows user.

```json
{
  "review_adhoc": true
}
```

Loading: read `~/.clank/config.json` first (user default), then
overlay `.clank/config.json` (repo override). Same merge strategy
as hooks.json.

## Core model changes

### Ad-hoc commits become reviewable

When `review_adhoc` is enabled:

- `AdHocEvent` in the fold gains a reviewable surface. The
  ad-hoc commit's SHA is a reviewable target — feedback files
  can be written against it at `.clank/agents/<author>/feedback/
  _/<sha>.md` (using `_` as the plan key for ad-hoc commits,
  matching the existing `FeedbackTarget::AdHoc` if one exists,
  or introducing one).
- The gate on an ad-hoc commit works the same as a plan commit:
  unreviewed → waiting for reviewer, approved → done,
  request_changes → master to address.

### Wait-for-work changes

`derive_work` gains a new branch when `review_adhoc` is enabled:

- **Reviewer**: if there are unreviewed ad-hoc commits, emit
  `WaitItem::Reviewer` for each (with `plan` set to a sentinel
  like `_` or a new `WaitItem::AdHocReviewer` variant).
- **Master**: if a reviewer requested changes on an ad-hoc
  commit, emit `WaitItem::Master` with `next: Revise` so master
  can address it (amend, new commit, etc.).
- Approved ad-hoc commits produce no further work items.

### Feedback path

Ad-hoc feedback lives at `.clank/agents/<author>/feedback/
_/<sha>.md`. The `_` directory distinguishes ad-hoc feedback
from plan feedback. `scan_feedback` is extended to scan this
path when `review_adhoc` is enabled.

The `clank feedback write` command needs to accept ad-hoc
commits: `--plan _` or a new `--adhoc` flag.

## Implementation

### Config loading

New `crates/cli/src/repo_config.rs` (or extend existing config):
- `RepoConfig { review_adhoc: bool }` with serde defaults.
- `load_repo_config(repo) -> RepoConfig`: load
  `~/.clank/config.json` then overlay `.clank/config.json`.
- Pass `review_adhoc` into `derive_work`.

### `derive_work` extension

`derive_work` signature gains `review_adhoc: bool` (or a
config struct). When true, it inspects `state.ad_hoc` for
unreviewed commits and emits work items.

### Ad-hoc gate computation

For each `AdHocEvent` in `state.ad_hoc`, scan feedback at
`.clank/agents/*/feedback/_/<sha>.md`. Compute gate state
(unreviewed / approved / changes_requested) using the same
logic as plan commits.

### `clank feedback write --plan _`

Allow `_` as a plan key that routes to the ad-hoc feedback
directory. The commit ref resolves against `state.ad_hoc`
SHAs instead of a plan's reviewable SHAs.

### `clank log` integration

Ad-hoc commits with reviews show the review content in
`clank log` output (already partially there via `LogEvent::
AdHoc` + feedback scanning — needs the `_` feedback path
to be scannable).

## Tests

- `review_adhoc=false` (default): ad-hoc commits produce no
  wfw work items.
- `review_adhoc=true`: unreviewed ad-hoc commit → reviewer
  wfw returns work.
- `review_adhoc=true` + reviewer approves → no more work.
- `review_adhoc=true` + reviewer request_changes → master
  wfw returns revise work.
- `clank feedback write --plan _ --commit <sha>` writes to
  the ad-hoc feedback path.
- Config shadowing: repo overrides user.

## Acceptance criteria

- `review_adhoc` setting in repo + user config, repo shadows
  user.
- When enabled, ad-hoc commits are reviewable via wfw.
- Reviewer and master work items for ad-hoc commits follow
  the same approve/request_changes flow as plan commits.
- Off by default. Enabled for this repo.
