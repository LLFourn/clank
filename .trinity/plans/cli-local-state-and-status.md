# cli-local-state-and-status

Make the operator CLI derive Trinity state directly from the local
repository instead of querying `trinity serve`, then add
`trinity status` as the first user-facing command built on that local
state path.

## Why

The current CLI split is backwards for mutating commands. The daemon
is useful as a cache, watcher, web UI, and coordination server, but it
should not be required for commands like `trinity finish` and
`trinity purge`. Those commands mutate the user's git repository; at
the moment of mutation the most trustworthy source of truth is the
repository and `.trinity/` files on disk, folded freshly in the process
that is about to mutate git.

This also removes a frustrating operational dependency: an operator can
be in a perfectly valid git repo with all plan and feedback files on
disk, but the CLI fails if the daemon is down, stale, watching an old
worktree, or briefly unreachable. That violates the filesystem-truth
direction of the architecture.

The better model:

- source of truth: git history plus `.trinity/` files on disk
- pure fold: canonical derivation of `RepoState`
- CLI: rebuilds state locally before mutating git
- daemon: cache, UI, file watcher, wait-for-work coordinator

No CLI command should ask the daemon for projection state.

## Current State

The code already has most of the right primitives:

- `src/rebuild.rs::rebuild_repo(repo_root)` runs
  `git_io::snapshot(repo_root)` and `disk_snapshot::derive_state`.
- `src/disk_snapshot.rs::derive_state` is the pure chronological fold.
- `src/responses.rs` contains reusable `RepoState -> api` projection
  helpers for plan lists and work context.
- `src/server/http.rs` still owns the finish/rewrite preview builders
  inline in the HTTP handlers.
- `src/cli/finish.rs` and `src/cli/purge.rs` call daemon HTTP endpoints
  with `reqwest` and then execute local git mutations.

This plan extracts the preview builders out of HTTP and makes both HTTP
and CLI call the same local projection code.

## Non-Goals

- No daemon fallback path for CLI. The goal is not "try daemon, then
  fold locally"; the goal is "CLI never queries daemon".
- No new database or persistent CLI cache.
- No change to MCP or web UI semantics, except that their HTTP handlers
  call the extracted preview builders.
- No redesign of dry-run/live rewrite parity. That is the separate stub
  `.trinity/stubs/dry-run-executes-rendered-program.md`.

## Design

### Local CLI State Entry Point

Add a small CLI-facing state module, for example `src/cli/state.rs`:

```rust
pub async fn load_repo_state(repo: &Path) -> anyhow::Result<RepoState> {
    crate::rebuild::rebuild_repo(repo).await.map_err(...)
}
```

All CLI subcommands that need Trinity state go through this function
after resolving the repo root with the same `git rev-parse
--show-toplevel` behavior as today.

This should be a thin wrapper, not another state model. The CLI should
not cache, partially rebuild, or invent a faster path. It folds the
repo it is about to mutate.

### Shared Preview Builders

Move the logic currently embedded in HTTP handlers into reusable
projection functions. Suggested module names:

- `src/operation_preview.rs`
- or `src/cli_preview.rs`

The module should expose typed functions along these lines:

```rust
pub async fn finish_preview_from_state(
    repo_root: &Path,
    repo_basename: &str,
    state: &RepoState,
    plan_key: &PlanKey,
) -> Result<FinishPreviewResponse, PreviewError>;

pub async fn rewrite_preview_from_state(
    repo_root: &Path,
    repo_basename: &str,
    state: &RepoState,
    plan_key: &PlanKey,
    include_finalize: bool,
) -> Result<RewritePreviewResponse, PreviewError>;

pub async fn rewrite_preview_all_from_repo(
    repo_root: &Path,
    repo_basename: &str,
    include_finalize: bool,
) -> Result<PurgeAllPreviewResponse, PreviewError>;
```

`finish_preview_from_state` should contain the current readiness,
latest-reviewable, gate, worktree-status, and sealed-approval logic.

`rewrite_preview_from_state` should contain the current single-plan
range, native/foreign classification, tree-based strip-path lookup,
linearity check, and `head_strip_paths` logic.

`rewrite_preview_all_from_repo` can rebuild or take a state argument if
useful, but it must remain local and daemon-free. The all-plans variant
currently does more git inspection than state projection; that is fine
as long as the shared builder is used by both CLI and HTTP.

The HTTP routes become adapters:

1. Resolve repo/plan from daemon runtime state.
2. Call the shared builder.
3. Serialize the returned typed response.

The CLI commands become adapters:

1. Resolve repo from `--repo` or cwd.
2. Rebuild local `RepoState`.
3. Resolve plan locally.
4. Call the shared builder.
5. Execute local git mutation.

No duplicate preview logic is allowed.

### Local Plan Resolution

Replace daemon-based plan inference with local inference from
`RepoState`.

Rules:

- If a plan argument is provided:
  - accept `<repo>/<stem>.md`
  - accept `<stem>`
  - reject repo basename mismatch
  - reject missing/ambiguous plan
- If no plan argument is provided:
  - inspect visible active plans in the local `RepoState`
  - require exactly one
  - on zero or many, print a clear error with candidate plan ids

This should share the same visibility/lifecycle rules used by list
plans/status, not ad hoc string filters.

### Remove Daemon Arguments From CLI Commands

Delete `--daemon` / `TRINITY_DAEMON_URL` from:

- `trinity finish`
- `trinity purge`

Keep daemon configuration only where a command is explicitly about
daemon coordination. `trinity init`, `finish`, `purge`, and `status`
should work with no running daemon.

This is an intentional CLI wire break. Acknowledge it in help text and
tests; do not keep compatibility shims just to preserve an option that
should no longer exist.

### Hash Checks Still Matter

`trinity finish` currently protects against daemon/watch lag by
re-reading sealed approval files and checking their body hashes against
the daemon preview. Preserve that invariant, but now the hash comes
from the locally rebuilt state.

The sequence is:

1. Fold repo locally.
2. Build finish preview with sealed approval paths and body hashes.
3. Before writing `.trinity/finished/<stem>/`, re-read each feedback
   file.
4. Recompute hash.
5. Abort on drift.

This still protects against a reviewer editing feedback during the CLI
run.

### `trinity status`

After CLI state is local, add:

```text
trinity status [--repo <path>] [--json]
```

Default repo resolution matches `git status`: start from cwd and use
`git rev-parse --show-toplevel`.

Human output should be compact and operational:

```text
repo: /Users/llfourn/src/trinity
head: 0a0f6de Bump trinity to 0.5.0 for Phases 5-8 ship

plans:
  active:
    trinity/local-cli-state-and-status.md
      path: .trinity/plans/cli-local-state-and-status.md
      phase: planning
      waiting_on: reviewers
      latest: plan 0a0f6de
      gate: approved 2/2
  finished:
    trinity/trinity-cli.md
      finished_at: 0a0f6de
```

Exact formatting can be adjusted, but it must expose:

- canonical repo root
- HEAD sha and subject
- visible plans grouped by lifecycle
- plan id and repo-relative plan path
- current posture/phase
- worktree status
- latest reviewable commit, if any
- gate state / waiting-on summary
- feedback write path for the current reviewer only if an
  `--author-label` flag is later added; do not invent a default author
  in this plan

`--json` should emit a typed response shape from `trinity-core::api`.
Prefer reusing or lightly extending `ListPlansResponse` plus repo
metadata over adding stringly-typed JSON. If a new wire type is needed,
put it in `trinity-core`.

## Testing

Add tests that prove the CLI no longer depends on the daemon:

1. `trinity finish` succeeds in a fixture repo with no daemon running.
2. `trinity finish` rejects unapproved gates using only local state.
3. `trinity purge --dry` succeeds in a fixture repo with no daemon
   running.
4. `trinity purge --into-branch` succeeds in a fixture repo with no
   daemon running.
5. Plan inference with no plan arg uses local active plans and reports
   useful zero/many errors.
6. `trinity status` from a nested cwd resolves the repo root like git.
7. `trinity status --json` round-trips through the typed API shape.
8. HTTP `finish_preview` and CLI-local preview builder produce the same
   `FinishPreviewResponse` for the same fixture.
9. HTTP `rewrite_preview` and CLI-local preview builder produce the same
   `RewritePreviewResponse` for the same fixture.

Run:

```sh
cargo test --workspace --exclude trinity-frontend
cargo clippy --all-targets
cargo fmt -- --check
```

## Phases

### Phase 1: Extract Preview Builders

Move finish/rewrite/all-rewrite preview construction out of
`server/http.rs` and into shared code. HTTP behavior should remain
unchanged. Add parity tests around the extracted functions.

### Phase 2: Local CLI Plan Resolution

Add CLI-local repo-state loading and local plan inference. Remove the
daemon query from plan resolution. Existing CLI commands may still call
HTTP for preview until Phase 3, but plan selection itself should be
local.

### Phase 3: Remove Daemon Queries From `finish`

Switch `trinity finish` and `finish --amend` to use local state plus
the shared finish preview builder. Remove `--daemon` from `FinishArgs`.
Preserve approval body hash drift checks.

### Phase 4: Remove Daemon Queries From `purge`

Switch single-plan purge, all-plans purge, squash, amend, and finish
composite rewrite preview calls to local state/shared builders. Remove
`--daemon` from `PurgeArgs`.

### Phase 5: Add `trinity status`

Add the status command and typed JSON output. Use it as the visible
proof that the CLI can fold and project the current repo without a
daemon.

### Phase 6: Documentation And Cleanup

Update CLI help/docs to say the daemon is not required for
`init`/`status`/`finish`/`purge`. Remove dead reqwest imports and any
CLI-only daemon plumbing made obsolete by this plan.

## Acceptance Criteria

- `trinity finish`, `trinity purge`, and `trinity status` do not make
  HTTP requests and do not accept a daemon URL.
- The daemon can be stopped and the CLI still works against the current
  repo.
- HTTP preview endpoints and CLI commands share the same preview
  builders.
- `trinity status` reports the current repo from cwd without any daemon
  process.
- No duplicate finish/rewrite projection logic remains in CLI and HTTP.
