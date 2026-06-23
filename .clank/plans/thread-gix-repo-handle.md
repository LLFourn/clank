# thread-gix-repo-handle

Make the opaque `git_io::Repo` handle (introduced by `gix-not-git-gate`) the
PRIMARY read API, opened ONCE per top-level operation and threaded down —
instead of `git_io`'s path-based read functions each calling `gix::open`
internally, which re-reads pack indexes on every call.

## Why

`git_io` reads are path-based (`rev_parse_head(repo: &Path)`,
`commit_subject(repo, sha)`, `commit_events_between(...)`,
`first_parent_commits(...)`, `diff_tree_changes(...)`, …). Each opens its own
gix handle. A single fold / `status` build / `log` render / preview opens the
SAME repo dozens of times — the constant-re-open cost flagged during the
status-tui CPU investigation. `status-dirty-stats-via-gix` fixed this only
WITHIN a snapshot (one handle threaded through the dirty walk + per-plan
worktree status via `git_io::Repo`); the rest still re-opens.

## Fix

- Make `git_io::Repo` the idiomatic handle. Convert the hot path-based read
  functions to take `&Repo` (or a method on it), so callers open once and pass
  it through.
- Thread ONE handle from each top-level entry point down: the fold/rebuild
  (`commit_events_between`, `first_parent_commits`, checkpoint walk), `status`
  build, `log` render, preview, html. Open at the top, pass `&Repo`.
- Keep thin `*_at(&Path)` conveniences ONLY for genuine one-shot CLI commands
  (e.g. a single `working_tree_dirty_at` check), not for anything called in a
  loop or a fold.
- gix's object cache is per-handle, so one threaded handle also gets warm
  object caching across reads — consider `object_cache_size_if_unset`.

## Risks / notes

- **`gix::Repository` Send/Sync across `await`.** The fold
  (`rebuild_repo_with_policy`) is async. Holding `&Repo` across an `.await` in
  a Send future needs `Repository: Sync`, which isn't guaranteed. Thread
  `&Repo` through the SYNC fold helpers (open just inside the sync section, or
  pass it down sync call chains); don't hold it across awaits. Verify it
  compiles through the async entry points.
- **Don't break the boundary.** `git_io::Repo` lives in `git_io`; threading it
  keeps `git_boundary.rs` green (callers name `git_io::Repo`, never `gix::`).
- **Test "opens once" concretely.** Add a test-only counter incremented in
  `git_io::open`; a fold/status build over a fixture asserts it's called once
  (or a small bounded N), not per read. Plus existing fold/status/log tests
  stay green for behavior.

## Out of scope

- The write layer (`git_plumbing`) — its mutations are one-shot; separate.
- Converting more subprocess ops to gix (separate, as encountered).

## Testing (no-binary-spawning)

- A fold / status build over a fixture opens the ODB once, not per read
  (assert via a counting wrapper or by construction — one `Repo` created).
- Read results are unchanged (existing fold/status/log tests stay green).

## Acceptance

- `git_io`'s hot reads are handle-first (`&Repo`); top-level ops open once and
  thread it.
- No behavior change; the re-open-per-call pattern is gone from the fold and
  status/log/preview paths.
