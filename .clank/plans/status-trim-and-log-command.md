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

JSON mode keeps all finished plans (no truncation) — consumers
need the full list for programmatic use. Only human mode trims.

## Part 2: clank log

### Architecture: log events from the fold

`apply_commit` currently mutates `RepoState` in place. Change it
to also return a `Vec<LogEvent>` describing what it did. This is
purely additive — no change to existing data structures, cache
encoding, or fold behavior. `LogEvent` is a new type in
`crates/core`.

```rust
pub enum LogEvent {
    PlanIntro { plan: PlanKey, sha: CommitSha, ts: i64 },
    PlanCommit { plan: PlanKey, sha: CommitSha, ts: i64,
                 touched_plan: bool, touched_code: bool },
    PlanFinalized { plan: PlanKey, sha: CommitSha, ts: i64 },
    PlanDeleted { plan: PlanKey, sha: CommitSha, ts: i64 },
}
```

The fold loop in `rebuild` collects these events into a
`Vec<LogEvent>` alongside the `RepoState`. Existing callers
(`status`, `wfw`, etc.) ignore the events.

### Reviews as virtual events

Reviews are NOT in the fold — they're local-only feedback files.
`clank log` scans feedback via `feedback_scan::scan_feedback`,
then attaches reviews as virtual events placed immediately after
their target commit. Within a commit's reviews, order by author.

### Ordering

Log events are already in fold order (chronological by commit).
Reviews are interleaved after their target commit's event.

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
- `--plan <stem>`: specific plan (active or finished).
- `--all`: all plans interleaved chronologically.
- No active plans + no `--plan`: show the most recent finished
  plan's timeline.

### Implementation

**`crates/core`**: new `LogEvent` enum. `apply_commit` signature
changes to return `Vec<LogEvent>`. Fold callers updated to
collect or discard the events.

**`crates/cli/src/rebuild.rs`**: the rebuild loop collects log
events from each `apply_commit` call. `RepoState` result type
extended to carry the events (or they're returned separately).

**`crates/cli/src/cli/log.rs`**: new file. Rebuilds the repo,
filters log events by plan, scans feedback for the relevant
SHAs, interleaves reviews, renders. Commit subjects come from
`git log --format=%s <sha> -1`.

Register in `crates/cli/src/cli/mod.rs` as `Command::Log` and
in `main.rs`.

No changes to `RepoState`, `FinishedPlan`, `PlanState`, or the
cache encoding.

## Tests

### Status trim
- Status with >3 finished plans shows only the 3 most recent.
- Status with `--all` shows all finished plans.
- Status with ≤3 finished plans shows all (no "of N" noise).

### Log
- Log with one active plan shows commit + review timeline.
- Log with `--plan` on a finished plan shows its timeline
  including the intro commit, later implementation commits, and
  their reviews.
- Log JSON mode produces typed event array.
- `apply_commit` return type change doesn't break any existing
  tests (callers updated to collect/discard events).

## Acceptance criteria

- `clank status` shows at most 3 finished plans by default.
- `clank log` outputs a chronological timeline of commits and
  reviews for one or more plans.
- Both commands work for active and finished plans.
