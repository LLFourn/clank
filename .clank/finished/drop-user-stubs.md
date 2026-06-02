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
file required, no copy step.

`--priority` keeps the existing `0..=999` range and `500`
default.

## Duplicate / ambiguous name handling

Queue items are keyed by `<name>` for `scan` / `remove` /
`promote`. Two files with the same name and different priority
prefixes (e.g. `400-foo.md` and `410-foo.md`) make every keyed
operation ambiguous.

Don't try to deduplicate or auto-resolve. Instead:

- `clank queue add <name>` fails if ANY existing
  `.clank/queue/<NNN>-<name>.md` matches the name, regardless
  of the priority prefix.
- `scan` / `remove` / `promote` fail loudly when they
  encounter multiple files for the same name.
- Every failure message tells the agent the exact fix:

  ```
  ambiguous queue name `<name>`: matched 400-<name>.md and
  410-<name>.md. Delete the duplicate from .clank/queue/ and
  re-run.
  ```

  That puts the cleanup squarely on the agent. No silent
  recovery.

## Surfaces touched

- `crates/cli/src/cli/queue.rs::add` — drop the
  `~/.clank/stubs/<name>.md` lookup; write the new file
  directly under `.clank/queue/`. Refuse if ANY existing
  `.clank/queue/<NNN>-<name>.md` matches the name (any
  priority prefix).
- `crates/cli/src/cli/queue.rs::scan_queue` /
  `::remove` / `::promote` — when more than one file matches
  a name, bail with the ambiguity message documented above
  instead of silently picking one.
- `crates/cli/src/cli/mod.rs` `QueueCmd::Add` doc comment
  (currently "Add a stub to the queue.") — rewrite to reflect
  the new behavior, e.g. "Add an empty queued plan stub at
  `.clank/queue/<NNN>-<name>.md`." No mention of
  `~/.clank/stubs/`.
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
- `queue_add_refuses_when_same_name_any_priority` — pre-create
  `.clank/queue/400-foo.md`, then run `clank queue add foo
  --priority 410`; assert it fails with a message naming the
  existing file. Pin that the destination at
  `.clank/queue/410-foo.md` is NOT created.
- `queue_promote_fails_on_ambiguous_name` — seed two files
  for the same name at different priorities, run `clank
  queue promote foo`, assert failure + the documented
  "delete the duplicate" message.
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
