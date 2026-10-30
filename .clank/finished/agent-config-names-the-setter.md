# agent-config-names-the-setter
# `clank agent config` sets what an agent runs

## Problem

`clank agent launch` (set or show how an agent's tool is run) and
`clank agent start` (run it) read as two names for the same act. Lloyd:
"we have launch and start that's too confusing for me."

## Change

- Rename the subcommand `agent launch` to `agent config`. Its behaviour is
  unchanged: bare, it shows the profile and the command `agent start` would
  run; `--cmd`, `--env`, `-- <args>` replace it; `--clear` empties it.
  `launch` stays as a hidden alias, the way `promote` is for `make-master`,
  and the old-spelling test lists it.
- Rename `AgentLaunchArgs` and the `Launch` variant to match.
- Sweep the whole repo for the user-facing name: README, book, skills,
  error and hint strings (e.g. the OpenCode fork refusal "Change the profile
  with `clank agent launch`"), and `--help` text. Prose calls it the agent's
  config, or its launch profile where the stored `launch` field is meant.
- The stored `launch` key in `.clank/config.json` keeps its name. It is a
  durable format, and renaming it would need a migration for no user gain.

## Out of scope

- Changing what the profile holds, or how `agent start` composes a launch.

## Acceptance

- `clank agent config <label> -- --standalone` sets the profile, and
  `clank agent config <label>` shows it.
- `clank agent launch …` still works, hidden from help.
- A whole-repo grep finds `agent launch` only in the alias, its test,
  finished plans and this plan.
- `cli::command` (README reference and old-spelling tests), `book_is_true`,
  and the `cli::agent` tests pass.
