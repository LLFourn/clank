# status-watch-nested-ignore

## Problem (proven on a live worktree)

`clank status --tui` pegs a CPU core and the UI freezes on a worktree with
heavy ignored-file churn (a running Flutter sim). Instrumented evidence
(`CLANK_STATUS_DEBUG` spike on `frostsnap/.clank/worktrees/full-app-sim-driver`):
**4141 watcher wakes in ~17s (~250/s); ~99% were `frostsnapp/build/` +
`.dart_tool/` paths; 0 were the git dir.** Each wake drives
`input_signature → working_tree_dirty →` a full gix status walk of that
large worktree — so it does hundreds of full-worktree walks per second,
none of which should happen.

Root cause: `WakeFilter`'s ignore test is NOT git-accurate. `build_matcher`
loads only the repo-ROOT `.gitignore` + `.git/info/exclude`
(`ignore::gitignore`), so paths ignored by a **nested** `.gitignore` (here
`frostsnapp/.gitignore`: `/build/`, `.dart_tool/`) aren't recognized as
ignored. They fall through the filter to "working-tree path → wake," and
storm. `git status` itself never looks at those paths; clank's watcher
does. (`git check-ignore -v` confirms the match is `frostsnapp/.gitignore`,
not root.)

Secondary: even a legitimate wake recomputes the full signature (incl. the
dirty walk) with no coalescing and no rate cap, so any churn becomes a
flood of walks.

## Fix (two changes; dirty + `InputSignature` left exactly as-is)

### 1. Make the wake-filter's ignore test git-accurate (root-cause fix)

Replace the root-only `ignore::gitignore` matcher with gix's exclude
machinery — the same one `git status` uses — which honors nested
`.gitignore`s, `info/exclude`, and `core.excludesFile`.

- **gix feature:** add `"excludes"` to the `gix` features in
  `crates/cli/Cargo.toml`.
- **git_io (boundary):** new `pub struct PathIgnore` owning an exclude
  stack with no self-referential borrow:
  - build via `repo.excludes(&index, None, source)?.detach()` →
    owned `gix_worktree::Stack` (`AttributeStack::detach()` returns it);
    keep a cloned `repo.objects` odb handle (Clone + Send) alongside.
    `index` loaded once (`repo.index_or_empty()`); a stale index is fine
    for ignore matching. `source =
    gix_worktree::stack::state::ignore::Source::WorktreeThenIdMappingsIfNotSkipped`
    (read `.gitignore` from the worktree, like status).
  - `pub fn is_ignored(&mut self, rela: &Path, is_dir: bool) -> bool` →
    `self.stack.at_path(rela, is_dir.into(), &self.objects)?.is_excluded()`
    (`&mut self`: the stack caches per-directory ignore state, so the
    storm's same-dir paths are cheap after the first).
  - `pub fn open_ignore(repo: &Path) -> Result<PathIgnore, GitIoError>`.
  - All gix names stay in `git_io`; the boundary test still passes.
- **WakeFilter (status.rs):** hold a `git_io::PathIgnore` instead of the
  `ignore::gitignore::Gitignore`. For a working-tree path, wake unless
  `is_ignored(rela, is_dir)`. `rela` = path stripped of `repo_root`. The
  existing special cases are unchanged: git dir → wake; `.clank`
  allowlist (`CLANK_WAKE_DIRS`) → wake; a `.gitignore` write → rebuild the
  `PathIgnore` + wake. (The `ignore` crate may then be unused here; only
  drop the dependency if nothing else uses it — otherwise leave it.)

Net: `frostsnapp/build/` churn is recognized as ignored and never wakes
the loop — the ~99% of wakes vanish before any walk.

### 2. Debounce + coalesce the status loop (defense-in-depth)

In `run_tui`, after a wake: drain all pending events (`try_recv` loop) so a
burst collapses to one, and rebuild at most ~once/second (trailing-edge
throttle keyed on an `Instant` of the last rebuild; a deferred wake fires
on the next loop tick once the interval elapses). Keep it event-driven —
this only caps the rate, so no future expensive input (or non-ignored
churn) can peg a core. Keys/resize stay responsive (they're not throttled).

`InputSignature` (head + **dirty** + clank) and the dirty display are
UNCHANGED — with wakes filtered and rate-capped, the dirty walk inside the
signature runs rarely and is cheap in aggregate.

The `CLANK_STATUS_DEBUG` spike is reverted (already done).

## Acceptance

- `WakeFilter` test with a fixture repo containing a **nested** `.gitignore`:
  a path ignored only by the nested file does NOT wake; a tracked-file edit
  DOES; a `.clank/<wake-dir>` path DOES; a git-dir path DOES; writing a
  `.gitignore` rebuilds the checker. (Fixtures may spawn `git`/open gix.)
- Debounce/coalesce: a pure helper (e.g. `should_rebuild(last, now,
  pending)`/min-interval) unit-tested; a burst of N wakes yields ≤1 rebuild
  per interval.
- `cargo test` green; fmt + clippy clean; `git_boundary` still passes
  (gix only in `git_io`). Build + `cargo install`.
- Manual: re-run the `CLANK_STATUS_DEBUG`-style check on the sim worktree
  (or eyeball CPU) → the status TUI sits near 0% while the sim churns.

## Out of scope

- Removing `dirty` from the signature / changing the dirty display.
- Not watching ignored directories at all (notify-level pruning) — the
  ignore-accurate filter already drops their events cheaply.
- Dropping the `ignore` crate dependency unless it's now unused.
