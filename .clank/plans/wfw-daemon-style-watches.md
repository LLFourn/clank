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

`<resolved worktree gitdir>` non-recursive catches the top-level
files Git rewrites during ordinary operations:

- `index` — rewritten by both `git add` AND by `git commit -m`
  itself. Empirically verified: a `git commit -m` against an
  already-staged change grows `.git/index` from 174 → 193 bytes
  on a minimal repo. The post-commit index write is the
  canonical "this worktree just produced a commit" signal.
- `HEAD` — rewritten on checkout / symbolic-ref.
- `packed-refs`, `ORIG_HEAD`, `MERGE_HEAD`, `FETCH_HEAD`,
  `COMMIT_EDITMSG`, etc.

Non-recursive deliberately does NOT walk into `objects/`,
`refs/`, or `logs/`. We don't need to — the `index` write fires
for every local commit and that's a sufficient invalidation
signal. The proof obligation for this claim lives in the
Tests section: a staged-before-wfw regression test isolates
the commit-boundary write from any pre-commit `git add` noise.

Cross-worktree ref pushes (where a separate process updates
`<common>/refs/heads/<branch>` without touching this worktree's
gitdir) are explicitly out of scope; the heartbeat catches them
as a slow fallback if they're ever relevant.

## Implementation

`crates/cli/src/cli/wfw.rs::WatchContext::attach`:

1. `std::fs::create_dir_all(<repo>/.clank)` (replaces the
   three-subdir create_dir_all loop).
2. Watch `<repo>/.clank` recursively.
3. Watch the resolved worktree gitdir (from `git rev-parse
   --git-dir`) non-recursively. If the staged-before-wfw test
   from the Tests section fails under non-recursive — i.e.
   notify on this platform doesn't deliver the commit-time
   `.git/index` rewrite — escalate to recursive. The
   non-recursive default mirrors the old Trinity daemon's
   proven shape; the escalation is a documented fallback, not
   the first choice.

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

`wfw_reviewer_wakes_on_code_only_commit` is one part of the
proof, but on its own it isn't enough: that test runs
`git add` AFTER wfw has parked. The `git add` itself rewrites
`.git/index`, which fires under the proposed non-recursive
gitdir watch. The subsequent `git commit -m` lands inside the
200 ms debounce window, so the test passes even if the
commit-time index write never fires. That's a false positive
for the invariant we actually care about.

**New required test: `wfw_reviewer_wakes_on_commit_with_index_already_staged`.**
The shape isolates the commit-boundary write from any
preceding `git add` noise:

1. Create the repo, the plan, and alice's prior approval.
2. Write the new code change AND `git add` it.
3. Start `clank wfw` and wait for watcher attach (1.5 s).
4. Run `git commit -m '[foo] code work'` and nothing else.
5. Assert wfw exits 0 with a `Reviewer` item on the new SHA,
   within a reasonable bound (say 10 s).

If non-recursive `.git/` doesn't catch the commit-time index
rewrite, this test fails — and the implementation must broaden
the gitdir watch root (e.g. recursive on `.git/`) rather than
fall back to individual-file watches. Re-introducing
fine-grained file watches is rejected: the plan's whole point
is that they're unreliable under Codex's sandbox.

### Script-level Codex-sandbox repro

`/private/tmp/clank-watch-repro/repro-git-commit-timeout.sh`
stays as one canonical out-of-tree repro. The behavior under
Codex's sandbox only appears under that tool sandbox; the Rust
suite covers the deterministic invariants and the shell repros
are the empirical safety nets.

Add a second shell repro that mirrors the new Rust isolation
test (stage-then-park-then-commit-only) so we can verify the
commit-boundary wake under the sandbox too. Both repros need
to pass after the switch.

Acceptance for the repros:

- Outside the sandbox: both pass (this must not regress).
- Inside the sandbox: both pass consistently with sub-second
  latency. No heartbeat fallbacks on the hot path.

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
- The new `wfw_reviewer_wakes_on_commit_with_index_already_staged`
  test passes. This is the load-bearing proof that
  `git commit -m` alone — with no preceding `git add` event —
  wakes wfw. If it fails on non-recursive `.git/`, the
  implementation broadens to recursive (NOT individual-file).
- Both shell repros (the existing one plus the new staged-
  before-wfw variant) pass after rebuilding, inside and
  outside the Codex sandbox.
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
