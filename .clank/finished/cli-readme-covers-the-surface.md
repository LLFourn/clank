# cli-readme-covers-the-surface
# The README covers the whole visible CLI

Fixes F8 of `docs/cli-audit.md`.

## Problem

The README command reference has no row for `remote`, `nudge`, `attending`,
`run` or `rereview`, and omits `agent swap`, `agent launch`, `events reply`,
`events read` and `queue unqueue`. `run` and `attending` are taught only by
the stop hook's runtime text (`stop_hook.rs`), not by any skill. The table's
`status` row credits `--watch` with TUI key bindings. The test
`the_readme_names_only_real_subcommands` checks only that what the README
names exists, not that what exists is named.

## Change

- Add the missing rows, and explain `send` / `send --interrupt` / `nudge` as
  three levels, and `run` vs `attending`.
- Teach `run` (and `attending` as its fallback) in the master and reviewer
  skills.
- Correct the `status` row.
- Extend the README test: every visible (non-hidden) top-level command has a
  table row, and every visible subcommand is named in its parent's row.
  Hidden commands are exempt by design.

## Acceptance

- The reverse-direction test fails on today's README and passes after.
- Run the `setup` skill tests that pin skill text.

## As built

- README reference table: rows for every visible command, `monitor` (still
  visible until `cli-plumbing-out-of-sight`) included, each naming its
  visible subcommands; `status` no longer credits `--watch` with the TUI's
  keys, which moved to the `tui` row. Two subsections after the table:
  `send` / `send --interrupt` / `nudge` as three levels (nudge keeps an
  attempt history, nothing to acknowledge), and `run` vs `attending`
  (`--pid` required, `--expect` a one-time check-in, nothing cancelled).
- `run`/`attending` guidance is in the shared skill core, so every role on
  every tool gets it: `run` for new work, `attending` with the task id and
  `--pid` for running work, the one `--expect` check-in, and that the
  harness's completion still wakes you, with no polling and no second wait.
  The codex reviewer skill's 5,700-byte ceiling held by tightening two
  reviewer invariants (milestones, manual steps) without dropping a rule.
  `every_skill_teaches_background_attendance` asserts it in the composed
  master and reviewer skills for claude, codex, grok and opencode.
- `readme_gaps` walks the real clap tree: a row per visible top-level
  command (the shared `block` / `unblock` row counts for both), and each
  visible child named in its own parent's row. Hidden commands, aliases and
  `help` are excluded. On the old README it reported 30 gaps. Its control
  test removes a row, unquotes a child, moves a child to another row, and
  splits the shared row; mutants that search the whole README or drop the
  missing-row report are caught.
- `docs/cli-audit.md`: F8 verdicts updated.
