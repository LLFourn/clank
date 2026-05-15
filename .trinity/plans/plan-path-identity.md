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

/// Stable internal identity. Derived from the filename stem. Both
/// `.trinity/plans/foo.md` and `.trinity/plans/done/foo.md` map to the
/// same `PlanKey("foo")` — that's how the active↔done move preserves a
/// plan's lifecycle. This is what the runtime maps key on; it never
/// crosses an API boundary.
pub struct PlanKey(String);

impl PlanKey {
    /// Returns `Some(PlanKey)` only if `path` matches exactly one of:
    ///   `.trinity/plans/<stem>.md`
    ///   `.trinity/plans/done/<stem>.md`
    /// where `<stem>` is non-empty, `.md` is the only extension, and
    /// `<stem>` itself contains no path separators. Nested paths and
    /// non-md files return None.
    pub fn from_path(p: &Path) -> Option<Self> { /* … */ }
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
    /// Plans whose disk state is contradictory at rebuild time (e.g.
    /// the same stem exists at BOTH `.trinity/plans/foo.md` and
    /// `.trinity/plans/done/foo.md`). The rebuild does not pick a
    /// winner; the plan is omitted from `plans` and surfaced here so
    /// API responses can flag the user. See §1b.
    pub plan_conflicts: BTreeMap<PlanKey, Vec<PlanPath>>,
}
```

`Session` is renamed to `Plan` (`pub struct Plan { id: PlanKey, plan_path: PlanPath, body: String, body_hash: ContentHash, plan_intro: CommitSha, ... }`). `AttributionResult::Attributed` carries `PlanKey` instead of `SessionId`.

### 1b. `PlanKey` uniqueness invariant + conflict handling

**Within one repo, exactly one tracked plan file maps to a given `PlanKey`.**

The natural violation is having both `.trinity/plans/foo.md` and `.trinity/plans/done/foo.md` on disk simultaneously — this can happen mid-rename, after a user mistake, or if a new `foo.md` is created after an old `foo.md` was moved to done. Today's code silently "last-write-wins" by HashMap insertion order. The rewrite makes it explicit:

- During rebuild, after every plan file has been hashed and converted to a `Plan` struct, group entries by their `PlanKey`.
- Groups of size 1 land in `RepoState.plans` as before.
- Groups of size ≥2 land in `RepoState.plan_conflicts` as `(PlanKey, Vec<PlanPath>)` and are **omitted** from `RepoState.plans`. No plan with that key is routable until the conflict is resolved (i.e. one of the offending files is moved or deleted).
- MCP `get_context` / `wait_for_work` for a conflicted `PlanKey` returns a `PlanConflict` error with the list of paths. The UI renders the conflict on the home page row with a clear "two files map to the same key, resolve manually" message.

### 2. Lifecycle across active/done

The runtime's primary key is `PlanKey`. The current `plan_path` rides on the `Plan` record as state. Moving `.trinity/plans/foo.md` to `.trinity/plans/done/foo.md` updates `Plan.plan_path` while `Plan.id` stays `PlanKey("foo")`.

**Resolution semantics for a caller-supplied `plan_path`** (used by every MCP / HTTP endpoint that takes one):

1. Compute `PlanKey::from_path(plan_path)`. If `None`, return `InvalidPlanPath` (not a valid path under `.trinity/plans/` or `.trinity/plans/done/`).
2. Look up `RepoState.plans[PlanKey]`. If absent: check `RepoState.plan_conflicts[PlanKey]` first and return `PlanConflict` with the conflicting paths; otherwise return `UnknownPlan`.
3. Compare `requested_path` against the looked-up `plan.plan_path`. If they match, accept. If not, check whether they are active/done counterparts (e.g. requester passed `.trinity/plans/foo.md` but the plan is currently at `.trinity/plans/done/foo.md`) — accept and respond with the canonical current path. Otherwise reject with `PlanPathMismatch { current: <plan.plan_path>, requested: <plan_path> }`.

This makes "same stem, different path" a hard error instead of silent acceptance. The active/done counterpart is the only allowed alias.

Nested plan paths (e.g. `.trinity/plans/team-a/foo.md`) are out of scope for this phase. `PlanKey::from_path` rejects them outright — a request that tries to register one fails at parse time, well before the runtime sees it.

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

- `start_plan` — accepts `plan_path` directly. Drop the auto-construction of `.trinity/plans/<id>.md`. The daemon validates the path against `PlanKey::from_path` (must be `.trinity/plans/<stem>.md`, non-nested) AND explicitly **rejects done paths** (`.trinity/plans/done/<stem>.md`) — done paths are produced by the move-to-done lifecycle transition, not by creation — AND rejects paths whose `PlanKey` already maps to an existing plan (active or done) in the runtime. Error variants: `InvalidPlanPath`, `DonePathNotAllowed`, `PlanAlreadyExists { current_path }`.
- `get_context` — accepts `plan_path` (was `session_id`).
- `wait_for_work` — accepts `plan_path` (was `session_id`). Response `Match.session_id` → `Match.plan_path`. The work-tool description's example loop is updated in lock-step.
- `list_sessions` — **renamed to `list_plans`**, no compatibility alias. The MCP catalog drops the old name cleanly. Hosts that have cached the old catalog will see `unknown tool: list_sessions` and refresh; that's the right failure mode. Response rows emit `plan_path` (and a derived `slug` for display).

`start_plan` keeps `label` for the agent-name autofill (same field as today).

Response shapes:

- `wait_for_work` `Match`: `{ repo, plan_path, reason }` (was `{ repo, session_id, reason }`).
- `get_context`: `plan_path` replaces `session_id` in the response; `slug` is added as a derived display helper. Everything else stays.

Tool descriptions explicitly note that `plan_path` is repo-relative, must match the canonical `.trinity/plans/` or `.trinity/plans/done/` form, and is case-sensitive on the filesystem.

### 4b. MCP shim cache: `plan_path` is NOT cached

`author_label` continues to be cached per-tool because it's the caller's stable identity within a shim lifetime. **`plan_path` must not be cached.** The point of moving identity to the path is to make the target explicit on every call; silently inferring it from a prior call would reintroduce exactly the kind of "which session am I writing feedback to?" ambiguity this plan removes. Agents reviewing multiple plans in one process would otherwise risk writing feedback to the wrong plan after a context switch.

The shim refuses to autofill `plan_path` even if the caller passes a string-typed arg with the same name to a different tool. Every canonical MCP call requires an explicit `plan_path`.

Tool description copy should make this rule visible to the agent: "`plan_path` is the target of this call; pass it on every invocation."

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

### 5b. Live events + SSE

`src/repo_state.rs` defines `LiveEvent { session_id: Option<SessionId>, .. }`. The runtime broadcasts it on `events_tx`; `src/server/http.rs::event_stream` serializes it as JSON for `/events`; the frontend's `EventStore` (`frontend/src/store.rs`) consumes it for live invalidation.

This identity has to migrate in lock-step with everything else, or the SPA's resource keying breaks the first time a `LiveEvent` arrives with a stale `session_id` shape. Specifically:

- `LiveEvent.session_id: Option<SessionId>` becomes `plan_path: Option<PlanPath>`. Repo-level events (e.g. `repo_rebuilt`) keep `plan_path: None`; plan-scoped events (`plan_worktree_changed`, `feedback_changed`, `feedback_removed`) emit the actual path.
- Frontend `LiveEvent` struct in `frontend/src/store.rs` mirrors the new shape.
- Activity-sidebar deep links (`/sessions/{id}`) become `/plan?repo={repo}&path={plan_path}` to match §5.
- `events_tx` typing changes; the broadcast channel was already `tokio::sync::broadcast::Sender<LiveEvent>` so it's just a struct-field rename.
- Tests cover at least: one repo-rebuild event (path = null), one feedback-write event (path = the plan's current path), one plan-worktree-changed event with the active and done paths in turn.

Wire payload after the change:

```json
{
  "ts": 1715666400,
  "repo": "/abs/repo",
  "plan_path": ".trinity/plans/foo.md",
  "slug": "foo",
  "kind": "feedback_changed",
  "payload": null
}
```

The `slug` field is convenience for activity-sidebar rendering; it's `PlanKey.as_str()`.

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

### Phase 1 — Identity types + core rename + conflict detection

Files: `src/lifecycle.rs`, `src/disk_format.rs`, `src/repo_state.rs`, `src/disk_snapshot.rs`, `src/attribution.rs`, `src/rebuild.rs`, `src/runtime.rs`, `src/runtime_snapshot.rs`, `src/projection.rs`.

- Add `PlanPath(PathBuf)` and `PlanKey(String)` newtypes in `lifecycle.rs`.
- Replace `SessionId` with `PlanKey` everywhere it's used as a map key or identity (`RepoState.sessions` → `RepoState.plans: BTreeMap<PlanKey, Plan>`, `AttributionResult::Attributed { session: PlanKey, .. }`, `state.plan_touches` value tuple, `Session.id`, all projection `_for_parts` helpers).
- Rename `Session` to `Plan`, `SessionSnapshot` to `PlanSnapshot`, `SessionSnapshotBundle` to `PlanSnapshotBundle`.
- Add `RepoState.plan_conflicts: BTreeMap<PlanKey, Vec<PlanPath>>` and the rebuild-time detection (§1b).
- Implement the resolution-semantics check in a shared helper (`fn resolve_plan(state, plan_path) -> Result<&Plan, PlanLookupError>`) so all callers go through it.
- Tests: rebuild green on fresh repos; explicit test for "active + done both on disk → plan lands in `plan_conflicts`, not `plans`"; resolution test that `.trinity/plans/foo.md` accepts `.trinity/plans/done/foo.md` as a counterpart but rejects `.trinity/plans/bar.md` with the same stem (impossible by construction but ensure the error variant fires).

### Phase 2 — MCP + wait_for_work + LiveEvent rewrite

Files: `src/tools.rs`, `src/server/mcp.rs`, `src/server/wait.rs`, `src/mcp_response.rs`, `src/mcp_shim/mod.rs`, `src/repo_state.rs` (LiveEvent struct), `src/server/http.rs` (event_stream JSON).

- Replace `session_id` with `plan_path` in every tool schema. `additionalProperties: false` enforces the strict cutover.
- `WaitArgs.session_id` → `WaitArgs.plan_path`. `Match.session_id` → `Match.plan_path`. New error variants: `WaitError::UnknownPlan`, `WaitError::PlanConflict { paths: Vec<PlanPath> }`, `WaitError::PlanPathMismatch { current: PlanPath }`, `WaitError::InvalidPlanPath`.
- Tool descriptions reworded: example loops use `plan_path`. Description copy explicitly says "pass `plan_path` on every call; the shim does NOT cache it."
- Shim cache: `author_label` stays cached; `plan_path` is explicitly not cached per §4b. The shim's `label_arg_for` helper gains no `plan_path` equivalent.
- `list_sessions` removed, `list_plans` added. No alias. Rows emit `{repo, plan_path, slug, phase, worktree_status, waiting_on}`.
- `start_plan`: enforce all four guards from §4 (not nested, not done, no PlanKey collision, valid path).
- `LiveEvent.session_id` → `LiveEvent.plan_path`. The `/events` SSE payload includes `{plan_path, slug, repo, kind, ts, payload}`. Repo-level events emit `plan_path: null`. The event-stream test in `tests/end_to_end.rs` updates assertions.
- Tests: wire tests for the new shapes; collision/mismatch tests for the new error variants; SSE test that a feedback write produces an event carrying the plan's current path.

### Phase 3 — HTTP + leptos route rewrite

Files: `src/server/http.rs`, `src/ui_response.rs`, `frontend/src/api.rs`, `frontend/src/main.rs`, `frontend/src/components/*.rs`, `frontend/src/store.rs`.

- Backend handlers move under `/api/plans*` and `/api/plan*` per §5.
- `frontend/src/api.rs` types rename: `SessionRow` → `PlanRow`, `SessionDetail` → `PlanDetail`. Field names follow.
- `frontend/src/store.rs::LiveEvent.session_id` → `plan_path`. Activity-sidebar deep links use `/plan?repo=&path=...`.
- Leptos router routes get the new shape (`/plan?repo=&path=...` etc.). `<MetaStrip>` etc. build links from `(repo, plan_path)`.
- The leptos plan's deferred `?repo=` URL plumbing lands here as a side effect — the SPA emits `?repo=` because every plan link uses both halves of the identity.
- Conflict UI: the home page list row renders a `PlanConflict` state when `RepoState.plan_conflicts` includes the plan, with the conflicting paths inline.
- Tests: e2e tests update path-based assertions; the wire tests in `src/server/http.rs::wire_tests` keep their structure but use the new URLs.

### Phase 4 — Cleanup

- Remove `SessionId` from `src/lifecycle.rs` (it survives Phase 1-3 only as a type-alias for legacy callers; Phase 4 drops the alias).
- Update CLAUDE.md and any stray prose references to "session" in tool descriptions where the term is now misleading.
- Update memory entries that mention `session_id` as identity (e.g. `mcp-vs-ui-surface-separation.md`).
- Audit `~/.claude/projects/-Users-llfourn-src-trinity/memory/MEMORY.md` for `session_id` references; rewrite or add follow-up notes as needed.

## Acceptance Criteria

- `rg "SessionId" src/` shows no occurrences outside of test-only fixtures.
- `rg "session_id" src/tools.rs src/server src/mcp_response.rs src/ui_response.rs` shows no occurrences (the MCP schemas use `plan_path` and JSON responses key on it too).
- `rg "session_id" frontend/src/` shows no occurrences (component types, fetch wrappers, and `LiveEvent` use `plan_path`).
- MCP `wait_for_work` distinguishes two plans with the same filename in different repos (test: spawn the daemon with two repos that both have `.trinity/plans/foo.md`; assert that `wait_for_work({plan_path: ".trinity/plans/foo.md", repo: A})` and the same with `repo: B` produce different matches).
- Duplicate `PlanKey` in one repo (active + done coexisting on disk) is detected: the plan lands in `RepoState.plan_conflicts`, MCP calls return `PlanConflict`, the UI surfaces the conflict on the home row. Work is NOT routed silently to either file.
- A `plan_path` that resolves to a known `PlanKey` but doesn't match the current `plan_path` or its active/done counterpart fails with `PlanPathMismatch` — it does not silently resolve by stem.
- The MCP shim does not autofill `plan_path`. An MCP call that omits `plan_path` fails at schema validation, not silently.
- `start_plan` rejects `.trinity/plans/done/<stem>.md`, nested paths, and paths whose stem collides with an existing plan.
- `LiveEvent` and `/events` no longer emit `session_id`. They emit `plan_path` (nullable for repo-level events) and a derived `slug`. Frontend `EventStore` and activity sidebar consume the new shape.
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

(All open questions from the original stub are now resolved above — see "Resolved questions". Nothing material left to decide before implementation; surface any new ones via plan revisions during Phase 1.)
