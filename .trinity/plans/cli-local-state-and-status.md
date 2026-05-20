# cli-local-state-and-status

Make the operator CLI derive Trinity state directly from the local
repository instead of querying `trinity serve`, then add
`trinity status` as the first user-facing read command built on that
local state path.

## Why

The current CLI is structured backwards for mutating commands. The
daemon is useful as a cache, file watcher, web UI, and coordination
server, but it should not be required for commands that mutate the
user's git repository. At the moment of mutation the most trustworthy
source of truth is the repository plus the `.trinity/` files on disk,
folded freshly in the same process that is about to mutate git.

This also removes a frustrating operational dependency: an operator
can be in a perfectly valid repo with all plan and feedback files on
disk, but `trinity finish` / `trinity purge` fail if the daemon is
down, stale, watching an old worktree, or briefly unreachable. That
violates the filesystem-truth direction the rest of the architecture
already commits to.

The correct model:

- source of truth: git history plus `.trinity/` files on disk
- pure fold: canonical derivation of `RepoState`
- CLI: rebuilds state locally inside the process that mutates git
- daemon: cache, web UI, file watcher, MCP coordination

No CLI mutation should ask the daemon for projection state.

## Current State

The fold and projection primitives already exist:

- `src/rebuild.rs::rebuild_repo(repo_root)` runs
  `git_io::snapshot(repo_root)` and `disk_snapshot::derive_state`.
- `src/disk_snapshot.rs::derive_state` is the pure chronological fold.
- `src/responses.rs` projects `RepoState` into the wire shapes used by
  list plans / work context.
- `src/server/http.rs` still owns the finish/rewrite preview builders
  inline in the HTTP handlers.
- `src/cli/finish.rs` and `src/cli/purge.rs` call daemon HTTP endpoints
  with `reqwest` and then run local git mutations.

The remaining work is to lift the preview builders out of the HTTP
layer so both HTTP and CLI invoke the same projection code, and to
remove the HTTP round trip from the CLI mutation paths.

## Non-Goals

- No daemon fallback for the CLI. The goal is not "try daemon, then
  fold locally"; the goal is "CLI never queries the daemon".
- No new persistent CLI cache.
- No redesign of dry-run vs. live rewrite parity. That is the separate
  stub `.trinity/stubs/dry-run-executes-rendered-program.md`.
- No change to MCP or web UI semantics. `wait_for_work`, `start_plan`,
  SSE, and the web SPA stay daemon-coupled — this plan only touches
  the operator-mutating CLI surfaces and the read-only `status`
  command.
- No compatibility shim for the removed `--daemon` flag on
  `finish`/`purge`. Heavy-dev mode: clean break, hard error if anyone
  passes it.

## Design

### Local State In CLI Commands

CLI subcommands that need Trinity state call `rebuild::rebuild_repo`
directly after resolving the repo root with `resolve_repo` (the
existing `git rev-parse --show-toplevel` helper in `src/cli/mod.rs`).

No new wrapper module — `rebuild_repo` already returns a `RepoState`
and is the canonical fold. Adding `src/cli/state.rs` to call one
function is noise.

The CLI must not cache, partially rebuild, or invent a faster path.
It folds the repo it is about to mutate, every time.

### Shared Preview Builders

Move the finish/rewrite projection logic currently inlined in
`src/server/http.rs` into a new module `src/preview.rs`. This is the
canonical preview module; both HTTP and CLI become thin callers.

Exposed surface:

```rust
pub async fn build_finish_preview(
    repo_root: &Path,
    state: &RepoState,
    plan_key: &PlanKey,
) -> Result<FinishPreviewResponse, PreviewError>;

pub async fn build_rewrite_preview(
    repo_root: &Path,
    state: &RepoState,
    plan_key: &PlanKey,
    include_finalize: bool,
) -> Result<RewritePreviewResponse, PreviewError>;

pub async fn build_rewrite_preview_all(
    repo_root: &Path,
    state: &RepoState,
    include_finalize: bool,
) -> Result<PurgeAllPreviewResponse, PreviewError>;
```

The builders are async because they invoke git plumbing
(`tree_plan_paths`, `commit_parent_count`, `first_parent_commits_to`,
`diff_tree_changes`) which already runs `git` as a subprocess via
async helpers in `src/git_io.rs`. There is no synchronous shortcut;
the existing HTTP implementation already awaits these calls.

`repo_basename` is derived inside the builder from `repo_root` —
callers don't pass it.

`PreviewError` is a typed enum in the app crate (alongside
`src/preview.rs`, not in `trinity-core::api`). It wraps git/IO
specifics and is consumed in-process by both the CLI and the HTTP
adapters; HTTP maps variants to status codes at the edge. Wire
DTOs in `trinity-core::api` stay free of operational error types.

`build_finish_preview` owns readiness, latest-reviewable resolution,
gate check, worktree-status, and sealed-approval projection.

`build_rewrite_preview` owns the single-plan range, native/foreign
classification, tree-based strip-path lookup, linearity check, and
`head_strip_paths` projection.

`build_rewrite_preview_all` owns the all-plans variant. It currently
does more git inspection than state projection; both still go through
this single builder.

HTTP routes become adapters:

1. Resolve repo/plan from daemon runtime state.
2. Call the shared builder.
3. Serialize the typed response.

CLI commands become adapters:

1. Resolve repo from `--repo` or cwd.
2. Rebuild local `RepoState`.
3. Resolve plan locally (see below).
4. Call the shared builder.
5. Execute the local git mutation.

No duplicate preview logic is allowed to remain in either layer.

### Local Plan Resolution

Replace daemon-based plan inference with local inference from
`RepoState`. Live in `src/cli/plan_resolve.rs`.

Rules:

- If a plan argument is provided:
  - accept `<repo>/<stem>.md`
  - accept `<stem>`
  - reject repo basename mismatch
  - reject missing/ambiguous plan with a clear error listing
    candidates
- If no plan argument is provided:
  - inspect visible active plans in the local `RepoState`
  - require exactly one
  - on zero or many, print a clear error with candidate plan ids

"Active" must use the same visibility/lifecycle rules used by
`list_plans` and the upcoming `trinity status`, not ad-hoc string
filters.

### Remove Daemon Arguments From CLI Mutations

Delete `--daemon` / `TRINITY_DAEMON_URL` from:

- `trinity finish`
- `trinity purge`

`trinity init` is already daemon-free; confirm and add a regression
test.

`TRINITY_DAEMON_URL` remains valid for the MCP shim and any explicit
daemon-coordination commands. Just not for these mutating CLIs.

This is an intentional CLI wire break. No compat shim, no deprecation
window. Help text and tests reflect the new shape directly.

### Hash Drift Checks Stay

`trinity finish` already protects against daemon/watch lag by
re-reading sealed approval files and comparing their body hashes to
the preview snapshot before writing `.trinity/finished/<stem>/`.
Preserve that invariant — the hash now comes from the locally
rebuilt state instead of an HTTP response.

Sequence:

1. Fold repo locally.
2. Build finish preview with sealed-approval paths and body hashes.
3. Before writing `.trinity/finished/<stem>/`, re-read each feedback
   file.
4. Recompute hash.
5. Abort on drift.

This continues to protect against a reviewer editing feedback during
the CLI run.

### `trinity status`

After CLI state is local, add:

```text
trinity status [--repo <path>] [--json]
```

Default repo resolution matches `git status`: start from cwd, walk to
the git toplevel.

Human output is compact and scannable. Sketch:

```text
repo  /Users/llfourn/src/trinity (master @ 0a0f6de)

active plans:
  trinity/cli-local-state-and-status.md
    phase    intro
    waiting  reviewers
    latest   0a0f6de  Bump trinity to 0.5.0 for Phases 5-8 ship
    gate     0/2

finished plans:
  trinity/trinity-cli.md         @ 064ddf0
```

Exact formatting may be adjusted during implementation, but the
output must surface:

- canonical repo root, current branch, HEAD short-sha + subject
- visible plans grouped by lifecycle (active / finished)
- plan id and repo-relative plan path
- current phase / posture
- waiting-on role
- latest reviewable commit, if any
- gate state
- worktree dirty / clean indicator (compact, e.g. trailing `*` on
  the head line)

No author-specific output in this plan. A future `--author-label`
flag can add "your next action" later; do not invent a default
author here.

`--json` emits a typed response from `trinity-core::api`. Prefer
extending `ListPlansResponse` with repo metadata over coining a new
stringly-typed shape. If a new wire type is genuinely needed, it
lives in `trinity-core`.

## Testing

Tests must prove the CLI no longer touches the daemon:

1. `trinity finish` succeeds in a fixture repo with no daemon
   running.
2. `trinity finish` rejects an unapproved gate using only local
   state.
3. `trinity finish` aborts on feedback-body-hash drift detected
   between the local fold and the on-disk file at write time.
4. `trinity purge --dry` succeeds in a fixture repo with no daemon
   running.
5. `trinity purge --into-branch` succeeds in a fixture repo with no
   daemon running.
6. `trinity init` works with no daemon running (regression).
7. Plan inference with no plan arg uses local active plans and
   reports useful zero/many errors.
8. `trinity status` from a nested cwd resolves the repo root like
   `git status`.
9. `trinity status --json` round-trips through the typed API shape
   in `trinity-core`.
10. HTTP `finish_preview` and the shared builder produce the same
    `FinishPreviewResponse` for the same fixture.
11. HTTP `rewrite_preview` and the shared builder produce the same
    `RewritePreviewResponse` for the same fixture.

Run:

```sh
cargo test --workspace --exclude trinity-frontend
cargo clippy --all-targets
cargo fmt -- --check
```

## Phases

### Phase 1: Extract Preview Builders

Lift finish / rewrite / all-rewrite preview construction out of
`server/http.rs` into `src/preview.rs`. HTTP behavior unchanged. Add
parity tests around the extracted functions and the typed
`PreviewError`.

### Phase 2: Local CLI Plan Resolution

Add `src/cli/plan_resolve.rs` and the local plan-inference rules.
Remove daemon queries from plan selection. Existing CLI commands may
still call HTTP for preview at this point — only plan selection is
moved local.

### Phase 3: Remove Daemon Queries From `finish`

Switch `trinity finish` and `finish --amend` to use local state plus
the shared finish preview builder. Remove `--daemon` from
`FinishArgs`. Preserve approval body hash drift checks.

### Phase 4: Remove Daemon Queries From `purge`

Switch single-plan purge, all-plans purge, squash, amend, and the
finish composite rewrite preview to local state / shared builders.
Remove `--daemon` from `PurgeArgs`.

### Phase 5: Add `trinity status`

Ship the status command and its typed JSON output. This is the
visible proof that the CLI can fold and project the current repo
without a daemon.

### Phase 6: Cleanup

Drop dead `reqwest` use from CLI mutation paths. Update CLI help and
any inline docs to say the daemon is not required for
`init`/`status`/`finish`/`purge`. Remove any CLI-only daemon plumbing
made obsolete.

## Acceptance Criteria

- The daemon process can be fully stopped and
  `trinity init`/`status`/`finish`/`purge` still work against the
  current repo.
- `trinity finish`, `trinity purge`, and `trinity status` make no
  HTTP requests and do not accept a daemon URL or env var.
- HTTP preview endpoints and CLI commands share the same preview
  builders. No duplicate finish/rewrite projection logic remains.
- `trinity status` resolves the current repo from cwd without any
  daemon process.
- MCP coordination (`wait_for_work`, `start_plan`) and the web UI
  continue to function unchanged; this plan does not regress them.
