# tui-adhoc-review-activity
# TUI: reviewers reviewing an AD-HOC commit show as active

## Why

`clank status --tui` shows a reviewer as 👀-with-spinner only when a
PLAN's `waiting_on` awaits them. A reviewer owed feedback on an
AD-HOC commit (a commit outside any plan) shows 💤 with no spinner —
while their stop-hook wait is actively delivering that very review
item. The gap is structural and self-documented:
`awaited_reviewers` (status_tui/derive.rs) builds its reviewer
projection with `ad_hoc: Vec::new()` because `StatusSnapshot`
doesn't carry the fold's ad-hoc queue at all — `WorkStatus.ad_hoc`
(core wait.rs) exists and drives the real wait, but the TUI's
snapshot never sees it. The panel disagrees with `clank wait`, which
is exactly the class of drift the shared-`is_actionable` routing was
built to prevent.

## What

- `StatusSnapshot` carries `ad_hoc: Vec<AdHocWorkState>`, captured
  from the SAME fold the plans/pr_reviews fields come from.
- **Awaited-reviewer discovery becomes ROSTER-driven** (intro review
  956bed7): today `awaited_reviewers` enumerates candidates from
  plan missing-sets and PR `missing_reviewers`, so ad-hoc reviewers
  can never enter the output — `AdHocWorkState` carries only
  sha/gate, no labels. The revision deletes work-kind-specific
  candidate discovery entirely: candidates are the non-master
  reviewer labels from `StatusSnapshot.agents` (the roster), the
  `WorkStatus` projection carries the REAL `ad_hoc` list, and
  `WorkStatus::is_actionable` alone decides who is active. One
  routing path then covers plan, PR, ad-hoc, their unions, and
  global preemption with zero special cases.
- Survey the TUI's other consumers of the snapshot (timelines,
  agent panel rows) for places that should name the ad-hoc commit
  (short sha) the way plan rows name their plan; add the minimal
  honest labeling, not a new panel.

## Acceptance

- Fixture with a pending ad-hoc commit review: its awaited
  reviewers render 👀 + spinner + "reviewing"; the master renders
  💤. Empty `ad_hoc` renders byte-identically to today.
- An AD-HOC-ONLY snapshot (no plans, no PRs) with reviewers present
  ONLY in the roster shows the right tier active — candidates come
  from `StatusSnapshot.agents`, not from any work item's label set;
  a tier `is_actionable` excludes (e.g. gate-only work vs a
  commit-tier reviewer) stays idle.
- Global preempts still idle everyone: the existing
  head-correction-idles-reviewers invariant extends to a snapshot
  whose ONLY work is ad-hoc (the projection must route through
  `is_actionable`, not pattern-match plan states).
- Plan + ad-hoc union: both sets of awaited reviewers show active.
- In-process tests only.
