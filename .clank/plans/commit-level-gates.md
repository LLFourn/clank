# commit-level-gates

## Problem

Feedback is per-commit (`agents/*/feedback/<sha>.md`) but
gate computation is scoped to one plan's reviewable timeline.
This causes:

1. `scan_feedback` resolves SHA refs against one plan's
   reviewable set — a feedback file written while reviewing
   a shared commit may not resolve for a different plan's
   scope
2. `preview::compute_gate` builds participant sets from
   plan-scoped feedback, so the same commit can show
   different gate states for different plans
3. `derive_status` already uses `ReviewLookup::reviews_for`
   which is correctly commit-scoped — but `status` and
   `finish` use the older `scan_feedback` path

## Semantics

A commit attributed to plans P (via `[foo,bar]` prefix)
carries approval for the plans in P. It does NOT approve
plans outside P. Each plan tracks its own latest reviewable
commit — the gate on that commit determines whether the
plan can advance.

The gate for a commit is determined by ALL feedback files
for that SHA, not filtered by any one plan's timeline.

## Design

### Fix gate computation to be SHA-scoped

Everywhere that computes a gate for a commit SHA should
use `reviews_for(sha)` (which scans all agents' feedback
for that exact SHA) instead of `scan_feedback` scoped to
one plan's reviewable set.

`derive_status` already does this correctly. The fix is
in `preview::compute_gate` and `status::build_view`.

### `preview::compute_gate`

Replace `scan_feedback(repo, &reviewable)` with a direct
`FsReviewLookup::reviews_for(target_sha)` call. The gate
is just `compute_gate(&entries)` on the result.

### `status::build_view`

The `plan_view::project` function takes a `FeedbackView`
built from `scan_feedback`. For display purposes (showing
which reviewers weighed in per commit), this is fine. But
the gate state should come from the SHA-scoped lookup,
not from the plan-scoped scan.

### `scan_feedback` stays for display

`scan_feedback` is still useful for `clank log` and
`clank status` — it shows which feedback exists for which
commits within a plan's timeline. It just shouldn't be
the source of truth for the gate.

## Implementation

- `preview.rs`: `compute_gate` uses `FsReviewLookup`
  instead of `scan_feedback`.
- `status.rs`: gate in `build_view` comes from
  `FsReviewLookup::reviews_for` on the latest reviewable.
- No core changes needed — `derive_status` is already
  correct.

## Tests

- Two plans share a commit, approve once → both plan
  gates are Approved in status and finish preview.
- Finish gate matches derive_status gate for the same SHA.
