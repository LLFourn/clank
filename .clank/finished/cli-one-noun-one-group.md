# cli-one-noun-one-group
# Each CLI group owns one noun at one scope

Fixes F2 of `docs/cli-audit.md`. Depends on nothing; breaks typed commands,
so each moved form keeps a hidden alias for one release.

## Problem

- `agent add` has three modes, one of which writes the user-scope agent
  library. `agent remove --global` deletes from it. The user-scope team
  library is `team`. `agent` therefore means both "this repo's roster" and
  "my library".
- Asking is `block create`, answering is top-level `unblock`, and the group
  about claims the group is the ask.
- Bare `open` and `open zellij` are the same command with the same flags.
- (Not a duplicate: `export` prints the raw roster config, `agent list -j`
  resolved runtime rows. `export` stays; see the audit's F2.)

## Change

- Move the agent library under `team` (e.g. `team agent add|remove|list`).
  `agent add` keeps only the repo-scope modes: inline `--tool`, or by name
  from the library.
- One blocking group: `block ask` / `block answer` / `block clean`, with
  `block create` and `unblock` as hidden aliases.
- `open` becomes one action; `open zellij` a hidden alias.

## Acceptance

- Skills, README and runtime text use only the new forms (sweep the whole
  repo, not just `crates/*/src`).
- Parse tests for the new forms and that each alias reaches the same args.
- `docs/cli-audit.md` verdicts updated and the inventory re-blessed.

## As built

- `team agent add|remove|list` own the user-scope agent library. They need
  only `$HOME`: no repo is found or written. `add` takes `--tool`, `--cmd`,
  `--env`, `--initial-prompt` and the tool's arguments after `--`, and
  writes a role-free description through `declare_global_agent`; `remove`
  goes through `remove_global_agent`, keeping its saved-team scrub; `list`
  prints label, tool and launch, or the raw map with `-j`. The old
  `agent add --global --tool` and `agent remove --global` are hidden flags
  that reach the same cores (`library_entry_from_add`). `agent add` now
  documents only its two repo modes.
- `block ask` and `block answer` are the blocking group; `block create` and
  top-level `unblock` are hidden aliases dispatching to the same
  `run_create` / `run_unblock`, with `-m -` stdin wired for all four.
- `open zellij` is hidden; bare `open` is the one action, and the merged
  options from `cli-usage-is-the-contract` still apply to both spellings.
  `open dry` and `open-worktree.sh` are unchanged.
- Active guidance moved to the new forms: the stop hook's block recipe and
  its tests, the master skill and its setup tests, the README (rows and the
  `block ask` line), the TUI's empty agent picker, the unknown-agent and
  team-member errors, the no-pane hint, and `RELEASE-CHECKLIST.md`. The
  README notes the hidden old spellings. `SECTIONS` drops `unblock`.
- README coverage now checks nested operations too (`team`'s row must name
  `agent add` / `agent remove` / `agent list`).
- Tests: old and new block spellings leave identical block/answer files;
  each old spelling parses to the new request; `team agent add` and
  `agent add --global` describe the same entry, launch included; the
  library round trip writes only `~/.clank/config.json`; hidden aliases are
  absent from help yet parse. Mutants caught: `team agent add` dropping the
  launch, and nested README operations unchecked.
- The aliases stay: clank has no release boundary to expire them against.
- `docs/cli-audit.md`: inventory re-blessed, F2 verdicts updated.
