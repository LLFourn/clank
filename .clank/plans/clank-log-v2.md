# clank-log-v2

## Summary

Fix `clank log` to be fast and look good. Three changes:

1. **`rebuild_from(repo, start_sha)`** — new rebuild API that
   loads the best cache at or before `start_sha`, then folds
   from there to HEAD collecting log events. The caller says
   where to start; the cache is invisible.
2. **Default to last 30 commits** — `clank log` shows at most
   30 log events by default (like `git log`). `-n N` overrides.
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
    start: &CommitSha,
) -> Result<(RepoState, LogEvents), RebuildError>
```

The caller says "I need fold state and log events starting from
this commit." `start` is **inclusive** — events begin at `start`
itself. The rebuild module:

1. Scans cached heads for the best ancestor strictly before
   `start`. Loads the cache.
2. Two-phase fold-forward from cache to HEAD:
   - **Phase 1 (silent):** fold from cache anchor to `start`'s
     parent. Builds state but discards log events.
   - **Phase 2 (collecting):** fold from `start` through HEAD,
     collecting log events for every commit.
3. Returns `(RepoState, LogEvents)` — events only for commits
   at or after `start`.

If no cache predates `start`, cold-fold from root with the same
two-phase split (silent to `start`'s parent, collect from
`start`).

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

Two-phase approach:

1. Fast rebuild (`rebuild_repo_with_policy(Use)`) to get fold
   state → find the plan's first commit SHA (active plan:
   `plans[key].commits[0].sha`; finished plan:
   `finished_plans[i].intro`).
2. `rebuild_from(intro_parent)` → get `(state, log_events)`.
   Filter events by plan. Scan feedback. Render.

The first call is a cache hit (~instant). The second call loads
a cache before the plan's intro and folds from there (~fast,
only the plan's range + whatever gap to the nearest cache).

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
- New `pub async fn rebuild_from(repo, start) -> Result<(RepoState, LogEvents)>`.
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

- `rebuild_from` with a warm cache: fold-forwards from cache,
  returns events only from `start` onward (not from cache anchor).
- `rebuild_from` on a cold repo: cold-folds, returns events only
  from `start` onward.
- `rebuild_from` with a re-introduced plan key: events contain
  only the current instance, not the prior finalized one.
- Log default shows at most 30 commit groups.
- A commit at the limit boundary keeps its reviews (group stays
  together).
- `--all -n 10` shows all plans but only 10 groups.
- Log `--oneline` produces compact output.
- Existing log tests still pass.

## Acceptance criteria

- `clank log` is fast (uses cache for fold context).
- Default shows 30 most recent events in git-log multi-line
  format with ANSI colors.
- `--oneline` shows compact format.
- `--json` unchanged.
- `rebuild_from` is a clean API that hides the cache.
- No duplicate `apply_commit` functions.
