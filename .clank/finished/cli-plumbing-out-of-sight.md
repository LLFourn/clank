# cli-plumbing-out-of-sight
# Plumbing leaves clank --help

Fixes F1 of `docs/cli-audit.md`.

## Problem

Hook adapters (`stop-hook`, `rewire`), the monitor daemon (`monitor start`,
spawned by clank), migrations (`monitor upgrade`, `block migrate`) and an
inspector with no internal caller (`open dry`) share `clank --help` with
daily commands.

## Change

- Hide `stop-hook`, `rewire` and `monitor` (`#[command(hide = true)]`). They
  keep working; the hooks that run them are unchanged.
- `block migrate` and `monitor upgrade`: establish whether the old shape can
  still exist on another machine (it can unless proven otherwise; a check of
  this machine proves nothing). Prefer making each migration happen where the
  old shape is first read; otherwise hide the command.
- `open dry`: search for external callers (scripts, editor integrations, the
  opencode plugin). If none, delete it; otherwise hide it.

## Out of scope

The hidden `shelve`/`unshelve` aliases. Their source comment promises one
release of back-compat, but clank has no release boundary (crate `0.0.1`, no
tags), so their expiry is undecided. They are already hidden and cost nothing
in `--help`. They stay until someone sets a compatibility boundary.

## Acceptance

- `clank --help` no longer lists the hidden commands; parsing tests show each
  still parses.
- If `open dry` is deleted, no test asserts it is gone (the framework
  enforces absence).
- `docs/cli-audit.md` verdicts updated and the inventory re-blessed.

## As built

- Hidden, unchanged in behaviour: `stop-hook`, `rewire`, `monitor` (with
  `start` and `upgrade`), `block migrate` and `open dry`. Dispatch and every
  installed hook string are untouched.
- `monitor upgrade` and `block migrate` are kept as hidden commands rather
  than turned into automatic migrations: their old shapes can exist on
  other machines, and making a read path migrate would turn status and
  discovery into writes.
- `open dry` is kept: `open-worktree.sh` calls `clank open dry --json` and
  reads its roster fields, so the audit's "no caller" was wrong.
- `SECTIONS` loses its `Plumbing` section; the section coverage test still
  requires every visible command exactly once.
- README: the hidden commands leave the reference table for a short note
  saying what each is for and who runs it.
- Tests: `hidden_plumbing_still_parses` (each real caller's argv) and
  `hidden_plumbing_is_not_listed` (root and group help). Unhiding a
  subcommand fails the latter; unhiding a top-level command fails the
  section and README coverage tests.
- `docs/cli-audit.md`: inventory re-blessed, F1 verdicts updated.
