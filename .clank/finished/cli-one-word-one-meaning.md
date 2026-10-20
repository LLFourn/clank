# cli-one-word-one-meaning
# One CLI word, one meaning

Fixes F6 of `docs/cli-audit.md` (F7, the sectioned help, is
`cli-help-is-beautiful`). Breaks typed forms, each kept as a
hidden alias for one release. Best done after `cli-one-noun-one-group`, so the
renames land on the final tree.

## Problem

- `promote` is both "activate a queued plan" and "make this agent master";
  the `agent promote` help apologises for it.
- Dry runs are `--dry-run` (`setup`) and `--dry` (`finish`, `stash`, `pick`,
  `purge`, `block migrate`). Printing is `--print` and `--print-path`.
- The roster label is `<LABEL>` in `agent add`, `agent remove` and `as`, and
  `<NAME>` in `agent start`, `promote`, `set-review` and `launch`.
- Removal verbs vary (`remove`, `delete`, `drop`, `clean`) where nothing
  distinguishes them; `team delete` is the stray.

## Change

- Rename `agent promote` (e.g. `agent make-master`).
- `--dry-run` everywhere, `--dry` hidden. One print flag.
- `<AGENT>` for the roster label everywhere.
- `team delete` → `team remove`. Keep `stash drop` (it discards commits) and
  `queue unqueue` (it keeps the body).

## Acceptance

- Whole-repo sweep for the old spellings, keeping only the aliases and their
  tests.
- `docs/cli-audit.md` verdicts updated and the inventory re-blessed.

## As built

- `agent make-master` is the canonical name, with `promote` a hidden alias
  on the same variant, so both reach the same roster-transition core.
  `queue promote` is unchanged.
- `team remove` is the canonical name for deleting a saved team, with
  `delete` a hidden alias on the same variant; `--force` policy unchanged.
  `queue remove`/`unqueue`, `stash drop` and `block clean` keep their own
  meanings.
- `--dry-run` everywhere (`finish`, bare `stash` and `stash push`, `pick`,
  `purge`, hidden `block migrate`), `--dry` a hidden alias on the same
  field, so the no-effect mode is unchanged; `stash`'s refusal of parent
  options before a verb names `--dry-run`.
- `html open --print` is canonical, `--print-path` a hidden alias: same
  field, same path-only stdout and no browser.
- `<AGENT>` for every value naming an agent: `agent add/remove/start/
  make-master/set-review/launch`, `as`, `team agent add/remove`, and every
  `--to`/`--author`/`--reply-to` style flag. `agent swap`'s `<OUT> <IN>`,
  plan, team and block names keep theirs.
- Sweep: runtime errors and hints (`agent_store`, `agent`, `doctor`,
  `open_zellij`, `pr_review`, `teams_config`, `rewrite`'s re-run hint), the
  dry-run previews themselves (`finish`, `pick`, `purge` and the squash
  preview's closing line, with `purge`'s renderer assertion),
  argument help (`--squash`'s `--dry-run` pointer, `agent swap`'s
  make-master pointer), the master/shared/reviewer/slash-command skills and
  their pinning tests, and the README (rows and the hidden-spellings note).
  Code comments, alias definitions and compatibility tests keep the old
  words.
- Tests: `renamed_words_parse_to_the_same_request` compares the full parsed
  matches of each old and new spelling; `help_advertises_only_the_new_words`
  renders long help. `team_repo_subcommands_are_removed` no longer claims
  `team remove` is gone.
- `docs/cli-audit.md`: inventory re-blessed, F6 verdicts updated.
