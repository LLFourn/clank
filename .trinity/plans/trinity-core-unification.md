# trinity-core unification

## Summary

Rename `trinity-wire` to `trinity-core`. Split it into two modules:
`trinity_core::model` (the daemon's fold state — pure data, no
projections) and `trinity_core::api` (HTTP/MCP/SSE response DTOs
built from the model). Make the daemon store `model` types directly
instead of parallel `repo_state::Foo` structs. Collapse the two
parallel response modules (`src/ui_response.rs` + `src/mcp_response.rs`,
1458 LOC total) into one `src/responses.rs` that has a single
projection path from `model` to `api`.

Server-side markdown rendering stays as-is. Wire-shape changes
are allowed where they improve the model; each is enumerated in
"Wire-shape changes" below. The deletion is structural, not
behavioral.

Result: ~1000 LOC removed; the silent divergence-bug class between
the two response surfaces (MCP empty `body_html`, MCP-vs-UI
`MultiPlan` handling) becomes impossible because there's one
projection path; the wire crate has a principled split between
"what the daemon stores" and "what crosses the wire."

This is the architectural fix the prior plan
(`purge-stringly-typed.md`) named but did not execute. That plan
unified the *vocabulary* (closed-vocab enums in one place); this
plan unifies the *storage shapes and projection path* without
conflating the two layers.

## Motivation

After `purge-stringly-typed` landed, ruthless caught the leftover
architectural mismatch: `trinity-wire` exists, the frontend imports
its types, the closed-vocab enums are typed end-to-end — and yet
the daemon still hand-writes two parallel translation modules from
domain → wire, with ~80 LOC of byte-identical helpers and ~150
LOC of near-identical projection builders (`plan_page` vs
`get_context_response_from_snapshot`). Two of those builders
already diverge silently in real, observable ways:

- `build_commit_gate`: UI renders `body_html`; MCP inlines
  `String::new()`. An MCP consumer reading `gate.feedback[author]
  .body_html` finds empty strings, with no compile error.
- `build_timeline`: UI `debug_assert!`s on `MultiPlan`-with-gate;
  MCP silently maps `MultiPlan → ReviewTargetPhase::Plan`. Today
  the invariant in `disk_snapshot.rs` keeps both branches dead, but
  the next change to gate-creation rules will produce a silent
  shape disagreement.

The plan called for `impl From<DomainType> for WireType`. There are
zero `impl From<…>` in `src/`. The boundary is two ad-hoc builder
modules.

The deeper cause: the daemon defines storage structs
(`repo_state::WaitingOn`, `repo_state::Feedback`, …) whose only
meaningful differences from the wire crate's `dto::WaitingOn`,
`dto::Feedback` are:

1. Validated newtypes (`AgentLabel`, `PlanKey`, `CommitSha`,
   `RepoBasename`, `ContentHash`) carried as typed values rather
   than `String`.
2. Path types: absolute `PathBuf` vs repo-relative `String`.
3. Server-rendered `body_html` field present on the wire and
   absent on the domain (it's computed at projection time).

(1) is resolved by moving the newtypes into `trinity_core::model`
and serializing them transparently — the wire JSON for a newtype
is identical to a plain `String`.

(2) is resolved by keeping repo-relative `String` paths in the
model, with the daemon resolving absolute at IO time. Today the
daemon already converts `plan_path: PathBuf` to a relative
`String` in every wire builder; removing the round-trip cleans
the code without changing behavior.

(3) is NOT resolved by removing `body_html` from the wire. Server
markdown rendering stays. `body_html` lives on `api::Feedback`
(rendered by the single projection path) and is absent from
`model::Feedback` (the storage shape). Both MCP and UI go through
the same projection, so they cannot disagree.

That leaves the daemon storing `model::Foo` types directly, with
a thin daemon-side wrapper for any genuine runtime state. Endpoint
responses are typed `api::FooResponse` structs built by ONE
projection module from `model` data.

## Hard Direction

After this plan, the following statements are simultaneously true:

- `trinity-core` exists; `trinity-wire` does not.
- `trinity_core::model` contains the daemon's fold state shape —
  identifiers, vocabulary enums, per-plan timeline, gates,
  feedback, lifecycle data. Pure data, serde-derived. No
  projections, no rendered fields, no presentation concerns.
- `trinity_core::api` contains every struct that appears in an
  HTTP response, MCP response, or SSE event payload. Built FROM
  `model` types by one projection module.
- `trinity-core` is data-only: depends on `serde`, optionally
  `serde_json` for tests. No `pulldown-cmark`, no `ammonia`, no
  IO. Wasm-clean (verified by CI).
- The daemon's storage structs that today parallel wire DTOs
  (`repo_state::WaitingOn`, `repo_state::Feedback`,
  `repo_state::ArchivedCycleSummary`, the publishable subset of
  `repo_state::Plan`) are replaced by `pub use trinity_core::model::Foo`
  re-exports.
- Daemon-only runtime state that is genuinely not pure data
  (broadcast channels, file-watcher handles, HEAD-SHA caches)
  stays in the daemon. See "Daemon-only state" below — the actual
  surface is small.
- One module `src/responses.rs` is the sole `model → api`
  projection path. Total ~200 LOC, down from 1458. No `build_*`
  helpers.
- The frontend imports `trinity_core::api` types directly. Wire
  format is stable EXCEPT for the small intentional improvements
  enumerated in "Wire-shape changes" below; e2e tests update only
  where those changes land.
- The two guard tests from `purge-stringly-typed` continue to pass
  with drained allowlists.

## Daemon-only state (the actual surface)

Audit of `repo_state::Plan` (the canonical daemon-side struct):
its fields are all data — `id: PlanKey`, `plan_path: PathBuf`,
`body: String`, `plan_intro: CommitSha`, `timeline:
Vec<PlanTimelineEvent>`, `archived_cycles:
Vec<ArchivedCycleSummary>`, `frozen_at: Option<...>`, etc. Zero
fields hold tokio handles, broadcast senders, or file-watcher
state. Those live on the runtime container (`crate::runtime`,
`crate::server::*`), not on `Plan`.

So the "split `Plan` into `PlanData` + `PlanRuntime`" idea from a
draft of this plan is overstated. The cleaner reality:

- `model::PlanState` (or just `model::Plan`) IS what the daemon
  stores. No wrapper needed today.
- `plan_path` becomes repo-relative `String` (it already is in the
  wire today; just stop converting at the boundary).
- The handful of daemon-side accessors that need absolute paths
  resolve via `repo_root.join(&plan.plan_path)` at IO time. Same
  treatment the feedback-path system uses.

If a future feature needs to commingle runtime state into the
plan struct, introduce a wrapper at that point. Today it would be
an empty seam.

## What goes into `trinity_core::model`

The fold-state types. These are the minimal truth the daemon
computes; the daemon's storage uses them directly.

- Identifiers: `AgentLabel`, `PlanKey`, `CommitSha`, `RepoBasename`,
  `ContentHash`, `PlanId`. All `#[serde(transparent)]` over their
  inner `String`, with `TryFrom<String>` / `FromStr` for
  validate-on-deserialize.
- Enums: `PlanLifecycle`, `Posture`, `WaitingRole`, `WaitingReason`,
  `CommitKind`, `Verdict`, `PlanWorktreeStatus`, `PlanTouchKind`,
  `ReviewTargetPhase`, `CommitGateState`, `ReviewGateState`,
  `ExpectedAction`, `DiffLineKind`. (Already in the wire crate.)
- Structs: `WaitingOn`, `Feedback` (without `body_html` —
  presentation lives on the API side), `CommitGate`, `ArchivedCycle`
  (the publishable summary), `Plan` (the full fold state, see
  audit above), `PlanTimelineEvent`.

The frontend reads `model` types directly when it deserializes
embedded fields of an `api` response — model types appear inside
api shapes via composition.

## What goes into `trinity_core::api`

Endpoint response structs. These are projections — they exist for
the wire, not for storage. The daemon does not store them.

- Top-level responses: `ListPlansResponse`, `GetContextResponse`,
  `PlanDetailResponse`, `PlanRevisionResponse`,
  `CommitDetailResponse`, `DiffResponse`, `RepoListResponse`,
  `WaitForWorkResponse`, `DeleteRepoOutcome`, etc.
- Projection-only structs: `PlanRow`, `CommitRow`, `ReviewGate`
  (the legacy plan/impl-tagged shape), `PrHint`, `PrHintOption`,
  `WriteFeedback`, `ReviewTarget`, `TimelineEvent` (the wire
  shape — distinct from `model::PlanTimelineEvent`).
- The "rendered" wire `Feedback` shape: an `api::Feedback`
  carries `body_html` (rendered server-side from
  `model::Feedback.body`). The projection module is the only
  renderer.

`CommitRow` and the prior `CommitRowDetail` collapse into one
`api::CommitRow` carrying `feedback: Vec<api::Feedback>`. The
MCP `commits[].feedback` shape gains `body_raw` + `body_html`
fields that today only the UI carries (`FeedbackSummary` had
`{author, verdict}` only). **This is an intentional wire-shape
improvement** — see "Wire-shape changes" below. `FeedbackSummary`
is deleted from `trinity-core` entirely; there is no consumer of
the summary-only shape now that body rendering is centralized
and the size delta is small.

## Wire-shape changes (enumerated)

Wire-shape changes are allowed in this plan where they improve
the model. The complete set:

1. **MCP `commits[].feedback` carries full feedback bodies.** Today
   MCP returns `FeedbackSummary { author, verdict }`; after this
   plan it returns the same `api::Feedback { author, verdict,
   body_raw, body_html, path, created_at }` shape the UI gets.
   Reason: the parallel-builder bug class came directly from MCP
   and UI having different per-commit feedback shapes. Collapsing
   to one shape eliminates the bug class structurally. The size
   delta is small (one extra render per feedback, which the daemon
   already does for the UI path; the bytes are markdown + sanitized
   HTML). `FeedbackSummary` is deleted from `trinity-core`.

2. **MCP `gate.feedback[author].body_html` is non-empty.** Today
   the MCP builder inlines `body_html: String::new()`; after the
   collapse to one projection path, body_html is rendered for
   both surfaces by the same function. This is an intentional
   correction of the silent-divergence bug ruthless flagged.

3. **`PrHintOption.name: String` → `kind: PrHintOptionKind` enum.**
   Closed vocabulary becomes typed (Phase 6). On the wire the
   field is renamed `name` → `kind` so the discriminator name
   matches the type. Frontend's `_ => "Option"` fallback goes away.

4. **MCP error responses get a typed envelope** (Phase 6). The five
   ad-hoc `json!({"error": "...", ...})` shapes in
   `src/server/mcp.rs` become variants of `api::McpErrorPayload`
   (`#[serde(tag = "error", rename_all = "snake_case")]`). The
   shapes are identical to today's `json!` output; the change is
   that they're now produced by typed constructors. Schema stable;
   producer typed.

Every other wire shape is preserved byte-identically. The Phase 0
snapshots split into two files per endpoint: a **schema snapshot**
(field names + types; must be stable except for changes 1, 3, and
4 above, which update the schema baseline once) and a **value
regression snapshot** (golden outputs from a fixture state; must
match exactly except for change 2 above, which updates one value).

## Rules

The plan changes structure, not semantics. The following invariants
must hold at every phase boundary:

- Tests pass at every phase. No phase leaves the workspace red.
- The guard tests from `purge-stringly-typed` continue to pass.
  Their allowlists are expected to shrink as parallel definitions
  go away — that's the success signal, not a regression.
- Wire shape is preserved EXCEPT for the four changes enumerated
  in "Wire-shape changes" above. Any other shape change is a bug
  in the migration; the Phase 0 schema snapshots catch it.
- Validated newtypes (`AgentLabel`, `PlanKey`, …) retain their
  validation. Moving them to `trinity-core::model` does NOT mean
  weakening their constructors.
- `trinity-core` stays wasm-clean and data-only. No `tokio`,
  `axum`, `git2`, `pulldown-cmark`, `ammonia`, no IO. Only
  `serde` (and `serde_json` in tests).
- Server-side markdown rendering stays. `pulldown-cmark` and
  `ammonia` remain daemon-side dependencies. The renderer lives
  in `src/responses.rs` as part of the projection step.
- Daemon-only runtime state (`Runtime`, `Trinity`, broadcast
  channels, watcher handles) stays daemon-side. `trinity-core` is
  unaware of it.

## Implementation Phases

### Phase 0: De-risk and verify

Before any rename, run two checks:

- `cargo check -p trinity-wire --target wasm32-unknown-unknown` —
  confirm the current wire crate is wasm-clean, so the rename
  doesn't reveal a latent issue.
- `cargo test --workspace` to capture the baseline. Note any
  flaky tests so they don't get blamed on the migration.

Snapshot every endpoint's current JSON output into
`tests/wire_snapshots/`, split into two files per endpoint:

- `<endpoint>.schema.json` — field-presence + types only (e.g.
  via `serde_json::Value` walked to a structural skeleton). This
  is the schema baseline. It updates ONCE per enumerated
  wire-shape change (#1, #3, #4 in "Wire-shape changes"); any
  other diff is a migration bug.
- `<endpoint>.values.json` — golden output for a deterministic
  fixture state. Must match byte-identically except for the
  `body_html` correction (change #2) on MCP commit-gate output.

After Phase 5 (and Phase 6 for the renames), the schema files
update to the new baseline once and stay frozen; the value files
update once for the `body_html` correction and stay frozen.

### Phase 1: Rename `trinity-wire → trinity-core`

Pure rename. No restructure.

- `crates/trinity-wire/` → `crates/trinity-core/`
- `Cargo.toml` package name, `lib.rs` module docs.
- Every `use trinity_wire::…` → `use trinity_core::…` across the
  workspace.
- `tests/wire_contract_guards.rs` paths.

The internal module structure (`dto`, `vocab`) stays as-is in
Phase 1 — Phase 5 introduces the `model` / `api` split. Phase 1
is a pure file/symbol rename.

After Phase 1: workspace builds, all tests pass, wire snapshots
unchanged, no functional change.

### Phase 2: Move validated newtypes into `trinity-core`

- Move `AgentLabel`, `PlanKey`, `CommitSha`, `RepoBasename`,
  `ContentHash`, `PlanId` from `src/lifecycle.rs` to a new
  `trinity-core::ids` module.
- Apply `#[serde(transparent)]` over the inner `String`, with
  `TryFrom<String>` / `FromStr` for validate-on-deserialize.
- Re-export from `src/lifecycle.rs` so existing daemon imports
  keep working without churn.
- Add round-trip tests in `trinity-core/tests/`: validation
  failures, snake-case stability where applicable, wire
  transparency (`serde_json::to_value(&AgentLabel("alice".into()))` is
  `"alice"` not `{"0":"alice"}`).

**Wire-version skew note:** newtype validation now runs on every
frontend fetch. A daemon bug emitting an invalid value fails the
whole response deserialize. The frontend's existing `FetchError`
path already distinguishes `Decode(e)` from network errors — add
an acceptance criterion that decode failures surface a visible
error banner rather than silently rendering nothing. (Today the
fetch wrappers in `frontend/src/api.rs` return `FetchError`; the
component error-handling already shows a `<p class="error">`
banner in the loading combinators. Verify and pin.)

After Phase 2: the daemon uses the new newtypes; on the wire they
appear as plain strings (no JSON shape change). Snapshots
unchanged.

### Phase 3: Move publishable structs into `trinity-core` (still as one module)

Phase 3a — types where daemon and wire shapes are identical once
newtypes move:

- `WaitingOn`: today wire has `agents: Vec<String>`, daemon has
  `Vec<AgentLabel>`. With newtypes in core, define
  `trinity_core::WaitingOn { agents: Vec<AgentLabel>, ... }`.
  Serde-transparent agents produce identical wire JSON. Drop the
  daemon copy in `repo_state.rs`; `pub use trinity_core::WaitingOn`.
- `ArchivedCycle` (was `ArchivedCycleSummary` daemon-side): same
  treatment.

Phase 3b — `Plan` and `Feedback`:

- `Feedback`: define `trinity_core::Feedback` WITHOUT `body_html`
  (matches the daemon's storage shape). The wire's
  `body_html`-bearing variant gets renamed temporarily to
  `WireFeedback` and continues to live in the dto module; Phase 5
  formalizes the model/api split. The daemon stores
  `trinity_core::Feedback` directly. The response builders'
  `body_html` field is populated at projection time as today.
- `Plan`: move the publishable subset of `repo_state::Plan` into
  `trinity_core::Plan`. Per the audit, this is all of `Plan`'s
  current fields (with `plan_path: String` repo-relative instead
  of `PathBuf`). Daemon code that does `repo_root.join(&plan.plan_path)`
  for IO continues to work.

After Phase 3: daemon-side `Plan`, `Feedback`, `WaitingOn`,
`ArchivedCycle` are all `pub use trinity_core::Foo` re-exports.
Wire snapshots are unchanged through Phase 3 — the daemon-side
projection still adds `body_html` at the wire boundary and the
`CommitRow` shape change has not happened yet.

### Phase 4: Introduce the `model` / `api` split

This is the architectural step that addresses codex's review.

- Inside `trinity-core/src/`, restructure into:
  - `trinity_core::model` — `ids`, vocab enums, `Plan`,
    `WaitingOn`, `Feedback` (raw, no `body_html`), `CommitGate`,
    `ArchivedCycle`, `PlanTimelineEvent`, all fold-state types.
  - `trinity_core::api` — all top-level response structs
    (`ListPlansResponse`, `PlanDetailResponse`, etc.) and
    projection-only structs (`PlanRow`, `CommitRow`, `PrHint`,
    `TimelineEvent`, `ReviewGate`, etc.). `api::Feedback` adds
    `body_html`.
- The `dto` module from Phase 1 dissolves; everything moves into
  one of the two new modules.
- `trinity_core` lib.rs re-exports `model::*` and `api::*` at
  the crate root for short import lines where unambiguous.
- Frontend imports `trinity_core::api::*` for response shapes.
  Where it matches on model-side enums it imports `trinity_core::model::*`
  explicitly.

After Phase 4: the model/api boundary is explicit in the crate.
No daemon code change yet — `src/ui_response.rs` and
`src/mcp_response.rs` still exist, still construct `api` types.

### Phase 5: Collapse `ui_response.rs` + `mcp_response.rs` into `responses.rs`

The point of this plan. With `model` types stored by the daemon
and `api` types projected from them in one place:

- New module `src/responses.rs` (or `src/responses/mod.rs` with
  per-endpoint files if it grows past ~300 LOC).
- One function per HTTP endpoint and per MCP tool that returns a
  response shape. Each function takes a borrow of the daemon's
  `model`-typed state and returns an `api`-typed response.
- The shared projection helpers (`build_waiting_on`,
  `build_archived`, `posture_to_review_target_phase`, `build_pr_hint`,
  `build_review_gate`, `build_commit_gate`, `build_timeline`)
  become free functions in `responses.rs`. Exactly one copy of
  each.
- `body_html` rendering is one function in `responses.rs`. Both
  MCP and UI projection paths call it. The current divergence (MCP
  emits empty string) becomes structurally impossible.
- `build_timeline`'s `MultiPlan`-with-gate handling lands one way
  (use the UI's stricter `debug_assert!` shape — it's the
  invariant-respecting choice).
- Delete `src/ui_response.rs` and `src/mcp_response.rs`.

`GetContextResponse` and `PlanDetailResponse` stay as **distinct
explicit response types** (per both reviewers). They share
substructures (most fields are the same composing types from
`api`), but their top-level identities are explicit. The
projection module has two functions, one per response; the
helpers used inside are shared. No `Option<...>` shared struct
with endpoint-dependent fills.

After Phase 5: ~1458 LOC of two parallel modules becomes ~200-300
LOC of one projection module. The Phase 0 schema snapshots
register two intentional updates (CommitRow collapse, the value
correction on MCP `body_html`); every other endpoint's shape and
values are unchanged.

### Phase 6: Fix the residual stringly leaks ruthless flagged

Two non-architectural leaks remain from the prior plan; clean up
here because they share the "promote to enum in `trinity-core`"
motion:

- **`PrHintOption.name: String`**: closed vocabulary
  (`keep_plan_in_pr` / `exclude_plan_from_pr`). Promote to
  `model::PrHintOptionKind` enum. Frontend matches on the enum,
  not the string. The `_ => "Option"` fallback in
  `pr_hint_card.rs` goes away.
- **MCP error envelope payloads** (`src/server/mcp.rs`): five
  distinct error shapes built via `json!` with an `error:` closed
  discriminator. Replace with a `trinity_core::api::McpErrorPayload`
  tagged enum (`#[serde(tag = "error", rename_all = "snake_case")]`)
  and a `StartPlanResponse` typed struct. The Guard A "allowed
  envelope" count in `src/server/mcp.rs` drops from 10 to the
  small handful that actually IS the open-vocabulary tool-dispatch
  envelope.

### Phase 7: Widen the guards; verify clean

- Replace the cap-based per-vocab needle list in Guard B with a
  **positive allowlist** of approved `String` fields. Maintain
  `trinity-core/tests/approved_string_fields.rs` (or similar)
  that names every `pub <ident>: String` in `trinity_core::model`
  + `trinity_core::api`. The guard scans for any unlisted
  `String` field on a `pub struct` and fails. Adding a new
  string-typed field requires a deliberate addition to the
  allowlist with a one-line justification (e.g. "subject — open
  vocabulary commit message"). Cap-based guards rot; explicit
  allowlists force conscious decisions at PR time.
- Match-needle widening: include `match <ident> {` patterns
  against string literals adjacent to wire vocabularies, not just
  `.as_str() {`. Catches the `pr_hint_card.rs::label_for(name: &str)`
  failure mode.
- Pin `trinity-core` as wasm32-clean in CI:
  `cargo check -p trinity-core --target wasm32-unknown-unknown`.
  Make it CI-blocking, not pre-commit (pre-commit can be skipped).

After Phase 7: the guards catch the failure modes that previously
required ruthless review to spot.

### Phase 8: Final audit + `kind_str` cleanup

- Delete `kind_str()` methods on `RepoEventPayload` /
  `PlanEventPayload`. Replace with a hand-written `Display` impl
  on each payload enum that delegates to the variant's snake_case
  discriminator. Frontend's `live_event_kind_str` becomes
  `format!("{}", payload)`. Daemon tests use `matches!()` for
  pattern-based assertions (more readable, rename-safe). The
  `Display` impl is one match arm per variant — same drift surface
  as `kind_str` had, but the Display approach keeps the
  discriminator string in one place per type, which makes future
  serde-tag drift obvious.
- Confirm guard A and guard B allowlists contain only the
  legitimately documented exceptions:
  - `src/mcp_shim/mod.rs` — JSON transport between processes; not
    a domain producer.
  - `src/tools.rs` — input-schema JSON is open-vocabulary by design.
  - `src/server/mcp.rs` — tool dispatch (open vocab of tool names).
    Phase 6's typed `McpErrorPayload` reduces the rest.
- Add divergence-bug regression tests (codex's pattern of
  pinning invariants):
  - Test that `body_html` is byte-identical for the same
    `CommitGate` rendered through both response paths. Today
    impossible because there's only one path; pin it.
  - Test that an `Engineered::MultiPlan`-with-gate fixture (use a
    custom builder to construct the state) produces identical
    `TimelineEvent` output for `get_context_response` and
    `plan_detail_response`. Pin the property by construction.
- Document the canonical pattern in the plan footer: domain
  storage IS `model`; wire shape IS `api`; one projection module
  is the boundary; rendering happens at projection time.

## Testing

- `cargo test --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo fmt -- --check`
- `cd frontend && trunk build`
- `cargo check -p trinity-core --target wasm32-unknown-unknown`
- The existing wire-contract guards (`tests/wire_contract_guards.rs`)
  continue to pass; their allowlists shrink phase-by-phase.
- The Phase 0 schema snapshots match across the migration except
  for the three schema updates enumerated in "Wire-shape changes"
  (CommitRow collapse, PrHintOption rename, McpError envelope);
  the value snapshots match except for the `body_html` correction
  on MCP commit-gate output.
- The Phase 8 divergence-property tests pin that the structural
  unification eliminates the bug class.

## Acceptance Criteria

- `trinity-wire` is renamed `trinity-core`.
- `trinity_core::model` contains the daemon's fold-state types
  (ids, enums, `Plan`, `Feedback`, `WaitingOn`, `CommitGate`,
  `ArchivedCycle`, `PlanTimelineEvent`). Pure data, no rendered
  fields. The daemon stores these types directly — no parallel
  `repo_state::Foo` for any of them.
- `trinity_core::api` contains response DTOs and projection-only
  structs (`api::Feedback` with `body_html`, `PlanRow`,
  `CommitRow`, `PrHint`, `TimelineEvent`, `ReviewGate`, all
  top-level `*Response` shapes).
- `trinity-core`'s `Cargo.toml` depends on `serde` (and tests on
  `serde_json`). No `pulldown-cmark`, no `ammonia`, no IO crates.
  Wasm-clean per CI gate.
- `src/ui_response.rs` and `src/mcp_response.rs` are deleted. One
  `src/responses.rs` (or directory) contains the sole `model →
  api` projection path, with shared free-function helpers used by
  both HTTP and MCP endpoints.
- The total LOC in the daemon's response-building code is under
  ~300, down from 1458.
- Wire shape changes match the enumerated set in "Wire-shape
  changes": Phase 0 schema snapshots update once for items 1 / 3
  / 4 and value snapshots update once for item 2; no other
  per-endpoint diffs exist.
- `PrHintOption.name` is a typed enum on the wire.
- MCP error envelope payloads in `src/server/mcp.rs` are typed
  variants of `api::McpErrorPayload`.
- Guard B uses a positive allowlist of approved `String` fields,
  not a cap-based per-needle scan.
- Two divergence-property regression tests pin the unification:
  `body_html` byte-identical across MCP and UI for same gate;
  `TimelineEvent` byte-identical across the two response paths
  for `MultiPlan`-with-gate fixture.
- `kind_str()` is replaced by `Display` on the payload enums.
- Newtype validation on the wire is documented; the frontend's
  fetch-error path surfaces decode failures visibly.

## Non-Goals

- No transport change. Still JSON over HTTP, SSE, and MCP.
- No wire-shape changes BEYOND the four enumerated in "Wire-shape
  changes". Any other diff is a migration bug.
- No move of markdown rendering off the daemon. Server-side
  rendering stays. `body_html` continues to appear on wire
  `Feedback` shapes. (A separate plan may revisit this once the
  structural unification is in place.)
- No new endpoints, no new commands, no semantic changes to the
  agent loop or freeze rules.
- No removal of newtype validation.

## Out Of Scope

- Reworking `Plan.timeline` semantics.
- Changing the feedback verdict vocabulary.
- Replacing the MCP framing protocol with something else.
- Adding a graphical diff renderer or richer markdown extensions.
- Moving markdown rendering to the client (see Non-Goals; reserve
  for a later intentional wire-contract change).

If a structural cleanup exposes a latent bug — e.g. the
`build_commit_gate` / `build_timeline` divergence both reviewers
flagged — fix that bug. Otherwise this plan is purely a structure
pass.
