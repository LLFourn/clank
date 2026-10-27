# a-repo-with-no-commits-still-has-a-log
# A repo with no commits still has a log

## Problem

In a repo whose branch has no commits yet, `clank log` fails at once with `repo has no HEAD` (crates/cli/src/cli/log.rs:20) and shows nothing. Observed 2026-10-27 in `~/tmp/backup-bip-experiment`: four messages had been sent to `claude`, delivered and acknowledged (`.clank/agents/claude/events/messages.jsonl`), yet the user couldn't see them in the log.

Messages, timers, command sources and GitHub events don't depend on git history. Only the commit and review rows do. An unborn branch should have a log with every non-commit event and simply no commit rows.

## What changes

- `clank log` with no HEAD shows the timeline's events (messages, timers, command and GitHub events, with their acknowledged/unacknowledged marks) in the usual order and format, plus a single dim line saying the branch has no commits yet. `--json` gives the same events with an empty commit list, not an error. Plan filters (`--plan`) apply as usual and simply find no commits.
- No HEAD is a state, not an error: represent it as such at the log's entry (e.g. an `Option` head carried through) rather than special-casing each consumer after the fact. Commit-walking code is skipped when there's no head; event gathering never depends on it.
- An explicit `--range` or revision that can't be resolved still errors, as now: the user asked for history that doesn't exist.
- The same applies to every other surface that shows the timeline: the status TUI's log/timeline view and the web page's feed. Sweep for HEAD lookups (`git_rev_parse(…"HEAD")`, `rev_parse_head`, "has no HEAD") on the paths these surfaces use, and make each either tolerate an unborn branch or explain why it's a genuine error (finish, rereview and preview need commits and keep refusing).

## Out of scope

- Commands that genuinely need a commit (`finish`, `rereview`, history rewriting).
- How events are stored or delivered.

## Acceptance

In-process tests with a real temporary git repo that has no commits (no clank binary spawned):

- With messages delivered to an agent, `clank log`'s output lists them with the right marks and says there are no commits yet; it does not error.
- `--json` returns the events and an empty commit list.
- After the first commit, the same events appear alongside the commit row (the unborn case isn't a separate code path that diverges).
- The TUI's timeline and the web feed show the same events for an unborn repo.
- An explicit `--range` naming a missing revision still errors.
- Mutation check: restoring the early `repo has no HEAD` bail fails the first test.
