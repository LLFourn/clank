# queue-add-requires-body

`clank queue add <name>` currently creates an empty stub at
`.clank/queue/<NNN>-<name>.md` containing just a `# <name>`
header. Agents (and humans) then forget to fill it in before
the stop hook surfaces it as a promote item, and the queue
gets useless title-only stubs. We just hit this — a one-line
stub queued itself and went straight to the master's "ready
to promote" prompt.

Make `clank queue add` require an actual body. Three sources,
checked in this order:

1. `--from <path>` — explicit file. Read the file's content
   as the stub body. Errors if the path doesn't exist or
   isn't readable.
2. `-m "<body>"` / `--message` — inline body. The string is
   used verbatim. If a `# <name>` header isn't present,
   prepend one for consistency.
3. Repo-local `.clank/stubs/<name>.md` — convenience for
   "I drafted this in my editor earlier." If the file exists
   and the agent passes neither `--from` nor `-m`, copy it
   into the queue.

If none of the three apply, fail with a clear error that
lists all three options. Empty stubs are NEVER created.

## Behavior details

- `-m` accepts multi-line bodies the same way `git commit -m`
  does (newlines preserved).
- `--from` and `-m` are mutually exclusive (clap conflict).
- `--from -` reads stdin. Useful for piping.
- After the queue file is written, the on-disk path is
  echoed (same as the current command).
- Duplicate-name detection (`scan_queue_no_dups`) still
  fires before any of the body sources are read — no IO
  wasted on conflicts.

## `.clank/stubs/`

- New repo-relative directory for stub drafts.
- NOT created by `clank init`. It's purely a "draft your stub
  here, then queue when ready" affordance.
- Not gitignored — if you want to track stub drafts, you can.
  If you don't, add it to your own gitignore. Clank stays
  neutral.

## Surfaces touched

- `crates/cli/src/cli/queue.rs::add` — accept a `body`
  enum (`Inline(String)` | `FromPath(PathBuf)` |
  `FromStubsDir`). Replace the empty-file write with
  content from the chosen source.
- `crates/cli/src/cli/mod.rs::QueueAddArgs` — add
  `#[arg(long, short = 'm')] pub message: Option<String>`
  and `#[arg(long, value_name = "PATH")] pub from:
  Option<PathBuf>`. Wire clap `conflicts_with` so `-m` /
  `--from` are exclusive.
- Doc comment on `QueueCmd::Add` updates: mention the three
  body sources.

## Tests

- `queue_add_inline_message_writes_body` — pass `-m "# foo\n
  body"`; assert file content matches.
- `queue_add_from_path_writes_body` — point `--from` at a
  prepared markdown file; assert file content matches.
- `queue_add_from_stubs_dir_when_present` — write
  `.clank/stubs/foo.md`, run `clank queue add foo`; assert
  the queue file has the stubs body.
- `queue_add_fails_with_no_body_source` — neither flag, no
  stub on disk; assert error mentioning all three options;
  assert no queue file was created.
- `queue_add_from_stdin_dash` — `--from -` reading from
  stdin; assert body content lands.
- `queue_add_m_and_from_mutually_exclusive` — both flags
  → clap parse error.

## Out of scope

- A `clank queue edit <name>` command to open the stub in
  `$EDITOR` after queueing. Useful, separate plan.
- Templating (`--template plan`, etc.). Not needed for v1.
- Migrating any existing empty stubs in queues out in the
  wild. Each operator can `rm` and re-queue with content.
