# status-watch

Add a persistent status watch surface for editor integrations.

## Goal

`clank status --watch` should print the current status immediately,
then keep running and print status again whenever the effective Clank
status changes. It never exits on its own.

With `-j`, watch mode should emit newline-delimited JSON: exactly one
compact JSON object per update, flushed after each line. This is the
editor-facing contract so a modeline or subprocess reader can consume
updates without parsing pretty-printed multi-line JSON.

## Shape

- Prefer `clank status --watch` over a separate command. It keeps the
  existing status projection and flags in one place.
- Preserve existing one-shot behavior when `--watch` is absent.
- Initial output happens before any watcher is installed.
- Subsequent output happens only when the rendered status payload
  changes, so duplicate filesystem notifications do not spam readers.
- Human watch output may print repeated normal status blocks separated
  by a blank line. Do not clear the terminal.
- JSON watch output must be one compact JSON line per update, not the
  existing pretty JSON format.
- Flush stdout after every update.

## Watch Sources

Reuse the same state sources that can affect `clank status`:

- git HEAD / refs / worktree state relevant to rebuild
- `.clank/plans/`
- `.clank/finished/`
- `.clank/agents/*/feedback/`
- `.clank/queue/`
- `.clank/config.json` and relevant agent config if status output uses it

The watcher should be level-triggered: on any event, refold/recompute
status, compare to the last emitted payload, and print if changed.

## Tests

- `clank status --watch` in a temp repo prints an initial status and
  stays alive until the test kills it.
- A plan/feedback/finalize or queue change causes a second status block
  without restarting the process.
- Duplicate notifications without status changes do not produce extra
  output.
- `clank status --watch -j` emits compact one-line JSON per update.
- Existing `clank status` and `clank status -j` one-shot output remain
  unchanged.
