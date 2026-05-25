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

4. **Feedback summary convention** — change feedback format from
   `VERDICT\n\n<body>` to `VERDICT <one-line summary>\n\n<body>`.
   Like git commits: first line is verdict + summary, blank line,
   then details. `FeedbackBody::summary()` extracts the summary
   from the first line. `clank feedback write --summary` or the
   verdict line itself carries the summary. Update skill files to
   explain the new convention.

## Part 1: `rebuild_from`

### API

```rust
pub async fn rebuild_from(
    repo_root: &Path,
    from: Option<&CommitSha>,
    to: &CommitSha,
) -> Result<(RepoState, LogEvents), RebuildError>
```

`from` is exclusive, `to` is inclusive — same as `git log from..to`.
`from = None` means "from repository root" (all commits up to `to`
are included — handles root commits with no parent).
The rebuild module:

1. Finds the best cache at or before `from` (or root if `None`).
   Loads it.
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
clank log [<range>] [--plan <stem>] [-n N] [--oneline]
          [--json] [--repo <path>]
```

`<range>` is optional, git-log-style:
- `<sha>` — show from that commit (inclusive) to HEAD. Resolved
  to `rebuild_from(parent_of(sha), HEAD)` — or
  `rebuild_from(None, HEAD)` if `sha` is the root commit.
- `<from>..<to>` — commits reachable from `to` but not from
  `from` (exclusive from, inclusive to). Passed directly to
  `rebuild_from(from, to)`.
- Omitted — last 30 commits from HEAD (like `git log` with no
  args). Shows whatever plan events are in that range.

`--plan <stem>` filters events to that plan within the range.

### Approach

Bare `clank log`: `rebuild_from(HEAD~30_parent, HEAD)`. No plan
resolution, no fold-state lookup. Just fold the last 30 commits,
collect events, filter by `--plan` if given, render.

`clank log --plan foo`: same 30-commit range but filter events
to plan `foo`. If the plan's intro is older than 30 commits,
the user passes an explicit range or `-n 0`.

`clank log <range>`: `rebuild_from` with the parsed range.
No default limit applied (the user specified the range).

`rebuild_from` handles caching internally.

### Default limit

`-n 30` (default): fold the last 30 commits from HEAD.
`-n N` overrides. When an explicit `<range>` is given, `-n`
is ignored (the range determines the window).

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

- `rebuild_from(HEAD~N_parent, HEAD)` for the default range.
- `--plan` filter applied after events are collected.
- Git-log-style multi-line default output with ANSI colors.
- `--oneline` compact output.

### `crates/cli/src/cli/mod.rs`

- Add `<range>`, `-n`, `--oneline` to `LogArgs`. Remove `--all`.

## Tests

- `rebuild_from(A, B)` with a warm cache: loads cache before A,
  folds silently through A, collects events for `(A, B]`.
- `rebuild_from` on a cold repo: cold-folds, same `(from, to]`.
- Bare `clank log` shows events from the last 30 commits.
- `clank log --plan foo` filters to plan `foo` within the range.
- `clank log A..B` excludes commit A, includes B.
- `clank log A` includes commit A.
- `clank log --oneline` produces compact output.
- Existing log tests still pass.

## Acceptance criteria

- `clank log` is fast (folds only the last 30 commits by
  default, using the cache for fold context).
- Default shows events from the last 30 commits in git-log
  multi-line format with ANSI colors.
- `--oneline` shows compact format.
- `--json` unchanged.
- `rebuild_from` is a clean API that hides the cache.
- No duplicate `apply_commit` functions.
