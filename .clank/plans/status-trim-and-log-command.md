# status-trim-and-log-command

## Summary

Two changes:

1. **`clank status`**: show only the 3 most recent finished plans
   instead of all of them. `--all` shows the full list.
2. **`clank log`**: new command that outputs a timeline of events
   from the fold — commits and reviews interleaved
   chronologically.

## Part 1: status trim

In `crates/cli/src/cli/status.rs::print_human` (line 212-222),
change the finished-plans render from iterating all entries to
taking the last 3:

```rust
let recent: Vec<_> = state.fold.finished_plans.iter().rev().take(3).collect();
```

Show a count header like `finished plans (3 of 14):` so the
operator knows there are more. `--all` already exists on status
and should show the full list.

Same trim for the JSON builder (`build_json`).

## Part 2: clank log

### What the fold gives us

Each active `PlanState` has `commits: Vec<PlanTimelineEvent>`
with `{ sha, ts, touched_plan, touched_code }`. Each
`FinishedPlan` has `{ plan, intro, finalized_at }`.

Reviews are NOT in the fold — they're local-only feedback files
at `.clank/agents/<author>/feedback/<plan>/<sha>.md`. The
`feedback_scan` module already reads these into a
`FeedbackView` keyed by `(plan, sha) → Vec<(author, verdict)>`.

### Event model for log

A log entry is one of:

- **commit** — from `PlanTimelineEvent`. Plan-scoped. Has sha,
  ts, touched_plan, touched_code. Commit subject from git.
- **review** — from feedback files. Virtual event placed after
  the commit it reviews. Has plan, sha (of the reviewed commit),
  author, verdict.
- **finalize** — from `FinishedPlan`. Plan-scoped. Has plan,
  finalized_at sha, ts from git.

### Ordering

Commits are ordered by `ts` (committer timestamp from the fold).
Reviews are placed immediately after their target commit (the
commit they provide feedback on). Within the same commit's
reviews, order by author name.

Finalize events are ordered by the finalized_at commit's
timestamp.

### Rendering

Human mode (default):

```
[foo] intro          abc1234  2026-05-24 10:00  plan
  ✓ codex approved
[foo] implement      def5678  2026-05-24 11:00  code
  ✗ codex request-changes
[foo] address codex  ghi9012  2026-05-24 12:00  plan+code
  ✓ codex approved
Finalize foo         jkl3456  2026-05-24 13:00
```

JSON mode (`--json`): array of typed event objects.

### CLI

```
clank log [--plan <stem>] [--all] [--json] [--repo <path>]
```

- No args + one active plan: show that plan's timeline.
- `--plan <stem>`: specific plan.
- `--all`: all plans interleaved chronologically.
- No active plans + no `--plan`: show the most recent finished
  plan's timeline.

### Implementation

New file `crates/cli/src/cli/log.rs`. Fold the repo, collect
events from active + finished plan timelines, scan feedback for
review events, sort chronologically with reviews after their
target commit, render.

The feedback scan is already available via
`crate::feedback_scan::scan_feedback`. Commit subjects come from
`git log --format=%s <sha> -1`.

Register in `crates/cli/src/cli/mod.rs` as `Command::Log` and
in `main.rs`.

## Tests

### Status trim
- Status with >3 finished plans shows only the 3 most recent.
- Status with `--all` shows all finished plans.
- Status with ≤3 finished plans shows all (no "of N" noise).

### Log
- Log with one active plan shows commit + review timeline.
- Log with `--plan` on a finished plan shows its timeline.
- Log JSON mode produces typed event array.

## Acceptance criteria

- `clank status` shows at most 3 finished plans by default.
- `clank log` outputs a chronological timeline of commits and
  reviews for one or more plans.
- Both commands work for active and finished plans.
