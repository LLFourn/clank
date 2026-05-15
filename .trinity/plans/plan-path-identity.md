# Plan Identity (was: Plan Path Identity)

## Revision note (2026-05-15)

Earlier revisions of this plan (97c3e17 etc.) defined the wire identity as the
two-component tuple `(repo_root, repo_relative_plan_path)`. That choice was
made to keep the on-disk file path visible at every API boundary.

Lloyd is strongly convinced the wire identity should instead be a single
string of the form

```
<repo_root>:<stem>.md
```

with active vs done as plan **state**, not part of the identity. The filename
of this plan stays `plan-path-identity.md` for git-history continuity, but
the design below has been rewritten around this revised identity. The repo
root is still the load-bearing scope qualifier — there is no global plan
namespace — but `repo:plan.md` is the single thing that names a plan
everywhere outside the daemon's internal maps.

The shipped Phase 1 / Phase 2 commits (`bc8efaf`, `e55aa6c`, `f4b6e10`,
`a9e1ae9`) are partially superseded; the in-flight Phase 3 work is replaced
by the Phase 2-revised + Phase 3-revised described below.

## Summary

Replace Trinity's two-component `(repo, plan_path)` wire identity with a
single colon-joined string:

```
<repo_root>:<stem>.md
```

Examples:

```
/Users/llfourn/src/trinity:plan-path-identity.md
/Users/llfourn/src/trinity:leptos-frontend.md
~/src/trinity:plan-path-identity.md            (display shorthand; same identity)
```

A plan's identity is **stable across the active↔done move**. The same
`PlanId` value names the plan whether the on-disk file is at
`.trinity/plans/<stem>.md` or `.trinity/plans/done/<stem>.md`. The state
("active" / "done") rides on the `Plan` record as a separate field and is
returned to callers but never written into the wire identity.

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
PlanId := <repo_root> ":" <stem> ".md"
```

where:

- `<repo_root>` is a canonical absolute path to the repo's working-tree
  root (the same value used as `Trinity.repos` map keys today).
- `<stem>` is the `PlanKey` slug — non-empty, no `/`, no `:`. Dots inside
  the stem are allowed (`foo.v2`).
- `.md` is fixed and required for parseability.

Examples:

```
/Users/llfourn/src/trinity:plan-path-identity.md
/Users/llfourn/src/trinity:leptos-frontend.md
/Users/llfourn/src/trinity-worktree:leptos-frontend.md   ← different identity
```

**On input** the daemon accepts either canonical or `~`-shorthand
(`~/src/trinity:foo.md`) and normalizes via `$HOME` expansion +
`dunce::canonicalize`. Display layers compress `$HOME` back to `~`.

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
`.trinity/plans/done/` is Done; otherwise Active). API responses include
both `state` and a `current_path` field so callers can render the move
state without parsing the path.

## Goals

- One wire identifier across MCP, HTTP, SSE, log lines: `PlanId`.
- Identity survives the active↔done move. URLs / MCP calls don't break
  when a plan transitions.
- Drop `?repo=` from HTTP entirely. Drop `repo` as a separate field on
  MCP plan-scoped tools. The repo is already inside the `PlanId`.
- Conflicts (`PlanKey` collisions) still surface explicitly; the
  identity is well-defined but resolution may return a `PlanConflict`
  error with both file paths listed.
- No `PlanPathMismatch` variant; no counterpart-acceptance branch. The
  same `PlanId` always resolves to the same plan or to a conflict.
- `start_plan` still creates `.trinity/plans/<stem>.md` (always active);
  the input is a `PlanId` whose stem becomes the filename. Done paths
  are produced exclusively by the move-to-done lifecycle transition.

## Non-Goals

- No on-disk feedback layout change (still `.trinity/feedback/<stem>/...`).
- No automatic migration of historical feedback or git commits.
- No nested plan paths in this phase. `PlanKey::from_path` still rejects
  them; nested support is a follow-up.
- No global cross-repo identity — repo root remains a load-bearing scope.
- No retirement of `Plan.plan_path` as an internal field — the daemon
  still needs to know where the file is on disk.

## Design

### 1. Identity types

In `src/lifecycle.rs`:

```rust
pub struct PlanKey(String);             // unchanged; stem only

/// Wire-form plan identity: `<canonical_repo>:<stem>.md`. Constructed
/// from a `(repo, key)` tuple or parsed from a wire string. Display
/// helper folds `$HOME` to `~`.
pub struct PlanId {
    repo: PathBuf,    // canonical absolute path
    key: PlanKey,
}

impl PlanId {
    pub fn new(repo: impl Into<PathBuf>, key: PlanKey) -> Self { … }
    pub fn parse(s: &str) -> Result<Self, ParsePlanIdError> { … }
    pub fn repo(&self) -> &Path { &self.repo }
    pub fn key(&self) -> &PlanKey { &self.key }
    /// Canonical wire form (absolute repo).
    pub fn to_wire(&self) -> String { format!("{}:{}.md", self.repo.display(), self.key) }
    /// Display form (folds $HOME to ~).
    pub fn to_display(&self) -> String { … }
}

impl serde::Serialize for PlanId { /* serializes to_wire */ }
impl serde::Deserialize for PlanId { /* via parse */ }
```

`PlanKey::from_path` stays. `PlanPath` (the public-facing newtype Phase 1
introduced) **is removed**; the runtime uses `PathBuf` for internal
on-disk-path bookkeeping and `PlanId` for everything that crosses a
boundary.

`PlanKey` grammar tightens: the stem **must not contain `:`** so the
wire form is unambiguously parseable by splitting on the last `:`
before the trailing `.md`. (Adds one rejection test; otherwise the grammar
is unchanged from Phase 1.)

`PlanId::parse` rules:

1. Accept input ending in `.md`. Strip the suffix.
2. Find the last `:` in the remainder.
3. Left of `:` → repo. Expand leading `~/` to `$HOME`. Apply
   `dunce::canonicalize`. (Fall back to the raw path if canonicalize
   fails, same way today's repo arg falls back.)
4. Right of `:` → stem. Validate against `PlanKey` grammar.
5. If any step fails, return `ParsePlanIdError` (the daemon maps to
   `invalid_plan_id` over MCP / `400 Bad Request` over HTTP).

`RepoState` is unchanged: still `BTreeMap<PlanKey, Plan>` per repo, with
`plan_conflicts: BTreeMap<PlanKey, Vec<PathBuf>>` for collisions. The
runtime never sees a full `PlanId` internally — the dispatch layer
splits `PlanId.repo()` to pick the `RepoState` and uses `PlanId.key()`
to look up within it.

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

1. Look up `RepoState` by `plan_id.repo()`. If absent (repo not loaded)
   → `unknown_repo` error.
2. Look up `repo_state.plans[plan_id.key()]`. If absent: check
   `plan_conflicts[key]` first and return `plan_conflict` with the
   conflicting paths; otherwise return `unknown_plan`.
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
  "plan_id": "/abs/repo:leptos-frontend.md",
  "author_label": "codex"
}
```

Tools affected:

- `start_plan` — accepts `plan_id`. Daemon parses → `(repo, key)`,
  enforces `state == active` (the input grammar can't express `done/`,
  so this is implicit), creates the file at
  `<repo>/.trinity/plans/<stem>.md`. Reject when a plan with the same
  key already exists or sits in `plan_conflicts`.
  Response: `{plan_id, canonical_path, committed, next_step}`. Errors:
  `invalid_plan_id`, `plan_already_exists`, `plan_in_conflict`.
- `get_context` — accepts `plan_id`. Errors: `invalid_plan_id`,
  `unknown_repo`, `unknown_plan`, `plan_not_committed`, `plan_conflict`.
  Response: `{repo, plan_id, slug, state: "active"|"done", phase,
  plan_worktree_status, waiting_on, ...}`. **No `plan_path` field**;
  the on-disk path is internal.
- `wait_for_work` — accepts `plan_id`. `Match`:
  `{plan_id, work, locations}` (locations stay repo-relative).
- `list_plans` — accepts optional `repo` (still a useful filter when
  watching multiple repos). Response: `{plans: [...], conflicts: [...]}`.
  Each plan row: `{plan_id, slug, state, phase, plan_worktree_status,
  waiting_on}`. Conflict row: `{repo, slug, paths}` (paths are
  repo-relative for display).

`start_plan` keeps `label` for the agent-name autofill.

Tool descriptions explicitly call out:

- Input form: `<repo>:<stem>.md` (canonical) or `~/<…>:<stem>.md`
  (shorthand).
- Equality is on canonical form.
- `state` is a response field; the same `plan_id` works whether the
  plan is currently active or in `done/`.

### 4b. MCP shim cache: `plan_id` is NOT cached

**Unchanged in spirit, renamed in detail.** The shim caches
`author_label`. It must not cache `plan_id`. Every plan-scoped call
passes its own target explicitly. Tool description copy:
`plan_id is the target of this call; pass it on every invocation`.

The shim auto-prefixes the cwd-repo for ergonomic shortcut forms? **No
auto-prefix** — the wire requires the full form, no shortcuts. If
agents want shorthand they type `~/repo:foo.md` themselves; the daemon
canonicalizes. This keeps the wire grammar single-form and the shim
purely transport.

### 5. UI routes + `/api/*`

```
GET  /                                # SPA home
GET  /plan/{*plan_id}                 # SPA plan detail
GET  /plan/{*plan_id}/revision/{sha}  # SPA plan rev view
GET  /plan/{*plan_id}/commit/{sha}    # SPA commit diff
GET  /plan/{*plan_id}/diff/{from}...{to}  # SPA plan rev-vs-rev
POST /api/plan/{*plan_id}/done        # body: {}

GET  /api/plans?repo=<repo>           # repo filter; absent = all watched repos
GET  /api/plan/{*plan_id}
GET  /api/plan/{*plan_id}/revision/{sha}
GET  /api/plan/{*plan_id}/commit/{sha}
GET  /api/plan/{*plan_id}/diff/{from}...{to}
```

Axum's `{*plan_id}` wildcard captures the colon-joined string verbatim
including the leading `/` from absolute paths. URLs are long but legal
and unambiguous:

```
/plan//Users/llfourn/src/trinity:plan-path-identity.md
/plan/~/src/trinity:leptos-frontend.md
```

The leading `//` (slash from the route + slash from the absolute repo)
is parsed correctly by axum and most browsers; we serialize tilde
shorthand in links the SPA generates to keep URLs reasonable in
practice.

For active/done state in URLs: there is no `done/` segment in the URL.
The same URL works for both states. The SPA reads `state` from the
detail response and renders a "done" badge accordingly.

Backend handlers (`api_plans`, `api_plan`, etc.) live in
`src/server/http.rs`. The old `/api/sessions*` routes are deleted with
no shim.

### 5b. Live events + SSE

`LiveEvent` carries `plan_id: Option<PlanId>` (None for repo-level
events). The `/events` SSE payload:

```json
{
  "ts": 1715666400,
  "plan_id": "/abs/repo:foo.md",
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

Same posture as Phase 2: breaking internal model change, no migration
shim, no compatibility code.

Concrete deltas from what's already shipped:

- MCP tool schemas: `repo` field deleted from plan-scoped tools;
  `plan_path` field renamed to `plan_id`. `additionalProperties: false`
  rejects the old shape clearly.
- HTTP routes: `/api/sessions*` was retained through Phase 2 as a
  carve-out; this revision deletes them in Phase 3.
- Frontend: `api.rs` DTOs rebuild around `plan_id`; route structure
  switches from `/sessions/:id` to `/plan/{*plan_id}`.
- The wire form is the canonical absolute repo + stem. The frontend
  composes hrefs in canonical form (or `~`-shorthand for display).

The disk layout under `.trinity/feedback/<stem>/...` is unchanged; no
file moves needed.

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

- `rg "SessionId" src/` clean outside test fixtures.
- `rg "session_id" src/tools.rs src/server src/mcp_response.rs src/ui_response.rs`
  empty.
- `rg "PlanPath" src/` clean (replaced by `PlanId` + `PathBuf`).
- `rg "plan_path" src/server src/mcp_response.rs src/ui_response.rs
  src/server/wait.rs src/server/mcp.rs frontend/src/api.rs
  frontend/src/store.rs` empty (the on-disk path no longer crosses any
  boundary; the daemon's internal `Plan.plan_path` is the only
  acceptable hit).
- MCP `wait_for_work` distinguishes two plans with the same filename
  in different repos: same stem, different repo roots → different
  `plan_id` values → different `Match`es.
- Duplicate `PlanKey` in one repo is detected; MCP calls return
  `plan_conflict`; `/api/plan/{*plan_id}` returns 409; `/api/plans`
  surfaces the conflict row; the UI renders the conflict on the home
  row. Work is NOT routed silently to either file.
- Moving `.trinity/plans/foo.md` to `.trinity/plans/done/foo.md`:
  - The `PlanId` is unchanged.
  - URLs built from the previous active path keep working.
  - `Plan.state` flips to `Done`.
  - Existing feedback under `.trinity/feedback/foo/...` still targets
    the same plan (the key is stable).
- The MCP shim does not autofill `plan_id`. An MCP call that omits
  `plan_id` fails at schema validation, not silently.
- `start_plan` rejects: invalid `plan_id`, paths whose stem collides
  with an existing plan, paths whose stem is in `plan_conflicts`.
- `LiveEvent` and `/events` emit `plan_id` (nullable for repo-level
  events) plus derived `slug` + `state`. Frontend `EventStore` and
  activity sidebar consume the new shape.
- Same-`PlanId` regression: a plan moved to `done/` keeps the URL
  reachable through the UI without reload (state field flips on the
  resource invalidation).
- `cargo test` and `cargo clippy --lib --tests` pass.
- `cd frontend && trunk build` succeeds.

## Resolved questions

- *PlanPathMismatch handling*: removed. The single-form wire grammar
  makes it unreachable by construction. (Previously partial: variant
  existed but never fired.)
- *URL aesthetics for absolute repo paths*: accepted. Long URLs are
  the tradeoff for a single-component identity. SPA emits `~`-shorthand
  in generated links; user-typed absolute paths also work.
- *`?repo=` ergonomics*: eliminated for plan-scoped routes. Kept only
  on `/api/plans` as an optional filter when the daemon is watching
  multiple repos.
- *Active/done identity drift*: identity is stable across the move;
  state rides on a separate field. The `counterpart` helper is
  internal-only or removed.

## Open Questions

- *Whether to keep `~`-shorthand support on input*. Pro: nicer for
  hand-written calls and URLs. Con: two input forms means more parse /
  canonicalize surface. **Tentative answer**: keep it; the daemon
  already canonicalizes via `dunce` so the marginal complexity is
  small.
- *URL escaping in browsers*: `:` is a sub-delim in RFC 3986 and is
  allowed in path segments; tested in Chrome / Firefox / Safari but
  worth verifying once Phase 3 lands.
- *Backwards-compat for already-recorded feedback*: feedback under
  `.trinity/feedback/<stem>/...` already keys by stem only and is
  unaffected by this revision. No migration needed.
