# queue-add-consumes-stub

`clank queue add <name>` should CONSUME the stub it reads: once the
queue entry is durably written, delete `.clank/stubs/<name>.md`. Today
the stub is read as a body source and left behind, so stubs accumulate
forever (26 in this repo, 21 of them for already-finished plans).

## Problem

The stub → queue → plan → finished lifecycle never deletes the stub:
`pick_body_source` READS `.clank/stubs/<name>.md` and `add` COPIES it
into `.clank/queue/<NNN>-<name>.md` (`crates/cli/src/cli/queue.rs`) but
never removes the source; `promote`/`finish` move the queue/plan files,
never the stub. So the staging file lingers indefinitely. The
`stubs-gitignored` plan deliberately deferred this cleanup — "revisit
as a follow-up only if the accumulation bothers." It bothers.

## Change

In `clank queue add`'s `add`, after the queue entry is written, delete
the stub IFF the body came from the stubs dir:

- `pick_body_source` already tags the source as
  `BodySourceKind::StubsDir(path)`. Capture that path BEFORE the body
  is consumed by `normalize_and_validate`.
- Delete the stub only AFTER `std::fs::write(dest, body)` succeeds:
  ordering matters — a failed queue write must leave the stub intact
  for a retry (never destroy the source before the destination exists).
- ONLY for `StubsDir`. `--from <path>` / `-m` / stdin are NOT consumed:
  `--from` points at an arbitrary user-owned file and deleting it would
  be surprising and destructive.
- Best-effort: a failed unlink warns (the queue add already succeeded);
  it does not fail the command.

Net: write `.clank/stubs/<name>.md` → `clank queue add <name>` → stub
gone, entry queued. The staging dir self-empties.

## Testing (in-process; no binary spawning — [[no-binary-spawning-tests]])

- Stub-sourced add consumes: with `.clank/stubs/foo.md` present and no
  `--from`/`-m`, `queue add foo` writes `queue/<nnn>-foo.md` AND the
  stub is gone afterward.
- Non-stub sources untouched: `add` with an inline / `--from <file>`
  body does NOT delete that file (and the existing inline `add` tests
  still pass — inline has no file to delete).

## Non-goals

- One-off cleanup of the existing stale stubs (a manual `rm`, decided
  separately) — this change only prevents FUTURE accumulation.
- No change to `add`'s source precedence or the `/stubs/` gitignore
  entry — stubs is still the staging area between write and consume.
