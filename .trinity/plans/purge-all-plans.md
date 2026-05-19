# purge-all-plans

When `trinity purge` is invoked with no plan argument, default to
purging every plan (active AND finished) instead of erroring on
single-active-plan inference. The intent is "scrub all Trinity
traces from history" in one operator action.

## Why

Today `trinity purge` with no plan arg calls
`infer_single_active_plan` (shared with `trinity finish`), which
errors on zero or multiple active plans. For purge that's the
wrong default — finalized plans have ALREADY succeeded and their
`.trinity/finished/<stem>/` snapshots are exactly what you'd want
to strip when exporting code or detaching from Trinity. Requiring
the operator to name every plan one at a time is also a footgun
on a repo with 10+ plans.

A natural-language "purge all" mode covers the export-to-external
use case (the third bullet in the original purge stub's "use
cases" list: "exporting code without `.trinity/` for an external
consumer") without changing single-plan purge semantics.

## Behavior

`trinity purge [--into-branch <name>] [--dry] [--yes]`:

- **no plan arg** → purge ALL plans (active + finished) in one
  pass. Per-commit:
  - touched only `.trinity/` paths → Drop
  - touched `.trinity/` + non-`.trinity/` → Rewrite, strip every
    `.trinity/` path the commit touched
  - touched no `.trinity/` paths → KeepVerbatim
- **`<plan>` arg** → unchanged: purge just that plan.

`--into-branch`, `--dry`, `--yes` work the same in both modes.

The interactive confirmation prompt distinguishes the all-plans
case:

```
About to purge EVERY plan's .trinity/ history (10 plans total)
and write rewritten history to a NEW branch `scrubbed`. The
current branch will be left untouched. Continue? [y/N]
```

The plan count comes from the daemon's response (see endpoint
below).

## Wire shape

New endpoint: `GET /api/repos/{basename}/rewrite_preview_all?include_finalize=…`

Response:

```rust
pub struct PurgeAllPreviewResponse {
    pub repo_basename: String,
    pub head_sha: CommitSha,
    pub linear: bool,
    /// Earliest commit in the first-parent walk that touched ANY
    /// `.trinity/` path. `None` if no commit ever touched
    /// `.trinity/`.
    pub intro_sha: Option<CommitSha>,
    /// Distinct plan stems whose artifacts appear anywhere in the
    /// walk. For the confirmation prompt's "N plans total"
    /// rendering. Order is sorted+deduped.
    pub plans_touched: Vec<PlanKey>,
    /// Same per-commit shape as `RewritePreviewResponse.commits`.
    /// `foreign` is always false in the all-plans case (no
    /// per-plan attribution applies).
    pub commits: Vec<RewriteCommit>,
}
```

The endpoint reuses `RewriteCommit` from `RewritePreviewResponse`
so the engine consumes one type.

`include_finalize` defaults to `true` for this endpoint —
all-plans purge always strips finished snapshots too. (The
single-plan endpoint defaults to `false`; the contract differs
because the use cases differ.)

## Daemon implementation

Walk first-parent commits to `snapshot.head` (same helper the
single-plan endpoint uses, `git_io::first_parent_commits_to`).
For each commit:

- `git_io::diff_tree_changes` → `CommitChanges`
- Classify based on path predicates:
  - `touched_any_trinity = !plan_touches.is_empty() || !finalize_changes.is_empty()`
  - `touched_other_paths = has_non_plan_code_changes`
  - Drop if `touched_any_trinity && !touched_other_paths`
  - Rewrite if `touched_any_trinity && touched_other_paths`
  - KeepVerbatim otherwise
- Strip paths for Rewrite: every plan_touch's new_path, every
  finalize_change's `.trinity/finished/<plan_key>/<file_name>`
  (only Upsert kind — deletes are already absent).

`intro_sha` is the first walked commit where
`touched_any_trinity` is true.

`plans_touched` aggregates all distinct `PlanKey`s seen across
`plan_touches` and `finalize_changes` in the walked range.

`linear` flips false on any merge commit in range (same
`commit_parent_count > 1` check as the single-plan endpoint).

This is read-only projection over git + the same `CommitChanges`
projection the fold already uses. No new fold state.

## Engine refactor

`cli::rewrite::RewriteOpts` currently takes `&RewritePreviewResponse`.
Unpack to the four fields the engine actually needs so both
`RewritePreviewResponse` and `PurgeAllPreviewResponse` can feed
the same engine:

```rust
pub struct RewriteOpts<'a> {
    pub repo: &'a Path,
    pub intro_sha: Option<&'a CommitSha>,
    pub head_sha: &'a CommitSha,
    pub linear: bool,
    pub commits: &'a [RewriteCommit],
    pub into_branch: Option<&'a str>,
    pub dry: bool,
}
```

The engine's pre-flight checks (linear, dirty worktree,
branch-exists, conditional ref update) are unchanged. The error
messages don't mention any plan-specific name today, so the
all-plans path needs no new copy.

## CLI dispatch

`trinity purge`:

- If `args.plan` is `Some(...)` → existing single-plan flow.
- If `args.plan` is `None` → new all-plans flow:
  - Fetch `/api/repos/{basename}/rewrite_preview_all?include_finalize=true`.
  - Confirm with the all-plans prompt (showing `plans_touched.len()`).
  - Run engine with unpacked fields.

The existing `infer_single_active_plan` helper stays — it's used
by `trinity finish`, which still wants single-active inference.
This plan only changes `trinity purge`'s no-arg semantics.

## Non-goals

- Changing `trinity finish`'s no-arg semantics. `finish` is a
  per-plan ceremony; "finalize all plans" makes no sense.
- A "purge plans matching pattern" mode (e.g. `--all-finished`).
  YAGNI until a real use case shows up.
- Re-running on the no-trinity-history case. The current run
  no-ops cleanly (empty commits list, engine prints nothing
  meaningful) — that's acceptable; we don't need a special
  empty-state message.

## Testing

Engine: unpacked-input refactor is covered by the existing four
engine tests (they construct `RewriteOpts` directly).

Endpoint tests (mirror the single-plan endpoint coverage):

1. **All-plans preview on a repo with one plan + one code commit**:
   commits.len() == 2, first commit Drop, second commit KeepVerbatim,
   `plans_touched == [foo]`.
2. **All-plans preview with a finished plan in HEAD**: the finalize
   commit (which touched `.trinity/finished/foo/*`) is Drop (no
   non-trinity code) when `include_finalize=true`; KeepVerbatim
   when explicitly `include_finalize=false`.
3. **All-plans preview with two plans + a mixed commit touching
   both plan files + code**: that commit is Rewrite, strip_paths
   contains both plan files, `plans_touched == [bar, foo]`.

CLI tests stay light — the existing engine and endpoint tests
cover the load-bearing code. Add one direct test for the
all-plans confirmation message and a smoke test for the no-arg
dispatch picking the right endpoint.

`cargo test --workspace --exclude trinity-frontend`,
`cargo clippy --all-targets`, `cargo fmt -- --check`.

## Acceptance criteria

- `trinity purge` (no arg) fetches the new endpoint and runs the
  engine against ALL plans' artifacts. Single-plan invocation
  (`trinity purge foo`) is unchanged.
- Confirmation prompt clearly states "EVERY plan" and includes
  the plan count.
- `--into-branch` and `--dry` work in both modes.
- The daemon's `/api/repos/{basename}/rewrite_preview_all`
  endpoint is the single source of truth for all-plans
  classification — the CLI never re-derives Trinity attribution.
- Engine takes unpacked fields (`intro_sha`/`head_sha`/`linear`/
  `commits`) instead of a `RewritePreviewResponse` reference, so
  both response shapes feed the same engine.

## Phases

**Phase 1 — Engine refactor + all-plans endpoint.** Refactor
`RewriteOpts` to unpacked fields; update the existing `cli::purge`
single-plan path to unpack before calling the engine (no behavior
change). Add `PurgeAllPreviewResponse` wire type and
`/api/repos/{basename}/rewrite_preview_all` endpoint with tests.
Bump `Cargo.toml` to `0.3.0`.

**Phase 2 — CLI all-plans dispatch.** Update `trinity purge` no-arg
flow to call the new endpoint, render the all-plans confirmation
prompt, and run the engine. Add the CLI dispatch test.
