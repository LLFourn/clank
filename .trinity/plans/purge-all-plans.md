# purge-all-plans

Add `trinity purge --all` to strip EVERY `.trinity/` path
(every plan, finalize snapshot, stub, gitignore, and any future
Trinity metadata) from history in one operator action.

## Why

Trinity-using projects sometimes need to export code without
`.trinity/` (third bullet in the original purge stub's use cases:
"exporting code without `.trinity/` for an external consumer").
Per-plan purge handles this for a single plan, but real repos
have many plans plus shared metadata (`.trinity/.gitignore`,
historical stubs, anything else committed under `.trinity/`).
Scrubbing them one stem at a time is tedious and incomplete —
non-plan paths like `.trinity/.gitignore` never get stripped.

A single `--all` flag walks history once and strips everything
under `.trinity/`. The single-plan path (`trinity purge <stem>`)
is unchanged.

## Why `--all` instead of redefining no-arg

The original idea was to make `trinity purge` (no plan arg)
default to all-plans. Codex pushed back on the review: that's a
hard compatibility break against the current single-active-plan
inference path, and `--yes` would let it happen non-interactively.
An explicit `--all` flag keeps no-arg behavior stable (still
infers the single active plan via the daemon) and makes
"scrub everything" a deliberate operator decision.

## Behavior

`trinity purge [--all] [<plan>] [--into-branch <name>] [--dry] [--yes]`:

- **`--all`** (no `<plan>` allowed alongside) → strip every
  `.trinity/**` path from history. Per-commit:
  - touched only `.trinity/` paths → Drop
  - touched `.trinity/` + non-`.trinity/` → Rewrite, strip every
    `.trinity/` path the commit touched (any path under
    `.trinity/`, not just plan/finalize files)
  - touched no `.trinity/` paths → KeepVerbatim
- **`<plan>` arg** → unchanged: purge just that plan.
- **no arg, no `--all`** → unchanged: single-active inference
  via the daemon, errors on 0 or >1 active plans.
- **`<plan>` + `--all`** → CLI error: pick one.

`--into-branch`, `--dry`, `--yes` work the same in both modes.

The interactive confirmation prompt distinguishes the all-plans
case (rendered with both interactive and `--yes` to keep
script-driven invocations from accidentally scrubbing history):

```
About to purge EVERY .trinity/ path from history (10 plans
touched in the range) and write rewritten history to a NEW
branch `scrubbed`. The current branch will be left untouched.
Continue? [y/N]
```

The plan count comes from the daemon's response (see endpoint
below). `--yes` still skips the prompt but the warning text is
also logged on stderr before the engine runs.

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

### Add `CommitChanges.trinity_paths`

`CommitChanges` today summarizes plan touches, finalize changes,
and "any non-Trinity code changed." Non-plan-non-finalize
`.trinity/` paths (e.g. `.trinity/.gitignore`, historical
`.trinity/stubs/*`, anything else under `.trinity/`) are
**dropped on the floor** by the current parser. Per codex's
review of the plan stub, that's the blocker for an "all-plans"
purge: those paths would never reach `strip_paths` and would
survive the rewrite.

Fix: add a typed field to `CommitChanges`:

```rust
pub struct CommitChanges {
    ...existing fields...
    /// Every repo-relative path under `.trinity/` that this
    /// commit's diff touched. Includes plan files, finalize
    /// snapshot files, AND anything else under `.trinity/` —
    /// `.gitignore`, stubs, future metadata. Sorted, deduped.
    /// Populated by `git_io::diff_tree_changes` alongside the
    /// existing summaries.
    pub trinity_paths: Vec<String>,
}
```

`git_io::parse_diff_tree` already iterates every changed path
to populate `plan_touches`/`finalize_changes`. Extend the same
loop to push any path starting with `.trinity/` into the new
list (mode of add/modify/rename's destination; deletes are
absent from the resulting tree so they don't need to be in
strip_paths).

The single-plan endpoint is unaffected — it filters strip_paths
by the named plan's `PlanKey`, and that filtering still works
off `plan_touches`/`finalize_changes`. The new field is only
consumed by the all-plans endpoint.

### Walk and classify

`GET /api/repos/{basename}/rewrite_preview_all?include_finalize=…`:

Walk first-parent commits to `snapshot.head` (same helper the
single-plan endpoint uses, `git_io::first_parent_commits_to`).
For each commit:

- `git_io::diff_tree_changes` → `CommitChanges`
- `touched_any_trinity = !trinity_paths.is_empty()`
- `touched_other_paths = has_non_plan_code_changes`
- Disposition:
  - Drop if `touched_any_trinity && !touched_other_paths`
  - Rewrite if `touched_any_trinity && touched_other_paths`
  - KeepVerbatim otherwise
- Strip paths for Rewrite: every entry in `trinity_paths`
  (filtered by `include_finalize=false` if the operator opts out:
  drop entries starting with `.trinity/finished/`).

`intro_sha` is the first walked commit where
`touched_any_trinity` is true. `None` when no commit in the
walk ever touched `.trinity/`.

`plans_touched` aggregates all distinct `PlanKey`s seen across
`plan_touches` and `finalize_changes` in the walked range —
this is for the confirmation prompt's plan-count rendering,
NOT the source of truth for what gets stripped.

`linear` flips false on any merge commit in range (same
`commit_parent_count > 1` check as the single-plan endpoint).

Read-only projection over git + the same `CommitChanges`
projection the fold already uses (now with the extended
field). No new fold state.

### Empty-history behavior

If the walk produced zero commits with `touched_any_trinity`
(`intro_sha.is_none()`), the response still serializes with
`commits` empty and `intro_sha: None`. The CLI dispatches on
this BEFORE invoking the engine:

```text
trinity purge --all
no .trinity/ history found in this repo; nothing to purge.
```

The engine never sees an empty manifest — it would have bailed
on `intro_sha.is_none()` anyway. Keeping the check at the CLI
layer means the daemon endpoint stays purely descriptive and
the engine's invariants stay simple. Regression test asserts
no commits / no ref updates and the no-op message.

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

- `--all` AND `<plan>` → CLI error, refuse before any HTTP call.
- `--all` alone → new all-plans flow:
  - Fetch `/api/repos/{basename}/rewrite_preview_all?include_finalize=true`.
  - If `intro_sha.is_none()` / `commits` empty → print no-op
    message and exit 0.
  - Render the all-plans confirmation (showing
    `plans_touched.len()` — the daemon's plan count, not derived
    on the CLI).
  - Run engine with unpacked fields.
- `<plan>` arg without `--all` → existing single-plan flow.
- No `<plan>`, no `--all` → existing single-active inference.

The existing `infer_single_active_plan` helper stays unchanged —
it's used by `trinity finish` and the no-arg single-plan purge
path. This plan does NOT modify those.

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
4. **All-plans preview with a non-plan Trinity path** (the codex-
   flagged case): a mixed commit touching `src/foo.rs` AND
   `.trinity/.gitignore` (or a `.trinity/stubs/` path) is
   classified as Rewrite, and `.trinity/.gitignore` appears in
   `strip_paths`. Asserts that the new `trinity_paths` field on
   `CommitChanges` is actually used as the source of truth.
5. **All-plans preview on a repo with no `.trinity/` history**:
   `intro_sha: None`, `commits` empty, response is OK (not a
   404), CLI no-ops cleanly.

CLI tests stay light — the existing engine and endpoint tests
cover the load-bearing code. Add:

- The `--all <plan>` mutual-exclusion error (refuses before HTTP).
- The empty-history dispatch path (no engine call, no commits,
  no refs touched).
- The `--all --yes` non-interactive path still logs the all-plans
  warning before running the engine.

`cargo test --workspace --exclude trinity-frontend`,
`cargo clippy --all-targets`, `cargo fmt -- --check`.

## Acceptance criteria

- `trinity purge --all` fetches the new endpoint and runs the
  engine against EVERY `.trinity/` path (plan files, finalize
  snapshots, AND non-plan Trinity paths like
  `.trinity/.gitignore`, `.trinity/stubs/*`). Single-plan
  invocation (`trinity purge foo`) and no-arg single-active
  inference are unchanged.
- `trinity purge --all <plan>` errors before any HTTP call.
- Confirmation prompt clearly states "EVERY `.trinity/` path"
  and includes the plan count from `plans_touched.len()`. The
  warning text is also logged on stderr before the engine runs
  when `--yes` is set.
- Empty-history case (`intro_sha: None`) prints a no-op message
  and exits 0 without calling the engine.
- `--into-branch` and `--dry` work in both modes.
- The daemon's `/api/repos/{basename}/rewrite_preview_all`
  endpoint is the single source of truth for all-plans
  classification — the CLI never re-derives Trinity attribution.
- `CommitChanges.trinity_paths` is the source of truth for
  strip paths in the all-plans case (not the per-plan
  `plan_touches`/`finalize_changes` summaries).
- Engine takes unpacked fields (`intro_sha`/`head_sha`/`linear`/
  `commits`) instead of a `RewritePreviewResponse` reference, so
  both response shapes feed the same engine.

## Phases

**Phase 1 — `trinity_paths` projection + engine refactor.** Add
the new `trinity_paths` field on `CommitChanges`, populate it
in `git_io::parse_diff_tree` from every `.trinity/`-prefixed
changed path. Refactor `RewriteOpts` to unpacked fields; update
the existing `cli::purge` single-plan path to unpack before
calling the engine (no behavior change). Bump `Cargo.toml` to
`0.3.0`.

**Phase 2 — All-plans endpoint.** Add `PurgeAllPreviewResponse`
wire type and `GET /api/repos/{basename}/rewrite_preview_all`
endpoint with full classification + `plans_touched`
aggregation. Endpoint tests including the non-plan-Trinity-path
case and the empty-history case.

**Phase 3 — CLI `--all` dispatch.** Add the `--all` flag to
`PurgeArgs`, wire mutual-exclusion vs. positional `<plan>`,
implement the empty-history short-circuit, the all-plans
confirmation prompt (logged on stderr even under `--yes`), and
the engine dispatch. CLI tests cover the flag combinations.
