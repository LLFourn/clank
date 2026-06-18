# show-adhoc-commits-in-status

`clank status` and `clank status --tui` hide ad-hoc (non-plan) commits
whenever there is exactly ONE active plan.

## Cause

`recent_log_rows` (status.rs) builds the status/tui activity log. When
there is exactly one active plan it sets `plan_filter = Some(plan)` and
then drops every ad-hoc event:

```
LogEvent::AdHoc { .. } => plan_filter.is_none(),   // → false, dropped
LogEvent::PlanIntro/Commit/Finalized/Deleted { plan, .. }
    => plan_filter.is_none_or(|f| f == plan),
```

Observed in `frostsnap`'s `device-prompt-animations` worktree: the only
active plan is `device-look-animation`, but the real work landed as
`[animations]`-tagged commits (ad-hoc, since `animations` is not a
plan). `clank status` therefore shows none of it — the pane looks idle
while real work piles up. `clank log` has no such filter and shows them,
confirming the status filter is the cause.

## Fix

`recent_log_rows` must NOT drop `AdHoc` events. The single-active-plan
narrowing should focus only the PLAN timeline; ad-hoc commits are real,
uncategorized activity and always belong in the log. Concretely:

```
LogEvent::AdHoc { .. } => true,
```

(Plan events keep the `plan_filter` narrowing.)

## Testing (in-process; no binary spawning — [[no-binary-spawning-tests]])

- `recent_log_rows` / the snapshot: with exactly one active plan AND an
  ad-hoc commit in range, the ad-hoc commit IS present in `log_rows`.
  (Reproduce-first: assert it's absent on the current code, then
  present after the fix.)
- Render check: the ad-hoc rows surface under the `AdHoc` umbrella in
  both the TUI log pane and human `clank status`, newest-first.

## Non-goals

- Validating / nagging on mistyped plan-tag prefixes, and the `[misc]`
  explicit-ad-hoc opt-in — that's the separate queued plan
  [[adhoc-commits-and-plan-tag-validation]], which first needs the
  host-repo-prefix design decision (don't nag on `[app]`/`[ci]`).
- Changing ad-hoc CLASSIFICATION in the fold (what counts as ad-hoc is
  unchanged; only whether the status log shows it).
