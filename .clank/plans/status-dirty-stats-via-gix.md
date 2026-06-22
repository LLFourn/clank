# status-dirty-stats-via-gix

Orthogonal to the incremental rearchitecture (`status-incremental-snapshot`):
regardless of WHEN the working-tree dirty facts are computed, they should be
computed with gix, not a `git` subprocess. The status-tui CPU investigation
found the live-overlay shells out:
- `dirty_stats` → `git_nol` → `git status --porcelain` + `git diff HEAD
  --shortstat` (the single biggest CPU when it ran),
- `worktree_status` → a `git` spawn per plan (`Command::status`),
- `head_commit`/`commit_events_between` → a fresh `gix::open` each call.

This is exactly the git→gix anti-pattern prior plans tried to purge.

## Fix

1. Compute working-tree dirty stats (insertions / deletions / untracked) and
   `worktree_status` via in-process **gix** — no `git` subprocess.
   - insertions/deletions today = `git diff HEAD --shortstat` (staged +
     unstaged, LINE counts). gix equivalent: enumerate paths differing from
     HEAD's tree (worktree status), then for each, run a blob LINE diff
     (`blob-diff` is already enabled) and sum +/−. This per-blob line-diff is
     the meaty part — `--shortstat` is line counts, not file counts.
   - untracked = dirwalk; `worktree_status` (per-plan body dirty) = diff that
     one path's worktree content vs HEAD's blob (the `--quiet` equivalent).
   - This needs ADDITIVE gix features (`status` + `dirwalk`). Keep features
     tight per the existing `crates/cli/Cargo.toml` gix comment.
2. Reuse ONE gix repo handle for the dirty/worktree computation, across its
   reads (dirwalk, tree-vs-worktree diffs, per-plan body diff), instead of
   re-opening the ODB (re-reading pack indexes) each call. SCOPE: just this
   computation. Threading a shared handle through `git_io.rs`
   (`head_commit`/`commit_events_between`, which also re-open) is the broader
   `replace-git-io-with-gix` migration — OUT of scope here; leave them noted
   for a follow-up.
3. Preserve current semantics EXACTLY: the `dirty: +N/-M` line, untracked
   count, unborn-HEAD degrade-to-0/0, and the `--quiet`-equivalent
   clean/dirty for `worktree_status`.

Independently valuable (lands with or without the incremental model); the
incremental model's `dirty` updater then calls this gix path. Pairs with
`gix-not-git-gate` so it can't regress.

## Testing (no-binary-spawning — in-process gix fixtures, never a spawned binary)

- gix dirty-stats matches the porcelain semantics on fixtures: clean, dirty
  (staged + unstaged), untracked, unborn HEAD.
- One gix handle per rebuild (no per-call re-open).

## Acceptance

- No `git` subprocess in the status dirty/worktree path.
- Output (dirty line, gate inputs) unchanged.
- One gix handle reused across a rebuild's reads.
