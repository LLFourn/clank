# wfw-daemon-style-watches

## Summary

`clank wfw` returns to the old Trinity daemon's coarse watching
strategy: one recursive watch on `<repo>/.clank` plus one
non-recursive watch on the resolved worktree gitdir.

Today wfw uses fine-grained watch roots — `git_dir/HEAD`,
`git_dir/logs/HEAD`, `git_common_dir/refs` recursive,
`git_common_dir/packed-refs`, and three separate `.clank/{plans,
feedback,finished}` recursive watches. More precise on paper;
not robust in practice.

The blocking case is agents running `clank wfw` inside Codex's
sandbox. The script-level repro at
`/private/tmp/clank-watch-repro/repro-git-commit-timeout.sh`
shows:

- Outside the Codex sandbox: every fine-grained watch wake fires
  on `git commit -m` within ~230 ms. 25+ trials clean here.
- Inside the Codex sandbox: the same binary, same script, hits
  `rc=2` / `wfw timed out` — the fine-grained file events never
  reach `notify`. The 1.5 s heartbeat does eventually catch up,
  but the immediate FS-event wake path is broken.
- After flipping to daemon-style directory watches inside the
  Codex sandbox: five consecutive wakes returned in ~230 ms with
  no heartbeat involvement.

So the bug is environmental — Codex's sandbox filters out or
drops the kernel-level events for individual Git files (or for
fine-grained file paths inside the gitdir generally) but lets
directory-level watches through. The architectural answer is to
stop relying on individual-file precision: watch coarsely so any
Git metadata movement wakes us.

## Hard Direction

- **Watches are coarse and structural, not file-specific.** Two
  roots: `<repo>/.clank` recursive and the resolved worktree
  gitdir non-recursive. No individual-file watches. No second-
  guessing which Git file fires for which operation.
- **No new polling.** The existing 1.5 s heartbeat from
  `wfw-finish-notification` stays — it's a finalize-race safety
  net, not the primary wake mechanism — but this plan does NOT
  add additional polling, sleeps, or retry loops. If watches
  work, the heartbeat just doesn't fire on hot paths.
- **Refold is the only invalidation.** Every wake (FS event or
  heartbeat tick) refolds. The watcher tells wfw "something
  changed under one of these roots"; refold figures out what.
  We don't try to classify events.
- **Linked worktrees still work.** A linked worktree's resolved
  gitdir (the per-worktree path under
  `<main>/.git/worktrees/<name>/`) gets its own `index`,
  `HEAD`, `ORIG_HEAD`, etc. on local commits. Non-recursive
  watch there catches the `index` write — which is enough,
  because the refold reads HEAD via `git rev-parse` against the
  worktree.

## Current vs Old Watch Shape

Current `clank wfw` watches:

```text
<resolved worktree gitdir>/HEAD          non-recursive
<resolved worktree gitdir>/logs/HEAD     non-recursive, if present
<git common dir>/refs                    recursive
<git common dir>/packed-refs             non-recursive, if present
<repo>/.clank/plans                      recursive
<repo>/.clank/feedback                   recursive
<repo>/.clank/finished                   recursive
```

Old Trinity daemon (the proven-to-work shape):

```text
<repo>/.trinity                          recursive
<resolved worktree gitdir>               non-recursive
```

This plan adopts the old shape adjusted for Clank's directory
name (`.clank` instead of `.trinity`):

```text
<repo>/.clank                            recursive
<resolved worktree gitdir>               non-recursive
```

## What Each Root Catches

`<repo>/.clank` recursive catches every write Clank itself
makes or sees: plan files, feedback, finished snapshots, and
any subdirectory created later. Replaces three separate watches
with one — the directory layout is no longer load-bearing on
the watcher.

`<resolved worktree gitdir>` non-recursive catches:

- `HEAD` rewrites (checkout, symbolic-ref).
- `index` updates — Git rewrites the index every `git commit`,
  `git add`, etc. This is the canonical "something Git is doing
  in THIS worktree" signal.
- `packed-refs`, `ORIG_HEAD`, `MERGE_HEAD`, `FETCH_HEAD`,
  `COMMIT_EDITMSG`, etc. — top-level files Git touches during
  ordinary operations.

Non-recursive deliberately does NOT walk into `objects/`,
`refs/`, or `logs/`. We don't need to — the `index` write fires
for every local commit and that's a sufficient invalidation
signal. Cross-worktree ref pushes (where a separate process
updates `<common>/refs/heads/<branch>` without touching this
worktree's gitdir) are explicitly out of scope; the heartbeat
catches them as a slow fallback if they're ever relevant.

## Implementation

`crates/cli/src/cli/wfw.rs::WatchContext::attach`:

1. `std::fs::create_dir_all(<repo>/.clank)` (replaces the
   three-subdir create_dir_all loop).
2. Watch `<repo>/.clank` recursively.
3. Watch the resolved worktree gitdir (from `git rev-parse
   --git-dir`) non-recursively.

Drop:

- The per-file `HEAD` / `logs/HEAD` watches.
- The separate `refs` recursive watch on the common gitdir.
- The `packed-refs` watch.
- The three `.clank/{plans,feedback,finished}` watches.
- The `git_common_dir` resolution — if a future need for
  shared-ref wakes appears, add a broad watch root for the
  common directory, not individual files.

Keep:

- `WatchContext::resolve` for the worktree gitdir path (drop
  the `git_common_dir` field).
- The `try_watch` helper (now with only two callers).
- The 1.5 s heartbeat in the `recv_timeout` loop.

## Tests

### Rust integration tests (`crates/cli/tests/wfw_integration.rs`)

The existing 12 tests all exercise the watcher and must
continue to pass under the new shape:

- Reviewer wake on new reviewable commit
- Reviewer wake on code-only commit (the bare git-ref signal)
- Reviewer wake inside a linked worktree
- Master wake on REQUEST_CHANGES
- Reviewer finish wake (human + JSON)
- `--plan` filter finish wake
- Mixed work + finished on one wake
- `--plan` against already-finished
- Plan-only approval → Implement
- Code-only approval → Finalize
- Early-snapshot + late-commit finalize race
- Master/code-only wake routing

`wfw_reviewer_wakes_on_code_only_commit` is the load-bearing
one for this change: it asserts the wake fires on a commit that
touches only `src/lib.rs`, so the ONLY trigger is gitdir
activity (the `index` write specifically). If non-recursive
watching the worktree gitdir doesn't catch that, the test fails.

### Script-level Codex-sandbox repro

`/private/tmp/clank-watch-repro/repro-git-commit-timeout.sh`
stays as the canonical out-of-tree repro. We don't try to
reproduce the sandbox behavior from a Rust unit test — the
behavior only appears under Codex's tool sandbox. The Rust
suite covers the deterministic invariants; the shell repro is
the empirical safety net.

Acceptance for the repro:

- Outside the sandbox: passes (it already does today; this
  must not regress).
- Inside the sandbox: passes consistently. ~230 ms latency,
  no heartbeat fallbacks.

## Acceptance

- `clank wfw` watches exactly two paths: `<repo>/.clank`
  recursive and the resolved worktree gitdir non-recursive.
  `grep -n 'watcher.watch'` in `crates/cli/src/cli/wfw.rs`
  shows two call sites.
- No watch is configured for `HEAD`, `logs/HEAD`,
  `packed-refs`, the common-gitdir `refs` tree, or any
  individual file inside the gitdir.
- The 12 existing wfw integration tests all pass after the
  switch (including linked-worktree and code-only-commit).
- The Codex-sandbox repro passes after rebuilding.
- The existing 1.5 s heartbeat from `wfw-finish-notification`
  is untouched. No new polling, no new fallbacks added in this
  plan.
- No regression to the broader test sweep:
  `cargo test --workspace` green; `cargo fmt --check` clean.

## Out of Scope

- **Removing the heartbeat.** The 1.5 s heartbeat was added as
  a finalize-race safety net; broad watches likely make it
  unnecessary on the happy path, but removing it is a separate
  decision and a separate code review.
- **Cross-worktree ref pushes.** A different process updating
  shared refs without touching the current worktree's gitdir
  is not in scope. If a real use case for this surfaces,
  expand the watch roots in a follow-on plan.
- **Switching to a different watcher backend.** `notify`'s
  `RecommendedWatcher` (FSEvents on macOS, inotify on Linux)
  stays.
- **Sandbox-tolerant alternatives** (pure polling, named-pipe
  IPC, etc.). The proposed design fits Codex's sandbox; we
  don't pre-design for fictional future sandboxes.

## Open Questions

None.
