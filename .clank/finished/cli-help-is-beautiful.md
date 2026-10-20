# cli-help-is-beautiful
# clank --help is a pleasure to read

The user asked for the CLI to be beautiful. This plan is the presentation
half of that; the other follow-ups of `docs/cli-audit.md` fix what the help
says. It covers F7 of the audit. Run it after `cli-help-says-what-it-is-for`,
so the words it lays out are already right.

## Problem

`clank --help` is a flat, uncoloured list of 37 commands in source order,
with abouts of every length (one word to a paragraph) and mixed punctuation.
The source groups commands by comment (`// ── getting set up ──` in
`crates/cli/src/cli/command.rs`), but clap does not render those comments.
No command shows an example of use.

## Change

- **Sections.** Render the top level in the audit's proposed sections
  (getting set up, the daily flow, plans, review, the team, talking, running
  remote, settings) from ONE table in `command.rs`. clap 4 cannot put
  subcommands under several headings, so the table drives a help template.
  A test fails when a visible command is missing from the table or the table
  names a command that is not visible, so the table cannot drift.
- **Style.** Set clap `Styles`: bold headings, highlighted literals and
  placeholders, colour only when the terminal supports it (clap's own
  detection, which honours `NO_COLOR`).
- **One-line abouts.** Every about fits one line at 80 columns: sentence
  case, no trailing period, with the detail in the long about. A test walks
  `Cli::command()` and enforces the length and form.
- **Examples.** The commands people type most (`init`, `status`, `queue add`,
  `finish`, `agent add`, `send`, `stash`) end their long help with a short
  `Examples:` block of real invocations. Each example is parsed by a test, so
  none can rot.
- Check the result at 80 columns and in a narrow pane (zellij splits), with
  and without colour.

## Out of scope

The output of the commands themselves (status, list, error lines). Once
help is done, queue a separate plan if that output also needs a consistent
look.

## Acceptance

- Section, about-form and example-parse tests fail on today's tree and pass
  after.
- `docs/cli-audit.md` inventory re-blessed (the abouts change).

## As built

- `command::SECTIONS` assigns each visible top-level command to one
  section by name; summaries come from clap. `cli()` / `cli_at(width)` build
  the top-level help template from it, wrapping each summary under its
  column by display width (terminal width, capped at 100; 80 when unknown).
  Below 24 columns of summary room, each summary stacks under its name.
  `main` parses through `cli()`. The `Plumbing` section holds `monitor`,
  `rewire` and `stop-hook` until `cli-plumbing-out-of-sight` hides them.
- `STYLES` (yellow bold headings and usage, cyan names and placeholders) is
  set on `Cli`, so every subcommand shares it; clap's own colour detection
  and `NO_COLOR` decide whether it shows. clap's `wrap_help` is on, so
  subcommand help wraps too.
- Summaries shortened to fit one line at 80 columns, the detail moved to
  the long help: config keys, `queue add`/`unqueue`, `stash push`/`pop`,
  `team save`, `events reply`, `pr-review start`/`propose`/`finish`,
  `monitor upgrade`, `open zellij`, `nudge`, `attending`, `tui`.
- `Examples:` blocks (long help only) on `init`, `status`, `queue add`,
  `finish`, `agent add`, `send`, `stash`.
- Tests: section coverage; every summary on one rendered row at 80
  columns; summary form; narrow-pane wrapping at 24, 30, 33 and 44 columns
  with no word lost and no overflow but an unbreakable word; styled vs plain rendering; every example shell-split and parsed as
  the command it documents. Each was mutation-checked.
