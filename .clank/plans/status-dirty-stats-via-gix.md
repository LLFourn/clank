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
3. Preserve current semantics, split into two tiers (glm intro note):
   - **Must-be-exact (functional / path-level, no diff-algorithm dependence):**
     the clean/dirty boolean, the untracked count, unborn-HEAD degrade-to-0/0,
     and the `--quiet`-equivalent clean/dirty for `worktree_status`. These feed
     behavior, so they match git exactly.
   - **Best-effort (display-only):** the `+N/-M` line count. It comes from
     git's `--shortstat` diff algorithm; gix's `blob-diff` may split hunks or
     pick a different algorithm and drift by a line on some changes. The dirty
     line is DISPLAY-only (not a gate input), so best-effort is acceptable —
     verify it matches `--shortstat` on the fixtures, and relax the claim if
     gix's diff legitimately drifts.

Independently valuable (lands with or without the incremental model); the
incremental model's `dirty` updater then calls this gix path. Pairs with
`gix-not-git-gate` so it can't regress.

## Testing (no-binary-spawning — in-process gix fixtures, never a spawned binary)

- gix dirty-stats matches the porcelain semantics on fixtures: clean, dirty
  (staged + unstaged), untracked, unborn HEAD.
- One gix handle per rebuild (no per-call re-open).

## Acceptance

- No `git` subprocess in the status dirty/worktree path.
- Functional facts unchanged EXACTLY: clean/dirty boolean, untracked count,
  unborn-HEAD degrade, `worktree_status` clean/dirty.
- `+N/-M` line count is display-only: best-effort match to `--shortstat`
  (verified on fixtures), may drift where gix's diff legitimately differs.
- One gix handle per snapshot, threaded through BOTH `dirty_stats` (status
  walk + per-path diffs) and the per-plan `worktree_status` body diffs — the
  rebuild opens the ODB once (`from_state` via `FsPlanStateLookup::with_git`).
  Standalone callers (preview/open/wfw, and the `dirty_stats(&Path)` entry for
  pr_review/tests) still open their own — they're not the per-repaint hot path.
