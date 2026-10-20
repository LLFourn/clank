# cli-usage-is-the-contract
# The usage line is the contract

Fixes F5 of `docs/cli-audit.md`.

## Problem

clap shows things optional that the command then refuses without:
`unfinish [PLAN]` bails (`unfinish.rs`), and `rewire` refuses without
`--from-stdin`. `--repo` declared on the groups `queue`, `pr-review`, `html`,
`config` and `open` without `global = true` is rejected after the
subcommand (`clank queue promote foo --repo .` fails), while `remote`'s is
global and every leaf command accepts it after its own name. `config <key>
[ACTION] [VALUE]` takes `get`/`set` as a free string. `feedback write
--author` is required where every other author flag defaults from the
session binding.

## Change

- `unfinish`'s plan positional required in clap; `rewire` either requires
  `--from-stdin` in clap or drops the flag.
- `global = true` on every group-level `--repo`. Keep the flags-before form
  working, since `html_open_argv` and older panes spawn it.
- `config <key> [value]`: value present means set. Accept `get`/`set` as a
  hidden form for scripts that use it.
- `feedback write --author`: find out whether the explicit author is
  deliberate (gate attribution, reviewers writing for another label). Default
  it from the binding only if nothing depends on it being explicit; either
  way, record the reason in the arg's doc.

## Acceptance

- Parse tests for each changed form, including `--repo` after the
  subcommand for each group.
- `docs/cli-audit.md` verdicts updated and the inventory re-blessed.

## As built

- `config <key> [value]`: `key_op` decides the one operation before any
  read or write. No value describes the key (key, type, default, value,
  source, help, unchanged); a value writes it; the older `get` (bare value
  read) and `set <value>` still work, and the words themselves are written
  as `set get` / `set set`. `set` alone, `get <x>` and a second value are
  errors, never a write. Type validation, null handling, unknown fields and
  `--global` layering are unchanged. Advice clank prints (`diff`'s missing
  editor, setup's autosquash notice) uses the new spelling, and a test
  parses both as writes.
- `global = true` on the group `--repo` of `queue`, `pr-review`, `html` and
  `config` (plus config's `-j` and `--global`), so they are read before or
  after the subcommand; `html_open_argv`'s flags-before form still parses.
  `open` drops `args_conflicts_with_subcommands`: `open::zellij_args` merges
  the opener's options from both sides of `zellij`, so `clank open --repo R
  zellij` and `clank open zellij --repo R` are one request. An option given
  on both sides must agree, and `--fork`/`--pr`/`--all` stay one target
  across them. `open dry` refuses any opener option before it, since its
  repo is the one containing its path.
- `unfinish <PLAN>` is required in clap and its runtime check is gone;
  `rewire --from-stdin` is required in clap, keeping the installed hook
  string.
- `feedback write --author` stays explicit: the gate attributes each verdict
  to it, so it is written out rather than guessed from the session. Its
  help says so.
- Tests: `a_key_operation_is_one_unambiguous_thing`,
  `a_group_repo_is_read_wherever_it_is_written`,
  `config_repo_after_the_key_writes_that_repo` (in-process dispatch),
  `the_config_commands_clank_suggests_are_writes`,
  `unfinish_and_rewire_say_what_they_require`. Mutants caught: old `get` as
  a write, a second value accepted, `queue --repo` not global, the options
  before `zellij` dropped, and `open dry`'s refusal removed.
- `docs/cli-audit.md`: inventory re-blessed, F5 verdicts updated.
