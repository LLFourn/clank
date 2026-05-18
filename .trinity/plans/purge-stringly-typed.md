# Purge Stringly Typed Wire Contracts

## Summary

Trinity treats too many known shapes as dynamic JSON. Public response
builders return `serde_json::Value`; closed-vocabulary fields like
`kind`, `state`, `phase`, `expected_action`, `waiting_on.reason`, and
verdict are `String` on the wire and at boundaries; the frontend
re-defines server response structs by hand; tests assert by indexing
raw JSON. This is the wrong model for a Rust codebase with a small
closed domain vocabulary.

JSON remains the external transport. The serialized form stays
human-readable snake_case strings. But inside Rust — across BOTH the
daemon and the WASM frontend — closed vocabularies are enums,
response bodies are typed structs, and the daemon and frontend share
those types through a single workspace crate.

This is not a gentle cleanup. The goal is that wire drift and state
vocabulary drift fail at compile time. A rename like
`finished -> closed` or `commit_needs_review -> needs_review` MUST
fail to build everywhere it matters, not surface at runtime as blank
panels and stale timelines.

## Motivation

The current cycle delivered the event-log fold and the timeline-
first-class model. Late in that cycle, a concrete bug exposed the
deeper problem this plan addresses:

- The backend emitted `"finalize_snapshot": null` on the negative
  branch of a parallel-fields response (`feedback` and
  `finalize_snapshot`, one always empty).
- The frontend DTO declared `Vec<FinalizeApproval>` with
  `#[serde(default)]`. Default covers missing, not null.
- Every non-finalize commit page on the live daemon rendered
  "Failed to load: invalid type: null" until ruthless caught it.

No test caught it because no test round-tripped wire JSON through
the frontend struct. `cargo test` was green. clippy was clean.
The wire shape contract was upheld by convention. Convention
failed.

The structural fix — the one that makes this class of bug
impossible — is `#[serde(tag = "kind")]` tagged enums where the
response shape varies by kind, plus shared types across producer
and consumer. That fix is the substance of this plan.

## Problem

Symptoms in the current codebase:

- `src/mcp_response.rs` builds MCP payloads via `json!` over
  `serde_json::Value`.
- `src/ui_response.rs` builds UI API payloads via `json!`.
- `src/server/http.rs` routes return `axum::Json<Value>` for
  responses with known shape.
- `src/repo_state.rs` carries `LiveEvent` payloads as
  `serde_json::Value`.
- `src/server/mcp.rs` collapses typed tool responses to `Value`
  earlier than the protocol envelope requires.
- `frontend/src/api.rs` duplicates response shapes the daemon
  already owns.
- Production code branches on serialized strings — e.g.
  `match reason.as_str() { "commit_needs_review" => ... }` instead
  of `match reason { WaitingReason::CommitNeedsReview => ... }`.
- Tests assert by `v["field"]` indexing, so producer/consumer
  drift surfaces only at runtime in the browser.

These are not separate problems. They are one problem: the wire
shape is enforced by reader-side conventions, not by the type
system.

## Hard Direction

Add ONE workspace crate that owns the closed-vocabulary enums AND
the public response DTO structs:

```text
crates/trinity-wire/
  Cargo.toml
  src/lib.rs
```

Both the daemon (`trinity`) and the frontend (`trinity-frontend`)
depend on this crate. There is exactly one definition of each
closed-vocabulary enum in the whole repo. There is exactly one
definition of each public response body in the whole repo.

The wire crate's only dependency is `serde`. No `tokio`, `axum`,
`leptos`, runtime state, git IO, or daemon internals. It must
compile to wasm32 unchanged (the frontend lives in WASM).

```toml
serde = { version = "1", features = ["derive"] }
```

### What goes in `trinity-wire`

Two kinds of types:

1. **Closed-vocabulary enums.** Every enum whose variants are a
   small fixed set the daemon and frontend both branch on:
   `PlanLifecycle`, `Posture`, `WaitingRole`, `WaitingReason`,
   `WorkAction` (or its tag), `CommitKind`, `Verdict`,
   `ReviewTargetPhase`, `PlanWorktreeStatus`, repo/plan/diff event
   kinds, `PlanTouchKind`. All derive `Serialize, Deserialize,
   PartialEq, Eq, Clone, Copy, Debug, Hash` with
   `#[serde(rename_all = "snake_case")]`.

2. **Public response bodies and their composing structs.**
   `GetContextResponse`, `ListPlansResponse`, `PlanDetailResponse`,
   `CommitDetailResponse`, `PlanRevisionResponse`,
   `DiffResponse`, `RepoListResponse`, `WaitForWorkResponse`,
   `LiveEvent`, plus the structs they compose (`ReviewGate`,
   `CommitGate`, `Feedback`, `TimelineEvent`, `FinalizeApproval`,
   `PrHint`, `ArchivedCycle`, etc.).

### What does NOT go in `trinity-wire`

- **Validated-newtype identifiers.** `PlanKey`, `CommitSha`,
  `AgentLabel`, `RepoBasename`, `ContentHash`. These carry parser
  invariants the frontend doesn't enforce. On the wire they are
  plain `String`; the daemon parses on the way in and serializes
  on the way out. The closed-vocabulary rule applies to
  ENUMS, not to all string-ish things.

- **Domain structs that are never serialized.** `Plan`,
  `PlanTimelineEvent`, `RepoState`, `Trinity`, `FoldCarry`. These
  carry `PathBuf`, mutable scratch state, and methods. The wire
  crate is data; the daemon's domain crate is logic.

- **The boundary between domain and wire is a `From<DomainType>
  for WireType` impl** — or just a constructor `WireType::from(...)`
  — in the daemon's response-building module.

### Tagged enums where shape varies by kind

This is the load-bearing pattern this plan introduces. Anywhere
the response carries kind-dependent fields, use a tagged enum:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TimelineEvent {
    CommitPlan {
        sha: String,
        subject: String,
        plan_touch: PlanTouchKind,
    },
    CommitImpl {
        sha: String,
        subject: String,
    },
    CommitMixed {
        sha: String,
        subject: String,
        plan_touch: PlanTouchKind,
    },
    CommitMultiPlan {
        sha: String,
        subject: String,
    },
    CommitFinalize {
        sha: String,
        subject: String,
        approvals: Vec<FinalizeApproval>,
    },
    Review {
        target: String,
        author: String,
        verdict: Verdict,
        phase: ReviewTargetPhase,
    },
}
```

Same rule for `CommitDetailResponse` (the response that broke in
the finalize_snapshot null bug):

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitDetailResponse {
    pub plan_id: String,
    pub commit_sha: String,
    pub subject: String,
    pub message_body: String,
    pub diff_files: Vec<FileDiff>,
    pub kind: CommitKind,
    pub payload: CommitDetailPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CommitDetailPayload {
    PlanOnly { feedback: Vec<Feedback> },
    CodeOnly { feedback: Vec<Feedback> },
    Mixed { feedback: Vec<Feedback> },
    MultiPlan {},
    Finalize { snapshot: Vec<FinalizeApproval> },
}
```

The frontend's `match payload { ... }` is exhaustive at compile
time. The null bug becomes unrepresentable.

## Rules

Production code MUST NOT contain:

```rust
// String comparison on closed vocabularies:
match reason.as_str() {
    "commit_needs_review" => ...
    _ => ...
}

// json! body assembly for known response shapes:
json!({ "state": lifecycle.as_str(), "kind": kind.as_str(), ... })

// Value indexing in production paths:
if response["lifecycle"] == "finished" { ... }

// kind: String / state: String / phase: String in public DTOs:
pub struct PlanRow {
    pub state: String,
    pub phase: String,
}
```

Production code MUST use:

```rust
match reason {
    WaitingReason::CommitNeedsReview => ...
    WaitingReason::AddressCommitChanges => ...
}

let resp = GetContextResponse { lifecycle, kind, ... };

if response.lifecycle == PlanLifecycle::Finished { ... }

pub struct PlanRow {
    pub lifecycle: PlanLifecycle,
    pub phase: Posture,
}
```

`as_str()` may remain for log messages, file paths, panic strings,
and serde rename targets. It must not be the way production
control flow branches on domain state.

## What To Purge

Concrete sites to delete or convert (non-exhaustive — Phase 1's
inventory is the authoritative list):

| Pattern | Action |
|---|---|
| `json!({...})` building a public response body | Replace with typed struct construction |
| `axum::Json<Value>` on a known-shape route | Replace with `axum::Json<T>` |
| `serde_json::Value` return type on a response builder | Replace with the typed response struct |
| `v["field"]` indexing in production code | Replace with field access on a typed deserialization |
| `kind: String` / `state: String` / `phase: String` on a DTO | Replace with the enum |
| `match s.as_str() { "..." => ... }` for closed vocabularies | Replace with `match enum { ... }` |
| `frontend/src/api.rs` duplicating server response structs | Delete; import from `trinity-wire` |
| `payload: serde_json::Value` on `LiveEvent` / `RepoEvent` / `PlanEvent` | Replace with typed payload enum |
| Test assertions via raw `Value` indexing | Replace with typed deserialization + enum match |

Allowed exceptions (must be small, named, and audited):

- MCP protocol envelope fields that ARE genuinely dynamic (tool
  call args, error envelope payloads). Tool args parse into typed
  structs immediately; the envelope itself stays `Value` only at
  the outermost layer.
- Tool input schema JSON documents (or replace with a typed
  schema builder — separate scope).
- Golden JSON tests whose explicit purpose is wire-shape
  compatibility.
- Debug endpoints documented as arbitrary JSON.
- Test fixtures that intentionally exercise malformed JSON
  (negative-path round-trip tests like the
  `null_finalize_snapshot_fails_to_decode` test added in
  `dd1b8f6`).

Even allowed exceptions stay out of core domain logic.

## Implementation Phases

### Phase 1: Inventory + Guard Test

Output is two things:

1. **A guard test** that fails CI when production response modules
   gain new `json!` or `serde_json::Value` usage outside an
   allowlist. The allowlist starts at "every current site"; phases
   2-7 drain it. Implementation: a `#[test]` that greps the
   workspace for the offending tokens and asserts the count
   matches the allowlist length. The point is that NEW dynamic-
   JSON sites land with a CI failure and a justification, not
   silently.

2. **A short audit document** classifying each existing site:
   public response builder / protocol envelope / schema document
   / runtime event payload / test-only / debug-only. This drives
   the conversion order in phases 3-6.

No behavior changes in Phase 1.

### Phase 2: Add `trinity-wire`

- Create `crates/trinity-wire/` as a workspace member.
- Define the closed-vocabulary enums (with serde derives).
- Define DTOs for: list plans, get context, wait for work, plan
  detail, plan revision, commit detail (with the
  `CommitDetailPayload` tagged enum), repo list, diff page, review
  gate, commit gate, feedback, timeline event (tagged enum),
  finalize approval, archived cycle, PR hint, live event (tagged
  enum), plus the SSE event shape.
- Add `serde` round-trip tests for representative DTOs including
  tagged-enum payloads.
- Tests: every closed-vocabulary enum serializes as snake_case;
  every tagged-enum variant round-trips; missing required fields
  fail to decode.

The daemon and frontend do NOT depend on this crate yet — just
build and test it.

### Phase 3: Domain enum unification

This is the architectural step that defines whether we have one
source of truth or two.

- Re-route the daemon's domain enums (`PlanLifecycle`, `Posture`,
  `WaitingRole`, `WaitingReason`, `CommitKind`, `Verdict`,
  `PlanWorktreeStatus`, `PlanTouchKind`) to the definitions in
  `trinity-wire`. Either re-export via `pub use trinity_wire::*`
  in `src/repo_state.rs`, or delete the daemon's parallel
  definitions and update imports to point at the wire crate.
- Keep the daemon's enum-specific impl blocks (helpers like
  `CommitKind::is_reviewable()`, `PlanLifecycle::from_plan(&Plan)`)
  in the daemon — `trinity-wire` stays free of domain logic.
- Validated-newtype identifiers (`PlanKey`, `CommitSha`,
  `AgentLabel`) stay in `trinity::lifecycle`. They surface as
  `String` on the wire.

After Phase 3, the workspace has ONE definition of each closed-
vocabulary enum, used by both daemon domain code and wire
serialization.

### Phase 4: Convert MCP responses

- Convert `src/mcp_response.rs` builders to construct typed DTOs
  from `trinity-wire`, returning the typed struct up to the final
  MCP envelope serialization point.
- `wait_for_work`, `list_plans`, `get_context`: typed end-to-end
  internally; only the outermost MCP envelope serializes to
  `Value`.
- Update MCP tests to deserialize bodies into `trinity-wire`
  structs and assert enum variants.
- Drain the relevant entries from Phase 1's allowlist.

### Phase 5: Convert UI/HTTP responses

- Convert `src/ui_response.rs` to construct typed DTOs.
- Convert HTTP handlers in `src/server/http.rs` from
  `axum::Json<Value>` to `axum::Json<T>`.
- Commit detail, finalize snapshot, plan revision, diff, repo
  list, home dashboard data, force-action responses all use the
  shared typed DTOs.
- Replace route tests' JSON-indexing assertions with typed
  deserialization. Keep a narrow set of golden-shape tests where
  external compatibility matters.
- Drain the relevant entries from Phase 1's allowlist.

### Phase 6: Convert frontend DTO usage

- Add `trinity-frontend`'s dependency on `trinity-wire`.
- Delete duplicated response shapes from `frontend/src/api.rs`.
  Anything that's in `trinity-wire` is imported; only frontend-
  local types (`FetchError`, view models that compose multiple
  responses for rendering) stay in the frontend crate.
- Replace UI control flow that branches on serialized strings
  with enum matches.
- The existing DTO round-trip tests in `frontend/src/api.rs`
  (added in `dd1b8f6`) move with the types or are reproduced
  against the wire crate's types.
- Drain the relevant entries from Phase 1's allowlist.

### Phase 7: Convert SSE + runtime live events

- Replace `payload: serde_json::Value` on `RepoEvent` / `PlanEvent`
  with typed payload enums in `trinity-wire`.
- Runtime carries typed events; the SSE serializer is the only
  place that converts them to JSON for the HTTP boundary.
- Frontend's live-event handler matches on typed event variants.
- Unknown/malformed events fail visibly in development (a
  deserialize error logged + ignored) rather than silently
  becoming no-ops.
- Drain the relevant entries from Phase 1's allowlist.

### Phase 8: Prune + audit

- Delete `as_str()` helpers and constructors that only existed
  for `json!` assembly.
- The Phase 1 allowlist should now be near-empty. Document each
  remaining entry with a short reason.
- Update `CLAUDE.md` (or equivalent codebase guidance) so future
  contributors know production response paths return typed DTOs,
  not `Value`.

## Testing

- `cargo test` (workspace)
- `cargo clippy --all-targets -- -D warnings`
- `cargo fmt -- --check`
- `env -u NO_COLOR trunk build` from `frontend/`
- Targeted contract tests:
  - **Tagged-enum round-trip**: every variant of every
    `#[serde(tag = "kind")]` enum round-trips through serde.
  - **Snake-case discriminators**: every closed-vocabulary enum
    serializes to the documented snake_case wire string. Pinned
    by a single test per enum.
  - **Decode-failure regression**: representative malformed
    payloads (null on a required field, missing kind on a tagged
    enum, unknown variant string) fail to decode. The
    `null_finalize_snapshot_fails_to_decode` test from `dd1b8f6`
    is the template.
  - **Rename guard**: a test that pins the wire string for
    each enum variant explicitly, so a developer who renames
    `WaitingReason::CommitNeedsReview -> NeedsReview` (which
    silently changes the wire string from `commit_needs_review`
    to `needs_review`) gets a failing test pointing at the
    contract.
  - **Domain enum identity**: the daemon's domain enum and the
    wire enum are the SAME type after Phase 3. This is a
    compile-time invariant; no runtime test needed.

## Acceptance Criteria

- `crates/trinity-wire` exists, depends only on `serde`, compiles
  to wasm32, and is consumed by both `trinity` and
  `trinity-frontend`.
- `src/mcp_response.rs` and `src/ui_response.rs` no longer return
  `serde_json::Value` for known response bodies. Their return
  types are concrete DTOs from `trinity-wire`.
- All public HTTP handlers in `src/server/http.rs` return
  `axum::Json<T>` for a concrete `T`. The exceptions are
  documented and small.
- The closed-vocabulary enums are defined ONCE, in `trinity-wire`.
  The daemon's `repo_state` and friends import them; there are
  no parallel `WaitingReason` / `CommitKind` / `PlanLifecycle` /
  etc. definitions.
- No production code branches on serialized strings for closed
  vocabularies. The Phase 1 guard test enforces this for new
  code.
- `frontend/src/api.rs` imports shared wire contracts; the only
  remaining frontend-defined types are view models built FROM the
  wire types, plus frontend-local infrastructure (`FetchError`,
  fetch wrappers).
- Runtime live events carry typed payloads. The SSE boundary is
  the only conversion point.
- A rename like `WaitingReason::CommitNeedsReview ->
  WaitingReason::NeedsReview` fails to compile across the daemon
  AND frontend wherever the variant is matched, and fails the
  wire-string pinning test at the renamer's commit.
- Response tests deserialize typed structs and assert on enum
  variants, not string equality.

## Non-Goals

- No transport change. Trinity still speaks JSON over HTTP, SSE,
  and MCP.
- No TypeScript, no JS build step, no OpenAPI, no codegen.
- No database or filesystem migration.
- No change to feedback file body format.
- No reworking of event-log semantics or freeze rules.
- No new agent runner, plugin system, or auth model.

If a type cleanup exposes a direct bug (e.g. a wire-shape
contradiction the type system surfaces), fix that bug. Otherwise
this plan is purely a typing pass.

## Out Of Scope

- Reworking `Plan.timeline` or any event-log semantics.
- Changing the feedback verdict vocabulary.
- Adding a graphical diff renderer or a richer markdown pipeline.
- Replacing MCP's input-schema JSON with a typed schema builder
  (a future plan).
