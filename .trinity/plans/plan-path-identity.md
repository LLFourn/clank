# Plan Identity (was: Plan Path Identity)

## Revision note (2026-05-15)

The wire identity has been through several drafts:

1. `(repo_root, plan_path)` two-field tuple (approved at 97c3e17).
2. `<canonical_repo_root>:<stem>.md` single string (61c20b7 → ef80833).
3. Sanitized `<canonical_repo_root>:<stem>.md` with `/`→`_`
   (lossy; abandoned).
4. **Current**: `<repo_basename>/<stem>.md` — just the directory name
   of the repo (the one containing `.git`), not the full path.

The full-path variants ran into URL-routing snags (axum wildcards must
be terminal; sub-routes like `/plan/{*plan_id}/revision/{sha}` won't
register), and every encoding scheme was either lossy or ugly.

The basename-only identity sidesteps all of it:

- URLs are two clean segments: `/plan/{repo}/{stem_md}` and
  `/plan/{repo}/{stem_md}/revision/{sha}`. No wildcards, no encoding.
- Two repos with the same basename can't coexist; the second one
  registered is refused. In practice a user keeps one checkout per
  repo and this never collides.
- Active vs done remains plan **state**, not identity.

## Summary

Wire identity is `<repo_basename>/<stem>.md`:

```
trinity/plan-path-identity.md
trinity/leptos-frontend.md
frostsnap/dkg-improvement.md
```

`<repo_basename>` is the directory name of the repo's working-tree
root (the file_name of the path containing `.git`). The daemon
maintains an internal map from basename → canonical absolute path so
lookups resolve quickly.

A plan's identity is **stable across the active↔done move**: the same
`PlanId` value names the plan whether the on-disk file lives at
`.trinity/plans/<stem>.md` or `.trinity/plans/done/<stem>.md`. The
state ("active" / "done") rides on the `Plan` record as a separate
field and is returned to callers but never written into the wire
identity.

Two repos with the same basename are not allowed. The daemon registers
whichever it sees first; later attempts (via `start_plan` or the
`~/.trinity/repos` startup loader) are rejected with
`repo_basename_taken`.

`session_id` and `PlanPath` both retire as public-facing concepts:

- `PlanKey` (the filename stem) stays as the in-repo map key, unchanged.
- `PlanId` (new) is the wire-form `(repo, key)` tuple, serialized as
  `<repo>:<stem>.md`.
- `Plan.plan_path: PathBuf` stays as the daemon's internal handle to the
  current on-disk location, used for `git show` and worktree-status reads.
  It never crosses an API boundary.

## Problem

The two-component `(repo, plan_path)` identity that Phase 2 shipped has
three frictions:

1. **Every API surface needs both halves.** HTTP requires `?repo=` for
   every plan-scoped route. MCP carries `{repo, plan_path}` on every call.
   SSE events emit both. Two-field identity infects every wire shape.
2. **The same plan has two valid `plan_path` values.** When a plan moves
   to `done/`, the URL changes. Callers bookmarking
   `/plan?repo=&path=.trinity/plans/foo.md` get a 404 after the move. The
   `PlanPathMismatch` / `counterpart` resolution logic was added to paper
   over this; it's unreachable through the current grammar and a sign the
   abstraction is misaligned.
3. **The wire surface duplicates filesystem detail.** Every URL has
   `.trinity/plans/` baked into it. Every query string carries the same
   prefix. The actual identifying content — repo + stem — is buried in
   the noise.

The runtime already keys by `PlanKey`. The wire should match.

## Target Model

Identity is one string:

```
PlanId := <repo_basename> "/" <stem> ".md"
```

where:

- `<repo_basename>` is the file_name component of the repo's
  working-tree root — i.e. the name of the directory containing
  `.git`. No path separators (impossible: directory names can't
  contain `/`).
- `<stem>` is the `PlanKey` slug — non-empty, no `/`. Dots inside
  the stem are allowed (`foo.v2`).
- `.md` is fixed and required for parseability.

Examples:

```
trinity/plan-path-identity.md
trinity/leptos-frontend.md
frostsnap/dkg-improvement.md
```

The daemon maintains a basename → canonical absolute path index. When
a `PlanId` is parsed, the daemon splits on the last `/` before
`.md` to recover `(repo_basename, stem)`, looks up the basename in
the index to find the canonical `repo_root`, then looks up the
`PlanKey` in that repo's `plans` map. If the basename isn't in the
index → `unknown_repo`. If the stem isn't in the repo's `plans` →
`unknown_plan` (or `plan_conflict` if it's in `plan_conflicts`).

**Repo registration** (via `start_plan`'s implicit cwd-repo resolution
or the daemon's `~/.trinity/repos` startup loader) canonicalizes the
repo path, takes its file_name as the basename, and refuses to
register if another watched repo already claims that basename
(`repo_basename_taken`). In practice this never collides — users have
one checkout per repo name. When it does, the user resolves it by
renaming a directory.

**State** lives on the `Plan` record:

```rust
pub struct Plan {
    pub id: PlanKey,              // stem; in-repo map key
    pub plan_path: PathBuf,       // current on-disk location (internal)
    pub state: PlanState,         // Active | Done
    // ... rest unchanged
}

pub enum PlanState { Active, Done }
```

`Plan.state` derives from `plan_path` (anything under
`.trinity/plans/done/` is Done; otherwise Active). API responses
include `state: "active" | "done"` and a `current_path` (repo-relative,
e.g. `.trinity/plans/foo.md` or `.trinity/plans/done/foo.md`) so the
UI can render the on-disk location without re-deriving it. The
canonical identity is `PlanId`; `current_path` is display material.

## Goals

- One wire identifier across MCP, HTTP, SSE, log lines: `PlanId`.
- Identity survives the active↔done move. URLs / MCP calls don't break
  when a plan transitions.
- Drop `?repo=` from HTTP entirely. Drop `repo` as a separate input
  field on MCP plan-scoped tools.
- Conflicts (`PlanKey` collisions within a repo) still surface
  explicitly via `plan_conflict` errors and a `conflicts` array in
  `list_plans`.
- No `PlanPathMismatch` variant; no counterpart-acceptance branch. The
  same `PlanId` always resolves to the same plan or to an error.
- `start_plan` still creates `.trinity/plans/<stem>.md` (always
  active). Done paths are produced exclusively by the move-to-done
  lifecycle transition.

## Non-Goals

- No on-disk feedback layout change (still `.trinity/feedback/<stem>/...`).
- No backwards-compat with the prior `session_id` / `plan_path` wire
  shapes. The wire just changes; old clients break.
- No nested plan paths in this phase. `PlanKey::from_path` still rejects
  them; nested support is a follow-up.
- No global cross-repo identity — repo root remains a load-bearing scope.
- No retirement of `Plan.plan_path` as an internal field — the daemon
  still needs to know where the file is on disk.

## Design

### 1. Identity types

In `src/lifecycle.rs`:

```rust
pub struct PlanKey(String);          // unchanged; stem only
pub struct RepoBasename(String);     // file_name of the repo root

/// Wire-form plan identity: `<repo_basename>/<stem>.md`. Two
/// components, no canonicalization. The daemon resolves the basename
/// against its `Trinity.repo_basenames` index to find the canonical
/// `RepoRoot`.
pub struct PlanId {
    repo: RepoBasename,
    key: PlanKey,
}

impl PlanId {
    pub fn new(repo: RepoBasename, key: PlanKey) -> Self { … }
    pub fn parse(s: &str) -> Result<Self, ParsePlanIdError> { … }
    pub fn repo(&self) -> &RepoBasename { &self.repo }
    pub fn key(&self) -> &PlanKey { &self.key }
}

impl fmt::Display for PlanId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}.md", self.repo, self.key)
    }
}

impl serde::Serialize for PlanId { /* via Display */ }
impl serde::Deserialize for PlanId { /* via parse */ }
```

`PlanKey::from_path` stays. `PlanPath` (the public-facing newtype
Phase 1 introduced) **is removed**; the runtime uses `PathBuf` for
internal on-disk-path bookkeeping and `PlanId` for everything that
crosses a boundary.

`PlanKey` grammar from Phase 1 is unchanged. The stem may not contain
`/` (already enforced) and the new wire form has no other separator
ambiguity — the only `/` in `PlanId.to_string()` is between the
basename and the stem.

`PlanId::parse` rules:

1. Find the last `/` in the input.
2. Left of `/` → `RepoBasename`. Reject empty or values containing
   `/` (impossible after split, but defensive).
3. Right of `/` → must end in `.md`; strip the suffix and validate the
   stem against the `PlanKey` grammar.
4. If any step fails, return `ParsePlanIdError` → `invalid_plan_id`.

Note: `PlanId::parse` does **not** touch the filesystem. It validates
the wire form only. The basename-to-canonical-root resolution is a
separate step that happens against `Trinity.repo_basenames` (see §6).

`Trinity` gains a basename index:

```rust
pub struct Trinity {
    pub repos: BTreeMap<RepoRoot, RepoState>,
    pub repo_basenames: BTreeMap<RepoBasename, RepoRoot>,
    pub live_events: VecDeque<LiveEvent>,
}
```

`RepoState` is unchanged: still `BTreeMap<PlanKey, Plan>` per repo, with
`plan_conflicts: BTreeMap<PlanKey, Vec<PathBuf>>` for collisions.

### 1b. `PlanKey` uniqueness invariant + conflict handling

**Unchanged from previous revision.** Within one repo, exactly one
tracked plan file may map to a given `PlanKey`. Collisions land in
`RepoState.plan_conflicts: BTreeMap<PlanKey, Vec<PathBuf>>` (paths are
the raw on-disk repo-relative paths, since they're for diagnostic
display only). The `Plan` is omitted from `plans` until resolved.

MCP `get_context` / `wait_for_work` for a conflicted `PlanKey` returns
a `plan_conflict` error with the list of paths. `list_plans` includes a
`conflicts` array alongside the `plans` array.

### 2. Lifecycle across active/done

The runtime's primary key is `PlanKey`. The plan's `state` (Active /
Done) rides on the `Plan` record. Moving `.trinity/plans/foo.md` to
`.trinity/plans/done/foo.md` updates `Plan.plan_path` and flips
`Plan.state` to `Done`. **The `PlanId` does not change.**

Resolution for an input `PlanId`:

1. Look up `trinity.repo_basenames[plan_id.repo()]`. If absent →
   `unknown_repo` error.
2. Look up `trinity.repos[<repo_root>].plans[plan_id.key()]`. If
   absent: check `plan_conflicts[key]` first and return `plan_conflict`
   with the conflicting paths; otherwise return `unknown_plan`.
3. Return the `Plan`. No counterpart logic, no path comparison.

This is strictly simpler than the previous revision's `resolve_plan`:
the input fully determines lookup; on-disk state determines `Plan.state`
in the response but never alters identity matching.

The `PlanPathMismatch` error variant is removed. The plan-id grammar
doesn't allow two equivalent inputs to refer to the same plan.

### 3. Feedback storage

**Unchanged.** On-disk layout stays:

```
.trinity/feedback/<plan-key>/<phase>/<sha>/<author>.md
```

The directory name is `PlanKey` (the stem). `FeedbackPath.plan_key`
remains the parsed lookup field.

MCP responses keep returning explicit `write_feedback.path` so agents
never compute the dir name themselves.

### 4. MCP shape changes

Every plan-scoped tool takes a single `plan_id` field. `repo` is
removed from every plan-scoped tool — it's encoded in the id.

```json
{
  "plan_id": "trinity/leptos-frontend.md",
  "author_label": "codex"
}
```

Tools affected:

- `start_plan` — accepts `{slug, label}` (and uses the shim's
  cwd-repo). The daemon canonicalizes the cwd-repo, derives its
  basename, registers the repo (refusing if another watched repo
  already claims that basename: `repo_basename_taken`), then creates
  `.trinity/plans/<slug>.md` and rejects when the slug collides with
  an existing plan or a `plan_conflicts` entry. Response:
  `{plan_id, repo, canonical_path, committed, next_step}`. Errors:
  `repo_basename_taken`, `invalid_slug`, `plan_already_exists`,
  `plan_in_conflict`. (`start_plan` is the only tool that takes a
  bare slug — it has cwd context. The others take the full
  `plan_id`.)
- `get_context` — accepts `plan_id`. Errors: `invalid_plan_id`,
  `unknown_repo`, `unknown_plan`, `plan_not_committed`, `plan_conflict`.
  Response: `{plan_id, slug, state: "active"|"done", current_path,
  phase, plan_worktree_status, waiting_on, ...}`. `current_path` is
  the plan's current repo-relative path.
- `wait_for_work` — accepts `plan_id`. `Match`:
  `{plan_id, repo, work, locations}`. `repo` is the canonical absolute
  path; `locations` are repo-relative. `repo` is redundant with
  `plan_id` (the agent could look up the basename) but spelling it
  out lets the caller act on `locations` directly.
- `list_plans` — accepts optional `repo` (filter; the value is a
  basename or a canonical path, both work). Response: `{plans,
  conflicts}`. Each plan row: `{plan_id, slug, state, current_path,
  phase, plan_worktree_status, waiting_on}`. Conflict row:
  `{plan_id, slug, paths}` (the conflicted plan still has one
  `plan_id` since all paths share a stem; `paths` lists every
  conflicting on-disk path).

`start_plan` keeps `label` for the agent-name autofill.

Tool descriptions explicitly call out:

- `plan_id` form is `<repo_basename>/<stem>.md`.
- `state` is a response field; the same `plan_id` works whether the
  plan is currently active or in `done/`.

### 4b. MCP shim cache: `plan_id` is NOT cached

**Unchanged in spirit.** The shim caches `author_label`. It must not
cache `plan_id`. Every plan-scoped call passes its own target
explicitly. Tool description copy: `plan_id is the target of this
call; pass it on every invocation`.

### 5. UI routes + `/api/*`

`PlanId` is two segments (`<repo_basename>`, `<stem>.md`), so routes
use two ordinary path params — no wildcards, no encoding. Sub-routes
work because the wildcard rule doesn't apply.

Routes:

```
GET  /                                                # SPA home
GET  /plan/{repo}/{stem_md}                           # SPA plan detail
GET  /plan/{repo}/{stem_md}/revision/{sha}            # plan rev view
GET  /plan/{repo}/{stem_md}/commit/{sha}              # commit diff
GET  /plan/{repo}/{stem_md}/diff/{from}/{to}          # plan rev-vs-rev
POST /api/plan/{repo}/{stem_md}/done                  # body: {}

GET  /api/plans?repo=<basename>                       # filter; absent = all
GET  /api/plan/{repo}/{stem_md}
GET  /api/plan/{repo}/{stem_md}/revision/{sha}
GET  /api/plan/{repo}/{stem_md}/commit/{sha}
GET  /api/plan/{repo}/{stem_md}/diff/{from}/{to}
```

Example URLs:

```
/plan/trinity/plan-path-identity.md
/plan/trinity/plan-path-identity.md/revision/97c3e17
/api/plan/frostsnap/dkg-improvement.md/commit/abc1234
```

The `{stem_md}` capture is the stem with the trailing `.md`
(`foo.v2.md`, `plan-path-identity.md`). Backend handlers reconstruct
`PlanId` from the two captures and resolve via the
`trinity.repo_basenames` index.

The SPA reads `state` from the response and renders the done badge —
the URL doesn't change when a plan moves to done.

**Repo basename collisions.** Two repos with the same `file_name` of
their canonical path can't both be watched. `start_plan` refuses with
`repo_basename_taken`; the startup loader skips later duplicates with a
WARN log. Naming `~/src/trinity` and `~/work/trinity` is a user
configuration choice; the daemon doesn't try to disambiguate.

Backend handlers (`api_plans`, `api_plan`, etc.) live in
`src/server/http.rs`. The old `/api/sessions*` routes are deleted with
no shim.

### 5b. Live events + SSE

`LiveEvent` carries `plan_id: Option<PlanId>` (None for repo-level
events). The `/events` SSE payload:

```json
{
  "ts": 1715666400,
  "plan_id": "trinity/foo.md",
  "slug": "foo",
  "state": "active",
  "kind": "feedback_changed",
  "payload": null
}
```

Repo-level events (`repo_rebuilt`) emit `plan_id: null`. Plan-scoped
events (`plan_worktree_changed`, `feedback_changed`, `feedback_removed`)
emit `plan_id` + derived `slug` + `state`. The fanout layer captures
`slug` and `state` at emit-time (the runtime has them) rather than
re-parsing on every fan-out.

Tests cover: repo-rebuild (plan_id null); feedback-write on active
plan; feedback-write on done plan; plan_worktree_changed for an
active-to-done transition.

### 6. Internal projections

`src/projection.rs` continues to take `&PlanKey` + the supporting
indexes for `_for_parts` helpers — these never touch the wire form, so
they're unaffected by the rename from `plan_path` to `plan_id` at the
boundary.

`compute_match` in `src/server/wait.rs`:

```rust
fn compute_match(
    runtime: &Runtime,
    plan_id: &PlanId,
    role: WaitingRole,
    author: &AgentLabel,
) -> Result<Option<WorkItem>, WaitError>
```

It uses `plan_id.repo()` to pick the `RepoState`, `plan_id.key()` to
look up the plan, computes status from the plan's current on-disk
`plan_path`, and returns work items keyed off the plan key.

The HTTP/UI response builders (`src/ui_response.rs`, `src/mcp_response.rs`)
take `PlanSnapshotBundle` as today; they emit `plan_id` (computed from
`bundle.root` + `bundle.plan.id`) as the public identifier.

### 7. Compatibility strategy

Breaking change. No migration shim, no fallback to old shapes.

Concrete deltas from what's already shipped:

- MCP tool schemas: `repo` field deleted from plan-scoped tools;
  `plan_path` field renamed to `plan_id` (basename form).
  `additionalProperties: false` rejects the old shape clearly.
- HTTP routes: `/api/sessions*` was retained through Phase 2 as a
  carve-out; this revision deletes them in Phase 3 and replaces
  them with two-segment `/api/plan/{repo}/{stem_md}` routes.
- Frontend: `api.rs` DTOs rebuild around `plan_id`; route structure
  switches from `/sessions/:id` to `/plan/{repo}/{stem_md}`.

The disk layout under `.trinity/feedback/<stem>/...` happens to be
unchanged (the stem is the same), so feedback files written before
this change keep loading.

## Phases

Four phases. Phase 1 (already shipped) is mostly unaffected. Phase 2
(already shipped) is partially superseded — its MCP cutover happens
again with the new identity. Phase 3 is the bulk of the new work.

### Phase 1 — Identity types + core rename + conflict detection (mostly shipped)

Already in master at `bc8efaf` + `e55aa6c`. This revision adds:

- `PlanId` struct in `src/lifecycle.rs` with `parse` / `to_wire` /
  `to_display` / serde.
- Remove `PlanPath` newtype. The internal field on `Plan` becomes
  `plan_path: PathBuf` again (the type was load-bearing only at the
  boundary, which the new design eliminates).
- Add `Plan.state: PlanState` field, derived from `plan_path` at
  rebuild time.
- Tighten `PlanKey` grammar: reject stems containing `:`.

### Phase 2 — MCP + LiveEvent re-cutover (partially shipped, needs rework)

Files: `src/tools.rs`, `src/server/mcp.rs`, `src/server/wait.rs`,
`src/mcp_response.rs`, `src/repo_state.rs` (LiveEvent), `src/server/http.rs`
(SSE).

- Tool schemas: drop `repo` and `plan_path`; require `plan_id` everywhere
  except `list_plans` (which keeps `repo` as a filter).
- Dispatch parses `plan_id`, splits to `(repo, key)`, routes through the
  simplified resolution (no counterpart logic).
- `WaitArgs.plan_id` replaces `WaitArgs.plan_path` + `WaitArgs.repo`.
  New error variants: `InvalidPlanId`, `UnknownRepo`, `UnknownPlan`,
  `PlanConflict`. **`PlanPathMismatch` removed.**
- `LiveEvent.plan_path` → `plan_id: Option<PlanId>` with derived
  `slug` and `state` populated at emit time.
- `/events` payload uses the new shape.
- `mcp_response::get_context_response` drops `session_id` and emits
  `plan_id`, `slug`, `state`. (Addresses the prior reviewer's
  unresolved finding.)

### Phase 3 — HTTP + Leptos route rewrite (new scope)

Files: `src/server/http.rs`, `src/ui_response.rs`, `frontend/src/api.rs`,
`frontend/src/main.rs`, `frontend/src/components/*.rs`, `frontend/src/store.rs`.

- Delete `/api/sessions*` routes; add `/api/plans` + `/api/plan/{*plan_id}*`.
- Backend handlers parse `plan_id` from the URL wildcard, dispatch via
  the same resolve helper used by MCP.
- Conflict response: `/api/plan/{*plan_id}` returns 409 with
  `{error: "plan_conflict", paths: [...]}` when the stem is conflicted.
- Frontend `api.rs` types rename: `SessionRow` → `PlanRow` with
  `plan_id: String`, `slug: String`, `state: "active" | "done"`,
  `phase`, `worktree_status`, `waiting_on`. Same for `PlanDetail`.
- Frontend `store.rs::LiveEvent`: `plan_id: Option<String>` + `slug` +
  `state`. Activity-sidebar deep links built from `plan_id`.
- Leptos router: `/plan/{*plan_id}` and sub-routes. `<MetaStrip>`,
  `<PlanRevision>`, `<CommitDiff>`, `<PlanDiff>` accept `plan_id`
  directly; href construction is single-component.
- Conflict UI: home-row renders a conflict state for any entry in
  `conflicts[]`.

### Phase 4 — Cleanup

- Remove the `SessionId` type alias in `src/lifecycle.rs`.
- Remove the `PlanPath` newtype.
- Update CLAUDE.md and any memory entries that still mention
  `session_id` / `plan_path` as the identity.
- Audit `~/.claude/projects/-Users-llfourn-src-trinity/memory/MEMORY.md`.

## Acceptance Criteria

Each acceptance criterion is scoped to where it should hold. Internal
fields named `plan_path` are explicitly allowed on internal structs
(`Plan`, `PlanFileBlob`, runtime/state snapshots) because the daemon
needs an on-disk handle; the criteria below target the **boundary**:
tool schemas, response builders, the JSON they emit, and the frontend
DTOs that consume them.

Identity-leak checks (acceptance grep is over boundary surfaces only):

- `rg "SessionId" src/` clean outside test fixtures.
- MCP tool schemas in `src/tools.rs` mention `plan_id`; they do NOT
  mention `session_id` or `plan_path` as field names.
- Wire response JSON: `rg '"session_id"' src/mcp_response.rs
  src/ui_response.rs src/server/http.rs src/server/mcp.rs
  src/server/wait.rs` empty. (Match `"session_id"` in quotes to scope
  to JSON keys, not Rust idents.)
- Wire response JSON: `rg '"plan_path"' src/mcp_response.rs
  src/ui_response.rs src/server/http.rs src/server/wait.rs` empty.
  (`src/server/mcp.rs::start_plan` may still emit `"canonical_path"`
  for the creation response; that's a creation-time disk pointer, not
  identity.)
- Frontend boundary: `rg "session_id\|plan_path" frontend/src/api.rs
  frontend/src/store.rs` clean of identity fields; the only
  permissible hit is references to historical `session_id` removal
  comments.

Behavioral acceptance:

- MCP `wait_for_work` distinguishes two plans with the same stem in
  different repos: same stem, different basenames → different
  `plan_id` values → different `Match`es.
- Duplicate `PlanKey` in one repo is detected; MCP calls return
  `plan_conflict`; the corresponding `/api/plan/{repo}/{stem_md}`
  returns 409; `/api/plans` surfaces the conflict row; the UI renders
  the conflict on the home row. Work is NOT routed silently to either
  file.
- Moving `.trinity/plans/foo.md` to `.trinity/plans/done/foo.md`:
  - The `PlanId` is unchanged.
  - URLs built before the move keep working.
  - `Plan.state` flips to `Done`.
  - Existing feedback under `.trinity/feedback/foo/...` still targets
    the same plan.
- The MCP shim does not autofill `plan_id`. An MCP call that omits
  `plan_id` fails at schema validation.
- `start_plan` registers a previously-unknown repo (canonicalizes,
  computes basename, inserts into `Trinity.repos` +
  `Trinity.repo_basenames`, starts the watcher). It rejects:
  invalid slug, stem collides with an existing plan or
  `plan_conflicts` entry, basename collides with another watched
  repo (`repo_basename_taken`).
- The daemon startup loader skips entries in `~/.trinity/repos`
  whose basenames collide, with a WARN log; first entry wins.
- `PlanId::parse` validates the wire form (two segments, `.md`
  suffix, no `/` in stem) without touching the filesystem.
  Filesystem resolution happens in the `repo_basenames` lookup.
- URLs are two path segments: `/api/plan/{repo}/{stem_md}` etc.
  Round-trip: a `PlanId` constructed via `start_plan`, formatted
  into a URL, re-extracted, and looked up returns the same plan.
- `LiveEvent` and `/events` emit `plan_id` (nullable for repo-level
  events) plus derived `slug` + `state`. Frontend `EventStore` and
  activity sidebar consume the new shape.
- Same-`PlanId` regression: a plan moved to `done/` keeps the URL
  reachable through the UI without reload.
- `cargo test` and `cargo clippy --lib --tests` pass.
- `cd frontend && trunk build` succeeds.

## Resolved questions

- *PlanPathMismatch handling*: removed. The wire grammar makes it
  unreachable by construction.
- *URL encoding for plan-scoped routes*: none. `PlanId` is two
  ordinary path segments (`<repo_basename>/<stem>.md`). No wildcards,
  no encoding, sub-routes work.
- *Whether the wire identity carries the full repo path*: no. Just
  the basename. Two repos with the same basename can't both be
  watched; the daemon arbitrarily ignores the later one (first
  registration wins) and logs WARN. Users hit this only with a
  worktree dropped under the same name as its source (e.g.
  `~/wt/trinity` shadowing `~/src/trinity`); they resolve it by
  renaming the worktree directory.
- *`?repo=` ergonomics*: eliminated for plan-scoped routes. Kept only
  on `/api/plans` as an optional filter.
- *Active/done identity drift*: identity is stable across the move;
  state rides on a separate field.
- *Whether on-disk paths appear in responses*: yes for `get_context` /
  `list_plans` as a `current_path` display field. The internal
  `Plan.plan_path` is not serialized directly. Actionable filesystem
  locations (`write_feedback.path`, `wait_for_work.locations`) are
  repo-relative.
- *Should `wait_for_work` include `repo` in its response*: yes —
  redundant-but-helpful so callers can act on `locations` directly.

## Open Questions

None.
