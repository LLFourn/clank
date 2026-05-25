# commit-level-gates

## Problem

Gates are computed per-plan but feedback is per-commit. When
two plans share a commit, the same commit can have different
gate states for different plans because:

1. Each plan scans feedback scoped to its own reviewable shas
2. Participant sets differ between plans
3. Filename mode (short vs long SHA) depends on the scope list
4. The stop-hook tells the reviewer they're reviewing "plan X"
   but the approval actually covers all plans at that commit

The correct model: approving a commit means you've read all
active plan files and the implementation is good. The gate
lives on the commit, not on the plan.

## Design

### Gate is per-commit

`derive_status` computes one gate per commit SHA, regardless
of how many plans reference it. The gate for a commit is
determined by the feedback files at
`agents/*/feedback/<sha>.md` — same as `FsReviewLookup`
already does.

### Plan work items reference the shared gate

Each plan still has a "latest reviewable commit" (the most
recent commit that touched the plan or its code). The gate
for that commit is looked up from the global commit-gate
map, not computed per-plan.

### `WorkStatus` simplification

Currently `PlanWorkState` has its own `gate` field computed
independently. Change: `derive_status` builds a
`BTreeMap<CommitSha, CommitGateState>` first (one gate per
unique SHA across all plans), then each `PlanWorkState`
references that map.

### Stop-hook rendering

The reviewer prompt should not say "plan `foo`" — it should
say "commit abc1234" and list which plans are affected. The
reviewer approves the commit, not a plan.

### `scan_feedback` goes away for gate computation

Gate computation uses `ReviewLookup::reviews_for(sha)` which
is already commit-scoped. `scan_feedback` (which takes
plan-specific reviewable shas) is only needed for `status`
display and `log` review listing — not for gate decisions.

### Finish gate

`preview::compute_gate` should use the same commit-level
gate as `derive_status`. Currently it re-derives the gate
from a plan-scoped feedback scan. Change: look up the
latest reviewable commit's gate from the global map.

## Implementation

### `crates/core/src/wait.rs`

- `derive_status`: collect all unique latest-reviewable SHAs
  across all plans, compute one gate per SHA via
  `reviews.reviews_for(sha)`, then assign each plan's gate
  from the map.
- Remove per-plan gate computation loop.

### `crates/cli/src/preview.rs`

- `compute_gate`: use `FsReviewLookup::reviews_for` on the
  target SHA directly instead of `scan_feedback` with
  plan-scoped shas.

### `crates/cli/src/cli/stop_hook.rs`

- Reviewer prompt: show commit SHA and list affected plans
  instead of one plan per line.

### `crates/core/src/wait.rs` — `WaitItem::Reviewer`

- Keep `plan` field (needed for `--plan` filtering) but
  the feedback_path and gate are commit-level.

### `crates/cli/src/cli/status.rs`

- `build_view` can keep using `scan_feedback` for the
  display-level feedback listing, but the gate should come
  from the commit-level computation.

## Tests

- Two plans sharing a commit: approve once → both plans'
  gates are Approved.
- REQUEST_CHANGES on a shared commit blocks both plans.
- Finish gate for plan A uses the same commit-level gate
  as derive_status.
- Stop-hook reviewer prompt shows commit, not single plan.
