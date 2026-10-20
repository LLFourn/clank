# cli-help-says-what-it-is-for
# CLI help says what each command is for

Fixes F3 and F4 of `docs/cli-audit.md`. Nothing a user types changes,
except that the ignored `auto on/off --role` no longer parses.

## Problem

Group abouts enumerate their children by hand, and those lists have drifted:
`events` names three of five, `agent` six of eight, `queue` omits `unqueue`,
`auto` promises a role designation that is ignored. `agent set-review`'s
`<REVIEW>` doc omits `standby`. Other help texts describe the implementation
or its history: `agent start` cites a plan name, `agent promote` shows its
config write, `open dry` cites a review sha, `monitor` narrates the design
it replaced, `agent remove` mentions the skeleton dir. `status --tui`'s doc is
two strings spliced together. `nudge`'s help says it "leaves no durable
record", but every attempt is written to the agent's nudge history
(`nudge.rs`, `nudge_log`).

## Change

- Rewrite each group about to state the group's purpose without listing its
  subcommands (clap prints them).
- Rewrite each F4 text from the caller's seat: what it does, when you would
  run it, what it changes.
- Fix the `status --tui` doc and the `set-review` tier list.
- `nudge`'s help distinguishes the attempt history it keeps from the inbox
  item that `send` leaves to be acknowledged.
- Remove `auto on/off --role` (a no-op since roles became roster-derived),
  and its "Optionally update role" wording.

## Acceptance

- A unit test in `cli::command` walks `Cli::command()` and fails when a group's
  about or long about names one of its own subcommands as a word in a
  backticked or bare list (exact rule decided in implementation; it must
  fail on today's `events` about).
- `docs/cli-audit.md` verdicts for the touched paths are updated, and the
  inventory test still passes.

## As built

- Group abouts state the group's purpose; clap lists the children.
  `no_group_help_lists_its_own_subcommands` walks `Cli::command()` with
  `hand_listed_children`, which finds runs of two or more child names
  linked by `,` `/` `|` (bare, backticked, or compact like `list/ack/show`)
  and lets single explanatory references and `Examples:` blocks through.
- Rewritten from the caller's seat: `agent` subcommands (`start` now says a
  same-tool binding resumes and a tool change starts fresh; `add` gives the
  master-when-none default), `open dry`, `status --tui`, `html open`,
  `team delete`, `monitor`, `nudge` (attempt history vs `send`'s inbox),
  `rereview`, `block`, `stash`, `queue`, `feedback`, `events`, `team`,
  `auto`. `set-review` and `agent add --review` list `standby`.
- `auto on/off --role` removed with `RoleArg` and the shared skill's mention;
  this is the one change to accepted syntax.
- `docs/cli-audit.md` verdicts and inventory updated.
