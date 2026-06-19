# status-tui-unified-log

Make `clank status --tui`'s log pane show the SAME repo-wide recent
activity regardless of how many plans are active. Drop the special-case
that narrows the log to the single active plan.

## Problem

`recent_log_rows` (`crates/cli/src/cli/status.rs`) branches on the
active-plan count:

```rust
let plan_filter = match active.as_slice() {
    [only] => Some(only),   // exactly one active plan → narrow to it
    _      => None,         // zero or many → repo-wide
};
```

With exactly ONE active plan it shows that plan's events + all ad-hoc
commits, but DROPS every other plan's commits — including a plan that
just finished and whose commits are still in the recent window. So a
repo with one active plan + a recently-finished plan renders a gappy,
"mangled" log: ad-hoc commits remain but the finished plan's whole
commit run vanishes. Finishing the active plan (active count → 0) makes
them all reappear, which is the surprise that surfaced this.

The single-plan focus isn't worth the confusion: the log pane is a
recent-activity view, and "what shows" shouldn't flip based on the
active-plan count.

## Change

In `recent_log_rows`, remove the `active` / `plan_filter` computation
and the per-event plan filter. The view is always repo-wide: build
oneline rows from ALL events in the `LOG_WINDOW`, exactly the
`plan_filter == None` branch today. Once narrowing is gone the event
filter collapses to a no-op (every `AdHoc`/`Plan*` event passes), so
delete it and feed the window's events straight through —
`reviewable` collection, reviews lookup, newest-first, `oneline_rows`
all unchanged.

Update the doc comment (currently "Scope: the single active plan's
events when exactly one plan is active, repo-wide otherwise") to
"repo-wide recent activity, always."

## Testing (in-process; no binary spawning — [[no-binary-spawning-tests]])

- Reproduce-first: a repo with exactly one active plan `foo` PLUS a
  commit belonging to a DIFFERENT plan (a second plan, or one finished
  within the window) — assert that other plan's commit now appears in
  `log_rows` (it was dropped before this change).
- Keep `status_shows_adhoc_commits_with_one_active_plan` green (ad-hoc
  still shows; now plan commits do too).

## Non-goals

- The umbrella grouping / row rendering and the `LOG_WINDOW` size are
  unchanged.
- The commit classifier (tag-or-plan-touch attribution) is untouched —
  this is purely which events the log PANE displays.
