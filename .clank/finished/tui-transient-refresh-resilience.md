# tui-transient-refresh-resilience
# TUI survives transient snapshot errors; gix errors stop dressing as git

## Why

`clank status --tui` crashed with `git git status item: exit None: IO
error while writing blob or reading file metadata or changing
filetype`. Two defects in one report:

- **The crash**: the TUI refresh loop rebuilds via
  `StatusSnapshot::build_async(...).await?` (status_tui/mod.rs
  ~1908) — ANY transient error kills the whole TUI. The trigger here
  was gix's status iterator hitting a mid-walk IO race (a file
  deleted/changed between the dirwalk's stat and read — agents and
  builds mutate the tree constantly while the TUI watches). A
  read-only renderer must degrade, not die: a refresh that fails
  keeps the last frame and tries again on the next event.
- **The message**: `GitIoError::NonZero`'s Display renders
  `git <context>: exit <code>: <stderr>` — a SUBPROCESS shape.
  gix-sourced errors are wrapped through the same variant with
  `code: None` (the `nonzero(...)` helper), producing "git git
  status item: exit None: …" — it reads as a git subprocess dying
  when no subprocess exists, which sent the operator down the wrong
  path ("I thought we used gix for reads").

## What

- **Refresh resilience**: in the TUI event loop (and `status
  --watch`'s equivalent refresh, if it shares the `?`), a failed
  snapshot rebuild KEEPS the previous snapshot, surfaces a transient
  one-line notice (the existing notice channel — same surface as the
  timeline notices), and retries on the next watcher event / tick.
  Repeated consecutive failures (say 3+) may escalate the notice but
  still never exit; a hard exit remains only for the INITIAL build
  (there is no previous frame to keep) and terminal teardown errors.
  **The accepted input signature advances ONLY on successful
  snapshot replacement** (intro 1913796): the loop is
  signature-gated, so a failure that advanced the signature would
  strand — the next wake observes "unchanged" and never retries. A
  failed rebuild leaves the signature untouched, keeping the same
  state retryable on the next event.
- **Honest error shape, provenance at CONSTRUCTION** (intro
  1913796): `code: None` is NOT a usable discriminator — the
  `nonzero(...)` helper also serves the real subprocess boundary
  (`read_git_stdout` spawn/nonzero-exit failures), and ~22 gix call
  sites construct `GitIoError::NonZero` directly with `code: None`.
  Instead: a gix-specific variant (or constructor) carries
  provenance explicitly; EVERY gix-backed error path migrates to it
  — the direct `NonZero` constructors, the `nonzero(...)` uses at
  gix sites, and both `walk_err` closures — rendering
  `gix <context>: <error>` (no "git", no "exit"). `read_git_stdout`
  keeps a distinct subprocess constructor so its display contract
  stays intentional. The migration is a sweep with a checklist, not
  a Display-side inference.

## Acceptance

- A snapshot rebuild error mid-session leaves the TUI running on the
  last frame with a visible transient notice; the next successful
  rebuild clears it. Pinned with a unit test at whatever seam the
  loop exposes (e.g. a rebuild-result → action classifier), not by
  driving a real terminal.
- The initial build still fails loud (no frame to fall back on).
- A gix-sourced GitIoError Displays as `gix …` with no `exit`;
  subprocess errors are byte-stable (including `read_git_stdout`
  spawn failures, which share `code: None` and must NOT be
  relabeled). Existing tests that pin error text are updated
  deliberately, not loosened.
- A rebuild failure followed by a wake with the SAME signature
  retries (the signature-advance-on-success-only contract, pinned
  at the loop's decision seam).
- In-process tests only.
