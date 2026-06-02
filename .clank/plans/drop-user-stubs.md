# drop-user-stubs

`clank queue add <name>` currently requires a pre-existing stub
at `~/.clank/stubs/<name>.md` and errors with that path when
missing. Agents trying to queue work see the error and dutifully
create the file there — `~/.clank/stubs/` ends up accumulating
half-formed plans from every repo's agents. Queue items are
meant to be repo-local; the user-scope stub indirection was an
accident.

Purge it.

## Behavior change

`clank queue add <name> [--priority N]` now writes a fresh
empty stub directly to `.clank/queue/<NNN>-<name>.md` with a
single-line header:

```md
# <name>
```

The agent (or human) then edits that file in place. No external
file required, no copy step. Re-running `clank queue add` for
the same name is an error ("queue item already exists at …").

`--priority` keeps the existing `0..=999` range and `500`
default.

## Surfaces touched

- `crates/cli/src/cli/queue.rs::add` — drop the
  `~/.clank/stubs/<name>.md` lookup; write the new file
  directly under `.clank/queue/`. Refuse if the destination
  already exists.
- `crates/cli/src/cli/mod.rs:629` — strip the
  "`.clank/stubs/*`" mention from the `QueueAddArgs` doc
  comment.
- Any other comment / doc / test referring to
  `~/.clank/stubs/` or `.clank/stubs/` — grep and remove.
  (The skill assets I already checked don't mention it; only
  finished plans in `.clank/finished/` do, and those are
  historical — leave them.)

## Tests

- `queue_add_creates_file_with_header_and_no_stubs_dependency`
  — fresh repo, run `clank queue add foo --priority 400`,
  assert `.clank/queue/400-foo.md` exists with
  `# foo\n` body, no read from `$HOME/.clank/stubs/`.
- `queue_add_refuses_when_destination_exists` — pre-create
  `.clank/queue/400-foo.md`, run the command, assert error.
- Sanity: any existing queue test that pre-seeded a
  `~/.clank/stubs/<name>.md` file gets rewritten to drop that
  setup step.

## Out of scope

- Cleaning up the user's existing `~/.clank/stubs/` directory.
  Those are their files (mostly emacs-config notes, not
  clank's business). The user can `rm -rf` themselves if
  they want.
- Migrating any in-flight queue items. Repo-local queue
  contents are unaffected.
- Cross-repo idea pool. If we ever want one, it's a separate
  feature with explicit semantics, not an accidental
  side-effect of the queue command.
