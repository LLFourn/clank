# Plan Path Identity

## Summary

Replace Trinity's primary `session_id` concept with explicit plan identity: `(repo_root, plan_file_path)`, where `plan_file_path` is the repo-relative path to the tracked plan file, such as `.trinity/plans/leptos-frontend.md` or `.trinity/plans/done/runtime-lock-boundaries.md`.

`session_id` becomes a derived display slug (the filename stem). It stops being the primary key for state, MCP calls, or UI identity, and is removed from public API inputs — callers pass `plan_path`.

Two related cleanups fall out:

- The recurring `?repo=` URL plumbing in the leptos plan stops being awkward — `(repo, plan_path)` is just the natural identity, of which `?repo=` is one half. The other half is the plan path itself, not a derived slug.
- Active↔done moves stop needing special-case logic: the plan file's stable identity is its filename stem, and the runtime keys plans by that stem with the *current* path tracked as state.

## Problem

Trinity now treats session identity as effectively `(repo_root, session_id)`. That is already better than a global `session_id`, but it still keeps a separate naming layer that is mostly derived from the plan filename.

Current examples:

- `.trinity/plans/foo.md` becomes `session_id = "foo"`.
- `.trinity/plans/done/foo.md` keeps `session_id = "foo"`.
- Feedback lives under `.trinity/feedback/foo/...`.
- MCP calls use `{ session_id, repo }`.
- UI routes use `/sessions/:id?repo=...`.

This creates avoidable ambiguity and synchronization problems:

- The real filesystem object is the plan file, not the derived id.
- Worktrees are naturally path-scoped by repo root.
- Future nested plan paths or renamed plan files do not fit cleanly into a flat `session_id` namespace.
- Feedback path parsing depends on a derived slug rather than the actual plan path.
- Agents already need explicit returned paths; they should not need to reconstruct identity from ids.

## Target Model

A plan is identified by:

```text
(canonical_repo_root, repo_relative_plan_file_path)
```

Examples:

```text
(/Users/llfourn/src/trinity, .trinity/plans/leptos-frontend.md)
(/Users/llfourn/src/trinity, .trinity/plans/done/runtime-lock-boundaries.md)
(/Users/llfourn/src/trinity-worktree, .trinity/plans/leptos-frontend.md)
```

The last two components are intentionally different identities when the repo root differs, even if the plan path is the same.

## Goals

- Remove `SessionId` as the primary key in runtime state and public APIs.
- Runtime maps are keyed by `PlanKey` (derived filename stem); public APIs carry `plan_path` (repo-relative path).
- MCP tools accept `plan_path` instead of `session_id`; no compatibility shim, the schema is strict.
- UI routes carry `(repo, plan_path)` as the query-encoded identity.
- Feedback paths are returned explicitly in MCP responses; agents never reconstruct.
- The active↔done move preserves a plan's logical lifecycle because `PlanKey` is stable across the move.

## Non-Goals

- No on-disk feedback layout change (§3 keeps the existing `.trinity/feedback/<slug>/...` shape; the slug == `PlanKey`).
- No automatic migration of old session-id-shaped feedback directories — the disk layout is unchanged, so there's nothing to migrate.
- No global cross-repo identity.
- No attempt to merge sessions across worktrees (two worktrees with the same plan filename are two distinct plans).
- No support for nested plan paths under `.trinity/plans/<subdir>/` in this pass — `PlanKey::from_path` rejects them. Adding support is a separate plan.
- No daemon ingestion of `~/.trinity/stubs`.

## Design

### 1. Identity types

Two newtypes in `src/lifecycle.rs` (no new module — they sit next to `CommitSha`, `AgentLabel`, `ContentHash`):

```rust
/// Repo-relative path to a tracked plan file. Normalized to either
/// `.trinity/plans/<stem>.md` or `.trinity/plans/done/<stem>.md`. Used
/// as the public-facing identifier for the plan in API surfaces.
pub struct PlanPath(PathBuf);

/// Stable identity that survives an active↔done move. Derived from the
/// filename stem (the part between `.trinity/plans/[done/]?` and `.md`).
/// Both `.trinity/plans/foo.md` and `.trinity/plans/done/foo.md` map to
/// the same `PlanKey("foo")`. This is what the runtime maps key on.
pub struct PlanKey(String);

impl PlanKey {
    pub fn from_path(p: &Path) -> Option<Self> { /* strips dir + .md */ }
}
```

`PlanKey::from_path` replaces the existing `session_id_from_plan_path` in `src/disk_format.rs`. `PlanKey` is internal; `PlanPath` is what crosses boundaries (MCP, HTTP, feedback file paths, log lines).

Runtime state moves to:

```rust
pub struct RepoState {
    pub root: PathBuf,
    pub plans: BTreeMap<PlanKey, Plan>,
    pub attribution: BTreeMap<CommitSha, AttributionResult>,
    pub plan_touches: BTreeMap<CommitSha, Vec<(PlanKey, PlanTouchKind)>>,
    pub commit_order: Vec<CommitSha>,
    pub head: Option<CommitSha>,
}
```

`Session` is renamed to `Plan` (`pub struct Plan { id: PlanKey, plan_path: PlanPath, body: String, body_hash: ContentHash, plan_intro: CommitSha, ... }`). `AttributionResult::Attributed` carries `PlanKey` instead of `SessionId`.

### 2. Lifecycle across active/done

Decision: **the runtime's primary key is `PlanKey` (the stable filename stem). The current `plan_path` rides on the `Plan` record as state.**

Rationale: that's what `SessionId`-keyed maps already do today, just with the conceptual leak that `SessionId` looks user-visible. Renaming to `PlanKey` makes it clear it's internal and derived. No bridging logic needed — moving `.trinity/plans/foo.md` to `.trinity/plans/done/foo.md` updates `Plan.plan_path` while `Plan.id` stays `PlanKey("foo")`. The rebuild path already does this; we're just renaming the key type.

This makes nested plan paths (e.g. `.trinity/plans/team-a/foo.md`) fall out of scope until the same-stem-in-different-subdirs collision is solved separately. Non-goals lock that down for now.

### 3. Feedback storage

Decision: **keep the current on-disk layout.**

```text
.trinity/feedback/<plan-key>/<phase>/<sha>/<author>.md
```

The directory name is `PlanKey` — exactly what `<session_id>` is today (filename stem). Renaming `SessionId` to `PlanKey` is purely a code change; no files move on disk, no migration needed. The slug stays human-readable for `ls .trinity/feedback/` and avoids the by-plan/hash hop the original stub proposed.

This works because:

- One plan = one `PlanKey` = one feedback directory, even after the active/done move (the `PlanKey` doesn't change when the path moves).
- Two plans in two repos collide only in repo scope; the daemon already keys runtime state by `(repo_root, PlanKey)`. Disk-side, the feedback dir lives under the repo's `.trinity/feedback/`, so the repo dimension is implicit in the path.
- The hash-keyed alternative was insurance against same-stem collisions, which are already prevented by the daemon's "no two plans share a `PlanKey` in one repo" invariant — that invariant tightens when nested plan paths are introduced, but they're explicitly out of scope.

MCP responses keep returning explicit `write_feedback.path` so agents never compute the dir name themselves.

### 4. MCP shape changes

Replace `session_id` inputs with `plan_path`. No compatibility shim — the canonical schema requires `plan_path` and rejects `session_id` cleanly via the JSON schema's `additionalProperties: false`.

```json
{
  "repo": "/abs/repo/root",
  "plan_path": ".trinity/plans/leptos-frontend.md",
  "author_label": "codex"
}
```

Tools affected:

- `start_plan` — accepts `plan_path` directly. Drop the auto-construction of `.trinity/plans/<id>.md`. The caller chooses the path; the daemon validates it sits under `.trinity/plans/`.
- `get_context` — accepts `plan_path` (was `session_id`).
- `wait_for_work` — accepts `plan_path` (was `session_id`). Response `Match.session_id` → `Match.plan_path`. The work-tool description's example loop is updated in lock-step.
- `list_sessions` — response rows emit `plan_path` (and a derived `slug` for display). Tool renamed to `list_plans` for consistency; the old tool name stays only if MCP host UIs cache the catalog.

`start_plan` keeps `label` for the agent-name autofill (same field as today).

Response shapes:

- `wait_for_work` `Match`: `{ repo, plan_path, reason }` (was `{ repo, session_id, reason }`).
- `get_context`: `plan_path` replaces `session_id` in the response; `slug` is added as a derived display helper. Everything else stays.

Tool descriptions explicitly note that `plan_path` is repo-relative, must match the canonical `.trinity/plans/` or `.trinity/plans/done/` form, and is case-sensitive on the filesystem.

### 4b. MCP shim cache

The shim caches `author_label` today. After this rewrite it should also cache the **last `plan_path`** the caller used, so re-entering an agent loop after a tool call doesn't require re-typing the path. Same pattern as `author_label`: per-tool autofill keyed by which arg name carries the path.

### 5. UI routes + `/api/*`

Decision: **query-param identity, not path-in-path.**

```text
GET  /                                                # SPA home
GET  /plan?repo=<repo>&path=<plan_path>               # SPA plan detail
GET  /plan/revision?repo=&path=&sha=                  # SPA plan rev view
GET  /plan/commit?repo=&path=&sha=                    # SPA commit diff
GET  /plan/diff?repo=&path=&from=&to=                 # SPA plan rev-vs-rev
POST /api/plan/done                                   # body: { repo, path }

GET  /api/plans?repo=<repo>                           # rows of {repo, plan_path, ...}
GET  /api/plan?repo=&path=                            # rich plan detail
GET  /api/plan/revision?repo=&path=&sha=
GET  /api/plan/commit?repo=&path=&sha=
GET  /api/plan/diff?repo=&from=&to=&path=
```

Rationale for query params: plan paths contain slashes (`.trinity/plans/foo.md`). Encoding a slash-containing string into a path segment works but trips up server-side path-routers (axum's path matchers don't naturally accept percent-encoded slashes). Query params sidestep it.

The leptos plan's `/sessions/:id` routes get rewritten to this shape. `<MetaStrip>`, `<PlanRevision>`, `<CommitDiff>`, `<PlanDiff>` build hrefs from `(repo, plan_path)` directly; the `?repo=` plumbing the leptos plan deferred lands naturally as a consequence of this work.

Backend handlers (`api_sessions`, `api_session_detail`, etc.) get renamed to `api_plans` / `api_plan` to match the URL shape.

### 6. Internal projections

`src/projection.rs` already has the `_for` / `_for_parts` variants from the runtime-lock-boundaries work. Those become the canonical signatures; the `&Session` / `&RepoState` overloads either disappear or take `&Plan` / `&RepoState` after rename. Specifically:

- `phase_for(plan_path, plan_key, attribution) -> Phase`
- `all_plan_revisions_for(plan_key, commit_order, plan_touches) -> Vec<CommitSha>`
- `all_implementation_commits_for(plan_key, commit_order, attribution) -> Vec<CommitSha>`
- `plan_gate_for_parts(plan_key, plan_feedback, commit_order, plan_touches) -> Option<ReviewGateDecision>`
- `impl_gate_for_parts(plan_key, impl_feedback, commit_order, attribution) -> Option<ReviewGateDecision>`

The wait_for_work matcher (`src/server/wait.rs`) currently takes `session_id: &SessionId`; that argument becomes `plan_key: &PlanKey`, with the wire shape carrying `plan_path` that `compute_match` resolves to a `PlanKey` on entry. Unknown `plan_path` → `WaitError::UnknownPlan(String)` (was `UnknownSession`).

The HTTP/UI response builders (`src/ui_response.rs`, `src/mcp_response.rs`) take snapshot bundles whose session field is renamed `plan`. The `runtime_snapshot::SessionSnapshotBundle` becomes `PlanSnapshotBundle`.

### 7. Compatibility strategy

**Breaking internal model change, no migration shim, no compatibility code.**

Concrete posture:

- `cargo test` updates fixtures to use `plan_path` everywhere.
- The MCP catalog drops `session_id` from input schemas. Existing MCP host caches advertising the old shape will fail with `additionalProperties` violation — that's the right failure mode; the alternative is silent drift between schema and code.
- On-disk feedback layout is unchanged (see §3), so existing `.trinity/feedback/<slug>/...` directories keep working — the slug just happens to be `PlanKey` instead of `SessionId`.
- The web UI is the only surface where an old bookmark `/sessions/<id>?repo=<r>` stops working. The SPA shell catches that path (axum fallback) and the SPA's client router shows `<NotFound>` with a hint. Not worth a redirect.

## Phases

Phase boundaries match the surfaces, so each phase is independently testable. Four phases instead of the stub's five — Phase 1 absorbs the old "feedback rewrite" because §3 decided to keep the on-disk layout.

### Phase 1 — Identity types + core rename

Files: `src/lifecycle.rs`, `src/disk_format.rs`, `src/repo_state.rs`, `src/disk_snapshot.rs`, `src/attribution.rs`, `src/rebuild.rs`, `src/runtime.rs`, `src/runtime_snapshot.rs`, `src/projection.rs`.

- Add `PlanPath(PathBuf)` and `PlanKey(String)` newtypes.
- Replace `SessionId` with `PlanKey` everywhere it's used as a map key or identity (`RepoState.sessions` → `RepoState.plans: BTreeMap<PlanKey, Plan>`, `AttributionResult::Attributed { session: PlanKey, .. }`, `state.plan_touches` value tuple, `Session.id`, all projection `_for_parts` helpers).
- Rename `Session` to `Plan`, `SessionSnapshot` to `PlanSnapshot`, `SessionSnapshotBundle` to `PlanSnapshotBundle`.
- Test: `cargo test` green after rename; no behavior change yet.

### Phase 2 — MCP + wait_for_work rewrite

Files: `src/tools.rs`, `src/server/mcp.rs`, `src/server/wait.rs`, `src/mcp_response.rs`, `src/mcp_shim/mod.rs`.

- Replace `session_id` with `plan_path` in every tool schema. `additionalProperties: false` enforces the strict cutover.
- `WaitArgs.session_id` → `WaitArgs.plan_path`. `Match.session_id` → `Match.plan_path`. `WaitError::UnknownSession` → `WaitError::UnknownPlan`.
- Tool descriptions reworded: example loops use `plan_path`.
- Shim cache gains a `last_plan_path` per-tool fallback alongside `last_label`.
- `list_sessions` renamed to `list_plans`; rows emit `{repo, plan_path, slug, phase, worktree_status, waiting_on}`.
- Tests: wire tests for plan_path-shaped requests; the wait_for_work integration suite gets renamed but keeps its scenarios.

### Phase 3 — HTTP + leptos route rewrite

Files: `src/server/http.rs`, `src/ui_response.rs`, `frontend/src/api.rs`, `frontend/src/main.rs`, `frontend/src/components/*.rs`.

- Backend handlers move under `/api/plans*` and `/api/plan*` per §5.
- `frontend/src/api.rs` types rename: `SessionRow` → `PlanRow`, `SessionDetail` → `PlanDetail`. Field names follow.
- Leptos router routes get the new shape (`/plan?repo=&path=...` etc.). `<MetaStrip>` etc. build links from `(repo, plan_path)`.
- The leptos plan's deferred `?repo=` URL plumbing lands here as a side effect — the SPA emits `?repo=` because every plan link uses both halves of the identity.
- Tests: e2e tests update path-based assertions; the wire tests in `src/server/http.rs::wire_tests` keep their structure but use the new URLs.

### Phase 4 — Cleanup

- Remove `SessionId` from `src/lifecycle.rs` (it survives Phase 1-3 only as a type-alias for legacy callers; Phase 4 drops the alias).
- Update CLAUDE.md and any stray prose references to "session" in tool descriptions where the term is now misleading.
- Update memory entries that mention `session_id` as identity (e.g. `mcp-vs-ui-surface-separation.md`).

## Acceptance Criteria

- `rg "SessionId" src/` shows no occurrences outside of test-only fixtures.
- `rg "session_id" src/tools.rs src/server src/mcp_response.rs src/ui_response.rs` shows no occurrences (the MCP schemas use `plan_path` and JSON responses key on it too).
- `rg "session_id" frontend/src/` shows no occurrences (component types and fetch wrappers use `plan_path`).
- MCP `wait_for_work` distinguishes two plans with the same filename in different repos (test: spawn the daemon with two repos that both have `.trinity/plans/foo.md`; assert that `wait_for_work({plan_path: ".trinity/plans/foo.md", repo: A})` and the same with `repo: B` produce different matches).
- The `runtime-lock-boundaries`-style ripgrep checks still pass — this rewrite shouldn't reintroduce locks-during-io.
- Moving `.trinity/plans/foo.md` to `.trinity/plans/done/foo.md` keeps the plan's `PlanKey` stable; `Plan.plan_path` updates; existing feedback under `.trinity/feedback/foo/...` keeps targeting the same plan.
- `cargo test` and `cargo clippy --lib --tests` pass.
- `cd frontend && trunk build` succeeds.

## Resolved questions (from the stub's "Open Questions")

- *Active/done canonicalization*: see §2. PlanKey is the stable identity; `Plan.plan_path` holds the current path. No canonicalization back to active path; both are valid current states.
- *Nested plan paths*: out of scope. `PlanKey::from_path` enforces one-level `<stem>.md` under `.trinity/plans/[done/]?`. Nested support is a follow-up plan.
- *Hash-based feedback directory*: rejected. §3 keeps the existing slug-keyed layout because there's no collision to defend against once `PlanKey` is the runtime identity.
- *`~/.trinity/stubs` naming*: non-goal — stubs are pre-Trinity scratch. When a stub becomes a plan it moves into `.trinity/plans/<stem>.md` (see this commit).

## Open Questions

- Should `start_plan` infer `plan_path` from a caller-supplied `slug` for ergonomics, or always require the full path? Leaning **always require** for the schema, with the shim documenting "to start a new plan named `foo`, pass `plan_path: '.trinity/plans/foo.md'`."
- Does the existing `attribution` Walk-back logic need any change when `Session` becomes `Plan`? Probably mechanical; flag if a corner case surfaces during impl.
