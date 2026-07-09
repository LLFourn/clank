# stop-hook-wrapped-wait-detection

The stop hook uses `BackgroundTask::is_clank_wait` to decide
whether a reported background task is already watching Clank.
The current implementation only accepts `clank wait` when it
is the first command in the reported command string. Agents
often launch the same watcher through a shell command list,
for example:

```sh
cd /path/to/repo; clank wait
cd /path/to/repo && clank wait --author claude
bash -lc 'cd /path/to/repo; clank wait'
```

Those tasks are currently treated as unrelated background
work, so the stop hook can incorrectly nudge the agent to run
`clank wait` even though one is already running.

## Goal

Recognize `clank wait` when it appears as an executable command
segment inside the background task's reported shell command,
while retaining the existing direct-command behavior and
avoiding broad substring false positives.

## Implementation

- Update `BackgroundTask::is_clank_wait` in
  `crates/core/src/hook_io.rs` to inspect command segments, not
  only the first two whitespace-delimited words.
- Recognize command boundaries used by common agent wrappers,
  including the start of input, `;`, `&&`, `||`, and newlines.
- Preserve path-qualified executables such as
  `/Users/me/.cargo/bin/clank wait` and `./clank wait`.
- Handle shell `-lc` wrappers generically rather than using a
  `bash`/`sh` allowlist. Inspect the script payload for command
  segments without executing or fully parsing arbitrary shell
  syntax; this must include `zsh -lc '...'`, the macOS-default
  shell case.
- Within each command segment, skip ordinary POSIX environment
  assignments and command prefixes before checking the executable.
  Cover at least `CLANK_DIR=x clank wait`, `env CLANK_DIR=x clank
  wait`, `exec clank wait`, and `command clank wait`.
- Treat a subshell opener as a command boundary so `(clank wait)`
  is recognized, while retaining the same executable-and-first-arg
  check inside it.
- Keep the helper pure over hook input. Do not inspect the
  process table or spawn a shell.

The detector must not classify mentions as watchers. In
particular, `echo clank wait`, `grep "clank wait" file`,
`clank waitx`, `clankwait`, comments, and ordinary commands
whose arguments contain those words remain negative.

## Tests

- Extend the core command-shape tests with direct and wrapped
  positives, including `cd ...; clank wait`, `cd ... && clank
  wait`, quoted paths, path-qualified binaries, and
  `bash -lc 'cd ...; clank wait'` plus the equivalent `sh` and
  `zsh` wrappers.
- Cover environment-assignment, `env`, `exec`, `command`, and
  subshell forms explicitly so their support is intentional.
- Add explicit false-positive coverage for echoed/searched
  text, near-matching executable or subcommand names, and
  comments.
- Cover the background disposition and stop-hook nudge path so
  a wrapped watcher produces the same armed/yield behavior as
  a direct `clank wait`, and is excluded from unrelated
  background-task counts.
- Run the focused core hook tests and CLI stop-hook tests.

## Out of scope

- A general-purpose shell parser.
- Changing `clank wait` behavior itself.
- Discovering watchers that are absent from the hook's
  background-task input.
