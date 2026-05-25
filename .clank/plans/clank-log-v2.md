# clank-log-v2

## Summary

Fix `clank log` to be fast and look good. Three changes:

1. **`rebuild_from(repo, from, to)`** — new rebuild API. `from`
   exclusive, `to` inclusive (git-log convention). Loads the best
   cache before `from`, folds silently through `from`, then
   collects log events for `(from, to]`. Cache is invisible.
2. **Default to last 30 commit groups** — `clank log` shows at
   most 30 commit groups by default. `-n N` overrides.
3. **Git-log-style output** — mimic `git log` rendering with
   ANSI colors. `--oneline` for compact. Default shows full
   commit messages with reviews beneath each commit.

Also clean up the existing code: delete the duplicate
`apply_commit` / `apply_commit_with_log` in `disk_snapshot.rs` —
`apply_commit` should just return `Vec<LogEvent>`, callers that
don't care use `let _ =`.

## Part 1: `rebuild_from`

### API

```rust
pub async fn rebuild_from(
    repo_root: &Path,
    from: &CommitSha,
    to: &CommitSha,
) -> Result<(RepoState, LogEvents), RebuildError>
```

`from` is exclusive, `to` is inclusive — same as `git log from..to`.
The rebuild module:

1. Finds the best cache at or before `from`'s parent. Loads it.
2. Two-phase fold-forward from cache to `to`:
   - **Phase 1 (silent):** fold from cache anchor through `from`.
     Builds state but discards log events.
   - **Phase 2 (collecting):** fold from `from` (exclusive)
     through `to` (inclusive), collecting log events.
3. Returns `(RepoState, LogEvents)` — events only for commits
   in `(from, to]`.

If no cache predates `from`, cold-fold from root with the same
two-phase split.

The cache is invisible to the caller. `rebuild_from` is just
another rebuild entry point — it uses the same `fold_forward`
(which now always returns log events) and the same cache
infrastructure.

### `fold_forward` cleanup

Merge `fold_forward` and `fold_forward_with_log` into one
function that returns `LogEvents`. All callers get events; those
that don't need them discard with `let _ =` or `_log`.

### `apply_commit` cleanup

Delete `apply_commit_with_log` from `disk_snapshot.rs`. Make
`apply_commit` return `Vec<LogEvent>`. Existing callers use
`let _ = apply_commit(...)`.

## Part 2: clank log uses `rebuild_from`

### CLI

```
clank log [<range>] [--plan <stem>] [--all] [-n N] [--oneline]
          [--json] [--repo <path>]
```

`<range>` is optional, git-log-style:
- `<sha>` — show from that commit (inclusive) to HEAD. Resolved
  to `rebuild_from(parent_of(sha), HEAD)` so `sha` is included.
- `<from>..<to>` — commits reachable from `to` but not from
  `from` (exclusive from, inclusive to). Passed directly to
  `rebuild_from(from, to)`.
- Omitted — inferred from the plan's intro's parent to HEAD
  (so the intro itself is included).

Plan selection:
- No args + one active plan: that plan's timeline.
- `--plan <stem>`: specific plan (active or finished).
- `--all`: all plans interleaved.
- No active plans + no `--plan`: most recent finished plan.

### Two-phase approach

1. Fast rebuild (`rebuild_repo_with_policy(Use)`) to get fold
   state → resolve the range (find the plan's intro SHA if no
   explicit range given).
2. `rebuild_from(intro_parent, head)` → get events for `(intro_parent, head]`.
   The intro itself is included (exclusive from). `rebuild_from`
   handles caching internally.
   Filter events by plan. Scan feedback. Render.

The first call is a cache hit (~instant). The second call loads
a cache before the range start and folds from there (~fast,
only the range + whatever gap to the nearest cache).

### Default limit

Show the most recent 30 **commit groups** (a commit + its
reviews count as one group). `-n N` overrides. `-n 0` shows
unlimited history.

`--all` remains the plan-scope selector (all plans), not a
history-depth flag. `--all -n 10` means all plans, 10 most
recent commit groups.

## Part 3: git-log-style rendering

### Default mode (multi-line)

```
commit abc1234 (plan: foo)
Author: LLFourn <lloyd.fourn@gmail.com>
Date:   2026-05-24 10:00

    [foo] intro

    ✓ codex approved

commit def5678 (plan: foo)
Author: LLFourn <lloyd.fourn@gmail.com>
Date:   2026-05-24 11:00

    [foo] implement the thing

    ✗ codex request_changes

        take another look at the edge case
```

Colors (ANSI):
- Commit SHA: yellow
- `(plan: ...)`: cyan
- `✓ approved`: green
- `✗ request_changes`: red
- Author/Date: normal

### `--oneline` mode

```
abc1234 [foo] intro                           ✓ codex
def5678 [foo] implement the thing             ✗ codex
ghi9012 [foo] address codex                   ✓ codex
jkl3456 Finalize foo
```

Colors: SHA yellow, verdict green/red.

### `--json` mode

Array of typed event objects with reviews as separate `kind:
"review"` events after their target commit (already implemented).

## Implementation

### `crates/cli/src/rebuild.rs`

- `fold_forward` returns `LogEvents` (merge the two variants).
- New `pub async fn rebuild_from(repo, from, to) -> Result<(RepoState, LogEvents)>`.
  `from` exclusive, `to` inclusive (git-log convention).
- Existing `rebuild_repo_with_policy` unchanged (discards log).

### `crates/cli/src/disk_snapshot.rs`

- Delete `apply_commit_with_log`. Make `apply_commit` return
  `Vec<LogEvent>`. Update `derive_state` and
  `derive_state_with_log` accordingly.

### `crates/cli/src/cli/log.rs`

- Two-phase rebuild: fast first, `rebuild_from` second.
- `-n` / `--oneline` flags on `LogArgs`.
- `print_human` replaced with `print_default` (multi-line) and
  `print_oneline`.
- ANSI color output (respect `NO_COLOR` env).
- Git log data: author name + email from `git log --format`.

### `crates/cli/src/cli/mod.rs`

- Add `-n` and `--oneline` to `LogArgs`.

## Tests

- `rebuild_from(A, B)` with a warm cache: loads cache before A,
  folds silently through A, collects events for `(A, B]`.
- `rebuild_from` on a cold repo: cold-folds, same `(from, to]`.
- `rebuild_from` with a re-introduced plan key: events contain
  only the current instance, not the prior finalized one.
- Log default shows at most 30 commit groups.
- A commit at the limit boundary keeps its reviews (group stays
  together).
- `--all -n 10` shows all plans but only 10 groups.
- `clank log A..B` excludes commit A, includes B.
- `clank log A` includes commit A.
- Log `--oneline` produces compact output.
- Existing log tests still pass.

## Acceptance criteria

- `clank log` is fast (uses cache for fold context).
- Default shows 30 most recent commit groups in git-log
  multi-line format with ANSI colors.
- `--oneline` shows compact format.
- `--json` unchanged.
- `rebuild_from` is a clean API that hides the cache.
- No duplicate `apply_commit` functions.
