# retire-hidden-aliases
# The old hidden command spellings are gone before the first release

## Why

RELEASE-CHECKLIST.md §1 `retire-hidden-aliases`. Lloyd chose (§0,
2026-11-05) to remove the hidden older spellings before the first
public release: nobody outside this machine uses them. The README's
command reference lists them as "still work, hidden". Each one is
surface area, tests and help text with no users.

## What changes

- Find every hidden alias in the clap tree: `#[command(hide = true)]`
  subcommands and `alias`/`visible_alias` entries kept for back-compat.
  Known ones: `block create`, `unblock`, `open zellij`, `agent promote`,
  `shelve` (alias of `stash`), `status --tui`, `--launch-arg` and
  `--launch-env`, plus whatever else the sweep finds. Remove each, and the
  code that serves only it.
- Hidden commands that are not aliases (for example `monitor upgrade`
  and internal hook entry points) stay. The plan lists each one it kept
  and why.
- Remove the README's list of old spellings, and any skill or book text
  that names them.
- Delete the alias tests. Per memory, there is no test that the removed
  spelling now fails, since clap enforces that. Gates that allow-list an
  old spelling (`it/tui_is_the_command.rs`'s `may_name_status_tui`) drop
  the entries that exist only for the alias. Spellings the installed
  panes still run, such as the pane matcher's two spellings, are judged
  case by case: running sessions' pane commands are durable.
- A full-repo grep for each removed spelling (README, book, skills,
  error strings, tests) finds no stranded references. Intentional ones,
  such as finished plans, stay.

## Acceptance

- `clank --help` and each subcommand's help list no removed spelling.
- The repo-wide sweep is recorded in the As-landed note: what was
  removed, what was kept and why.
- The whole `-p clank --lib` suite and `--test it` pass.

## As landed

ad3e40d0.

**Removed.** Each spelling went together with the code and tests that
served only it:
- `shelve`, `unshelve` and `shelve clean`;
- top-level `unblock`, and `block create`;
- `open zellij`. `cli/open.rs` is deleted, and `OpenZellijArgs` became
  `OpenArgs`;
- `agent promote` and `agent launch`;
- `agent add --global` and `agent remove --global`;
- `--launch-cmd`, `--launch-arg` and `--launch-env`;
- `team delete`, `attending` and `queue reprioritize`;
- `--dry` (`--dry-run` stays);
- `html open --print-path` (now `--print`);
- `config <key> get` and `config <key> set <value>`;
- the ten underscore config-key aliases, such as `review_adhoc_feedback`.
  They were found in the sweep: CLI aliases kept when the dotted names
  became primary;
- `status --tui`, and its "is `clank tui` now" message.

The README's paragraph listing the old spellings is gone.

**Kept, and why:**
- **Hidden commands that are internal entry points, not aliases:**
  - `monitor` and `monitor upgrade`;
  - `rewire`, the post-rewrite hook;
  - `stop-hook` and `question-hook`, which `setup` installs;
  - `licenses`;
  - `block migrate`;
  - `wait --deliver-to-session` and `--turn-end`, and `monitor start
    --installed-at` and `--installed-from`.
- **serde aliases on on-disk records.** They read old files and are not
  CLI spellings.
- **Legacy `.clank/shelved/` and `refs/clank/shelved/` storage.** It is
  durable state.
- **The pane matcher's `clank status --repo <r> --tui`.** Panes opened
  before the rename still run it, so `clank open` still finds them.

`clank setup` installs no old spelling, so no hook migration was
needed.

**Sweep.** I grepped the whole repo for each removed spelling and fixed
the strays, in tests' argv, comments and assertion messages. The `it`
tests that stashed through the shelve alias now go through `stash::run`.
None of the 99 `--help` pages names a removed spelling.

What still mentions them: `.clank/finished/`, which is history, the
storage comments, and the pane matcher. `tui_is_the_command` now allows
only the matcher. The checklist item was reworded so that it does not
spell the old flag.
