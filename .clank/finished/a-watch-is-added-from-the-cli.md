# a-watch-is-added-from-the-cli
# An agent's wake sources are listed, added and removed from the CLI

## Why

The only way to give an agent a standing wake source (`wait_events`: a
GitHub watch, a command source, a timer) is to hand-edit
`.clank/agents/<label>/config.json`. That bypasses the binding lock
that every other agent-config write takes, a typo is only discovered
when the next arm refuses the whole config, and the README tells people
to do it.

The trigger is that we're adopting a PR-based workflow for clank
itself: the master watches `LLFourn/clank` for `ci_passed` /
`ci_failed` and merges a green PR. That watch should be set up with a
command, not an edit.

## What changes

A `watch` subcommand group under `clank events`, since it configures
what lands in that inbox:

- `clank events watch list [--repo P] [--agent L] [-j]` shows the
  agent's configured sources, numbered from 1. Each line gives the kind
  and a one-line summary (repo plus watched objects and events, the
  command's argv, or the timer's name and due time), and says whether
  it has a prompt. `-j` prints the sources in exactly their config
  JSON.
- `clank events watch add '<json>' [--repo P] [--agent L]` takes one
  source in exactly the shape `clank wait --event` and `wait_events`
  already use.
  - It runs the SAME validation an arm runs (`wait.rs`'s source
    parsing and checks), so anything an arm would refuse is refused
    here, with the arm's own message, and nothing is written.
  - An exact duplicate of an existing source is refused.
  - The write goes through `agent_store`'s locked update
    (`update_agent_config`), never a raw file write.
  - On success it prints the new numbered entry.
- `clank events watch remove <n> [--repo P] [--agent L]` removes entry
  `n` from the list's numbering, under the same lock, and prints what
  it removed. It names no source that list did not show. The existing
  arm-time rules keep their meaning: an unhandled event survives
  removal of its source, and a timer's removal cancels its future
  schedule.

`--agent` defaults to the session's binding, as `clank events list`
does with `--author`. Use whichever spelling the `events` group already
uses for that flag, so the group stays consistent.

The README's "Extra wake sources" section shows the command first. It
keeps the JSON shape, since that is the argument, and drops the
instruction to edit the config file. The clank-github skill's "Saying
what to watch" section names the command the same way.

## Out of scope

- No flag-per-field builder (`--github R --pr-events …`). The nested
  JSON shape is the vocabulary, and a second spelling of it would have
  to be kept in sync.
- No edit-in-place. Remove and add again, which is also what an edit
  means to the log (a changed source starts a fresh log).

## Acceptance

- Unit tests drive the command cores in-process. They never spawn the
  clank binary.
  - `add` writes a valid github, command and timer source, and `list`
    shows each one.
  - `add` refuses invalid JSON, an unknown kind, an unknown github
    event or object (with the arm's message), and an exact duplicate.
    After each refusal the config bytes are unchanged.
  - `remove` deletes exactly entry n and leaves the others in order.
    An out-of-range n is refused, with the config unchanged.
  - Writes leave the agent's other fields (`session`, `auto_mode`,
    `resume_pending`) untouched.
- The refusal path is shared, not copied. Mutation check: weaken the
  arm's validation of an unknown github event, and the `add` refusal
  test must go red. That proves `add` calls the arm's validation and
  doesn't keep its own copy.
- `no_json_literal_config_writes` and the `cli::command` README tests
  pass.
- `cargo test -p clank --lib` for the touched modules and the whole
  lib, plus clippy and fmt.

## As landed

As planned, with these differences:

- **The flag is `--author`, not `--agent`.** It matches the rest of the `events` group.
- **Validation is shared with the arm.** `wait.rs` now exposes `parse_wait_event_source` and `validate_wait_event_sources`, with Config provenance, so `receipt_command` stays a configured-source feature. `add` validates the whole candidate list inside `agent_store::try_update_agent_config` under the binding lock, and that list check catches timer-name and storage-key collisions as well as exact duplicates. A refusal writes nothing and creates no config file. Taking the lock does create the agent's directory and `binding.lock`.
- **`remove` checks only the entry number.** It doesn't validate the entries that remain, because removing entries is how a config that an arm refuses gets repaired.
- **Removing and re-adding a source does not always start a fresh log**, contrary to the "Out of scope" shorthand. A command source is identified by its name, and a GitHub watch by its repo and what it admits. So a new argv under the same name, or a changed GitHub `prompt` / `poll_interval` / `delivery`, keeps the log. The README states these identity rules.
- **Known gap:** `add` with a mistyped `--author` creates a config for that label, as `update_agent_config` always has.
