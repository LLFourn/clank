# status-tui-live-log
# status TUI: live log pane below the status view

`clank status --tui` currently renders the status view and leaves the rest of
the terminal unused. Use the extra vertical space below the status block to
show a live log of recent activity, in the style of `clank log --oneline`
(one line per commit/review event, most recent visible).

Sketch:
- Reuse the existing `clank log --oneline` rendering (or its underlying
  timeline-folding logic) rather than reimplementing formatting; factor it
  into a shared function if needed.
- Fill whatever rows remain below the status view with the most recent N
  events, where N = available height.
- "Live": the TUI already refreshes; the log pane should update on the same
  cycle so new commits/reviews appear as they happen.
- Relevant code: `crates/cli/src/cli/status_tui.rs` (TUI), `clank log`
  implementation for the oneline formatter.

Open questions for the implementer:
- Whether to scope the log to the active plan (like `clank log --plan`) or
  show repo-wide events. Default to the active plan if there is one,
  repo-wide otherwise, unless something simpler falls out of the code.

## Promote-time notes (verified against the code, 2026-06-10)

- `print_oneline` (log.rs:318) exists but `println!`s directly and
  uses global `color()` (isatty) — the reuse is a real factoring:
  extract a line-PRODUCING function (plain text, no ANSI; e.g.
  `oneline_lines(events, reviews) -> Vec<String>`) that both
  `clank log --oneline` and the snapshot builder call. The TUI
  styles the lines itself (dim, via its existing span model) —
  don't thread ANSI through the shared formatter.
- TUI architecture rules (established by clank-status-tui) apply:
  the SNAPSHOT stays the single source of truth — StatusSnapshot
  grows the recent-events lines (capped, most recent first;
  ~30 is plenty — note `commit_subject` shells git per event, so
  the cap also bounds subprocess count per refresh), and
  `render(snapshot, rows, cols)` stays a PURE function — the log
  is the new LOWEST tier, filling whatever rows remain after the
  existing gauges, each line dim + width-truncated. Unit-test the
  tier in the existing pure-render suite (fills leftover rows;
  drops entirely when none; most-recent visible).
- Check whether the status fold (`rebuild_repo`'s RepoState)
  already retains `LogEvent`s or whether log.rs's range-fold
  helper needs calling in from_state — reuse whichever is already
  on hand; don't fold twice.
- Plan-scoping default per the stub: the single active plan's
  events when exactly one plan is active, repo-wide otherwise.
