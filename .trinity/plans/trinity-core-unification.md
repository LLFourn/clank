# trinity-core unification

## Summary

Rename `trinity-wire` to `trinity-core`. Move the daemon's pure
sans-IO domain types into it. Make those types implement
`Serialize`/`Deserialize` so they are simultaneously the in-memory
storage shape AND the on-the-wire shape. The daemon and the frontend
both consume the SAME structs — no parallel definitions, no
domain→wire translation step.

Result: `src/ui_response.rs` (618 LOC) and `src/mcp_response.rs`
(840 LOC) collapse to thin endpoint adapters; the duplicated
`build_*` helpers vanish; the silent divergence-bug class between
the two response surfaces becomes structurally impossible.

This is the architectural fix the prior plan
(`purge-stringly-typed.md`) named but did not execute. That plan
unified the *vocabulary* (closed-vocab enums in one place); this
plan unifies the *shapes* (response DTOs and the domain structs
they project from are the SAME structs).

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

The plan said "the boundary between domain and wire is a
`From<DomainType> for WireType` impl." There are zero `impl From<…>`
in `src/`. The boundary is two ad-hoc builder modules.

The deeper cause is the false model itself: "we need to translate
because the wire shape is different from the domain shape." Most of
the time it isn't different. The wire crate has `WaitingOn {
agents: Vec<String>, ... }`; the daemon has `WaitingOn { agents:
Vec<AgentLabel>, ... }`. The wire crate has `Feedback { path:
String, body_html: String, ... }`; the daemon has `Feedback {
path: PathBuf, body: String, ... }`. The differences are:

1. Validated newtypes (`AgentLabel`, `PlanKey`, `CommitSha`) flattened
   to `String` for serde.
2. Server-side projection caching (`body_html` pre-rendered).
3. Absolute `PathBuf` vs repo-relative `String`.

All three can be resolved structurally:

1. Move the newtypes into `trinity-core`, derive
   `Serialize`/`Deserialize` so they wire-serialize as strings while
   the daemon keeps validation.
2. Move HTML rendering to the WASM client — markdown is small, the
   pulldown-cmark dependency is wasm-clean, and rendering at the
   consumer is more honest about who owns presentation.
3. Replace absolute `PathBuf` with repo-relative `String` for paths
   that appear in both domain and wire; the daemon resolves to
   absolute at IO time.

Once those are resolved, `trinity-core::Feedback` IS the daemon's
storage `Feedback` AND the wire's response `Feedback`. One struct.

Daemon-only runtime state (`Plan`'s file-watcher handles, HEAD-SHA
caches, broadcast channels) stays in the daemon as a thin wrapper:

```rust
// trinity-core
pub struct PlanData { /* what was both repo_state::Plan and wire::PlanRow */ }

// daemon
pub struct Plan {
    pub data: trinity_core::PlanData,
    pub runtime: PlanRuntime, // file watchers, caches, etc.
}
```

Most of the daemon doesn't need the runtime wrapper; it operates on
`PlanData` directly. Endpoints serialize `PlanData` directly.

## Hard Direction

After this plan, the following statements are simultaneously true:

- `trinity-core` exists; `trinity-wire` does not.
- Every struct that appears in an HTTP response, MCP response, or
  SSE event payload lives in `trinity-core`. The daemon's storage
  uses these types directly — no parallel `repo_state::Foo` for any
  wire-shaped `Foo`.
- The daemon's response modules together total ~150 LOC, down from
  1458. They contain no `build_*` helpers; they construct
  `core::Foo` structs directly from the daemon's stored `core::Foo`
  data plus the few daemon-only fields (e.g. `plan_body_html` ↔
  `plan_body_raw`, derived `expected_action`, etc.).
- No `impl From<&domain::Foo> for wire::Foo` exists. There is
  nothing to translate.
- The frontend renders markdown locally; `Feedback.body_html` does
  not appear on the wire.
- Daemon-only fields on `Plan` (watcher handles, caches) live on a
  `PlanRuntime` struct held alongside `PlanData`, not interleaved
  inside it.
- Newtypes (`AgentLabel`, `PlanKey`, `CommitSha`, `RepoBasename`,
  `ContentHash`) live in `trinity-core`. They serialize transparently
  as strings; the daemon parses-once at ingest and uses the typed
  form everywhere internally.
- The two guard tests from the prior plan continue to pass with
  drained allowlists — no regression.

## Problem (what's wrong today)

After `purge-stringly-typed`:

- **Two response modules**: `src/ui_response.rs` (618 LOC) and
  `src/mcp_response.rs` (840 LOC) are 95% the same code with
  different signatures. Helpers `build_waiting_on`, `build_archived`,
  `posture_to_review_target_phase` are byte-identical. `build_pr_hint`,
  `build_review_gate`, `build_commit_gate`, `build_timeline` are
  near-identical with one or two divergence points.
- **Parallel struct definitions for the same data**: `repo_state::WaitingOn`
  vs `trinity_wire::dto::WaitingOn` differ only in `agents:
  Vec<AgentLabel>` vs `Vec<String>`. `repo_state::Feedback` vs
  `trinity_wire::dto::Feedback` differ only in path type and the
  pre-rendered `body_html`. The daemon-side `Plan` struct interleaves
  data-shape fields with runtime-only fields (timeline cache, etc.)
  that obscure which parts are publishable.
- **Two silent divergence bugs** already in shipping code:
  `body_html` empty on MCP, `MultiPlan`-with-gate handling differs
  between surfaces.
- **`From` impls do not exist**: the plan called for them; the code
  delivered ad-hoc `build_*` functions.
- **Markdown rendering is duplicated**: the daemon imports
  `pulldown-cmark` (~80 KB of dep) AND a sanitization pass; the
  frontend then re-parses the result as HTML to mount it. Move the
  rendering to where the rendered HTML is consumed.

## Rules

The plan changes structure, not semantics. The following invariants
must hold at every phase boundary:

- Tests pass at every phase. No phase leaves the workspace red.
- The guard tests from `purge-stringly-typed` continue to pass.
  Their allowlists are expected to shrink as parallel definitions
  go away — that's the success signal, not a regression.
- Wire format is BYTE-COMPATIBLE across the rename. The serialized
  JSON for every response shape must be unchanged. Existing e2e
  tests catch this.
- Markdown rendering relocation does NOT change the rendered HTML.
  The current pipeline (pulldown-cmark + `ammonia` sanitizer) ships
  to wasm verbatim. A round-trip test pins a few representative
  feedback bodies so daemon-side and wasm-side produce identical
  output during the migration window.
- Validated newtypes (`AgentLabel`, `PlanKey`, …) retain their
  validation. Moving them to `trinity-core` does NOT mean weakening
  their constructors.
- `trinity-core` stays wasm-clean. No `tokio`, `axum`, `git2`, FS
  IO. Only `serde`, `pulldown-cmark`, `ammonia`. Optional `chrono`
  if needed for timestamp types — but i64 unix seconds is preferred.
- Daemon-only runtime state stays daemon-side. `trinity-core` does
  not know about file watchers, broadcast channels, or HEAD SHAs.

## What Becomes `trinity-core`

The crate currently named `trinity-wire`, renamed. After the
migration, it contains:

**Newtypes** (moved from `src/lifecycle.rs`):
- `AgentLabel`, `PlanKey`, `CommitSha`, `RepoBasename`, `ContentHash`,
  `PlanId` — all implement `Serialize`/`Deserialize` as transparent
  strings via `#[serde(transparent)]` over `String` plus a
  `validate-on-deserialize` impl.

**Domain + wire structs** (collapse from daemon + wire):
- `PlanData` (was: `repo_state::Plan` minus runtime fields, plus
  what `wire::PlanRow` and `wire::PlanDetailResponse` need).
- `Feedback` — single definition. `body_html` removed; the wasm
  client renders. `path: String` (repo-relative).
- `CommitGate`, `WaitingOn`, `ArchivedCycle`, `PrHint`, `ReviewGate`,
  `TimelineEvent`, `CommitRow`, `CommitRowDetail`, `Feedback`,
  `FinalizeApproval`, `CommitDetail`, `LiveEvent`,
  `RepoEventPayload`, `PlanEventPayload`, `PlanConflict`, etc. —
  one definition each.

**Closed-vocab enums** (already there from purge-stringly-typed):
- `PlanLifecycle`, `Posture`, `WaitingRole`, `WaitingReason`,
  `CommitKind`, `Verdict`, `PlanWorktreeStatus`, `PlanTouchKind`,
  `ReviewTargetPhase`, `CommitGateState`, `ReviewGateState`,
  `ExpectedAction`, `DiffLineKind`.

**Top-level response DTOs**:
- `ListPlansResponse`, `GetContextResponse`, `PlanDetailResponse`,
  `PlanRevisionResponse`, `CommitDetailResponse`, `DiffResponse`,
  `RepoListResponse`, `WaitForWorkResponse`, etc.

`GetContextResponse` and `PlanDetailResponse` collapse: they share
~90% of their fields. The remaining 10% (plan body raw markdown,
commits summary vs detail) becomes a single response type that
endpoint adapters project from. See Phase 3.

## What Stays Daemon-Only

- `RepoState` — holds `BTreeMap<PlanKey, Plan>` plus repo-level
  metadata. Daemon container.
- `Plan` — the runtime wrapper: `{ data: PlanData, runtime:
  PlanRuntime }` where `PlanRuntime` holds bookkeeping the daemon
  needs but the wire doesn't.
- `Trinity` — the global daemon state.
- All git walk / commit attribution code.
- All file-watcher / broadcast machinery.
- The MCP envelope (`ToolCallRequest`/`ToolCallResponse`) and tool
  dispatch — these stay typed but daemon-private.
- The `RepoEvent`/`PlanEvent` daemon-side variant that carries
  `repo: RepoRoot` (PathBuf) for routing. SSE serializer drops the
  routing field; what reaches the wire is `trinity_core::LiveEvent`
  (no PathBuf).

## Implementation Phases

### Phase 1: Rename `trinity-wire → trinity-core`

Pure rename. No behavior changes.

- `crates/trinity-wire/` → `crates/trinity-core/`
- `Cargo.toml` package name, `lib.rs` module docs.
- Every `use trinity_wire::…` → `use trinity_core::…` across the
  workspace.
- `tests/wire_contract_guards.rs` paths and crate references.

After Phase 1: workspace builds, all tests pass, no functional change.

### Phase 2: Move validated newtypes into `trinity-core`

- Move `AgentLabel`, `PlanKey`, `CommitSha`, `RepoBasename`,
  `ContentHash`, `PlanId` from `src/lifecycle.rs` to
  `trinity-core::ids` (or a similar module).
- Add `#[serde(transparent)]` over the inner `String`. Implement
  `TryFrom<String>` / `FromStr` for validate-on-parse semantics.
- Re-export from `src/lifecycle.rs` so existing daemon imports keep
  working.
- Add round-trip tests in `trinity-core/tests/`: validation
  failures, snake-case stability, wire transparency.

After Phase 2: the daemon uses the new newtypes; on the wire they
appear as plain strings (no JSON shape change).

### Phase 3: Collapse parallel struct definitions

Phase 3a — types where daemon and wire shapes are trivially identical
once newtypes move:

- `WaitingOn`, `ArchivedCycle` (was `ArchivedCycleSummary`) — drop
  the daemon copy in `repo_state.rs`; `pub use trinity_core::WaitingOn`.

Phase 3b — types that need a daemon-side wrapper for runtime state:

- `Plan`: split into `trinity_core::PlanData` (what the wire
  publishes) + daemon-side `Plan { data: PlanData, runtime:
  PlanRuntime }`. Phase 3b moves the publishable fields into
  `PlanData` and rewires daemon code that today reaches into
  `plan.<wire-field>` to read `plan.data.<wire-field>`.

Phase 3c — types with daemon-only projections:

- `Feedback`: move to `trinity-core`. `body` is raw markdown; the
  `body_html` field is REMOVED from this struct entirely (handled
  in Phase 4 by the wasm renderer).

After Phase 3: every wire-publishable type has exactly one definition.

### Phase 4: Move markdown rendering to wasm

- Add `pulldown-cmark` + `ammonia` to `frontend/Cargo.toml` (verify
  wasm-clean; both are pure-Rust pulldown is yes, ammonia is yes).
- Frontend `feedback_card.rs` / `expanded_commit.rs` /
  `plan_preview.rs` consume `body: String` and render to HTML at
  render time. Use a memoized `Memo<String>` over `body` if the
  per-frame rerender cost matters.
- Delete `ui_response::render_feedback_body` /
  `ui_response::render_markdown` / `ui_response::strip_marker_line`
  helpers and their daemon-side imports.
- Round-trip test: a daemon-vs-wasm goldenfile pinning a few
  representative bodies (the verdict-marker stripping, code fences,
  links) produce identical HTML during the migration window. After
  the migration the daemon test goes away; the wasm test stays as
  the renderer's pinning.

After Phase 4: the daemon's `Cargo.toml` drops `pulldown-cmark`
and `ammonia`; the frontend gains them. `Feedback.body_html` does
not appear on the wire.

### Phase 5: Collapse response modules

With domain types == wire types, the response builders become
field-selection adapters:

- One `src/responses.rs` module (replaces `ui_response.rs` and
  `mcp_response.rs`).
- One function per endpoint: `list_plans_response`,
  `plan_detail_response`, `commit_detail_response`, etc.
- Endpoints that produce overlapping shapes (`get_context_response`
  vs `plan_detail_response`) share the bulk and differ only in
  the few fields they include or omit. Either:
  - `GetContextResponse` becomes a `From<&PlanDetailResponse>`
    projection (cheap, ~one-liner), OR
  - both are aliases for one richer `PlanContext` struct with
    optional fields the endpoint clears as appropriate.
  Pick whichever produces a smaller and clearer adapter.
- Delete `build_waiting_on`, `build_archived`,
  `posture_to_review_target_phase`, `build_pr_hint`,
  `build_review_gate`, `build_commit_gate`, `build_timeline`,
  `build_rich_feedback`. Every site that called them now constructs
  `core::Foo { ... }` directly because the daemon's storage IS
  `core::Foo`.

After Phase 5: `ui_response.rs` + `mcp_response.rs` is replaced by
a single ~200 LOC module of endpoint adapters. No `build_*` helpers.

### Phase 6: Fix the residual stringly leaks ruthless flagged

Two non-architectural leaks remain from the prior plan; clean them
up in the same effort:

- **`PrHintOption.name: String`**: closed vocabulary
  (`keep_plan_in_pr` / `exclude_plan_from_pr`). Promote to
  `PrHintOptionKind` enum on `trinity-core`. Frontend matches on
  the enum, not the string. The `_ => "Option"` fallback in
  `pr_hint_card.rs` goes away.
- **MCP error envelope payloads** (`src/server/mcp.rs`): five
  distinct error shapes built via `json!` with an `error:` closed
  discriminator. Replace with a `trinity_core::McpErrorPayload`
  tagged enum (`#[serde(tag = "error", rename_all = "snake_case")]`)
  and a `StartPlanResponse` typed struct. The Guard A "allowed
  envelope" count in `src/server/mcp.rs` drops from 10 to the
  small handful that actually IS the open-vocabulary tool-dispatch
  envelope.

### Phase 7: Widen the guards; verify clean

- Add a generic `pub <ident>: String` scan to Guard B (per-file
  count cap, allowlist enumerates approved opaque-string fields).
  Catches future stringly closed-vocab fields named `name`,
  `action`, `outcome`, `category` that the current needle list
  misses.
- Add a match-without-`.as_str()` pattern check, OR widen the
  needle list to cover `match <ident> {` against string literals.
- Pin `trinity-core` as wasm32-clean: add
  `cargo check -p trinity-core --target wasm32-unknown-unknown`
  to whatever CI / pre-commit verification we use today.

After Phase 7: the guards catch the failure mode that previously
required ruthless review to spot.

### Phase 8: Final audit

- Delete `kind_str()` methods on `RepoEventPayload` /
  `PlanEventPayload`. Daemon tests use `matches!()`. Frontend's
  `live_event_kind_str` resolves to either (a) inline two small
  match arms, or (b) a hand-written `Display` impl on the payload
  enum (still hand-maintained, but with only one match arm per
  variant, eliminated alongside the serde tag — same drift surface
  as before, but no method to forget).
- Confirm guard A and guard B allowlists contain only the
  legitimately documented exceptions:
  - `src/mcp_shim/mod.rs` transport — passes JSON between processes,
    not a domain producer.
  - `src/tools.rs` schema JSON — input-schema is open-vocabulary by
    design.
  - `src/server/mcp.rs` tool dispatch — open vocabulary of tool
    names; the typed `McpErrorPayload` from Phase 6 means the rest
    of `mcp.rs` is typed.
- Document the canonical pattern in the plan footer: domain
  storage IS wire shape; daemon-only state lives on a runtime
  wrapper; the WASM client renders presentation.

## Testing

- `cargo test --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo fmt -- --check`
- `cd frontend && trunk build`
- `cargo check -p trinity-core --target wasm32-unknown-unknown`
- The existing wire-contract guards (`tests/wire_contract_guards.rs`)
  continue to pass; their allowlists shrink phase-by-phase.
- E2E tests in `tests/end_to_end.rs` — wire format is byte-stable
  across the migration, so these should pass without changes
  except for any that asserted on `body_html` content (those move
  to wasm-side render tests).
- A new wasm-side markdown render test pins the rendered HTML for
  a handful of representative bodies (the verdict-marker pattern,
  code fences, sanitization edge cases).

## Acceptance Criteria

- `trinity-wire` is renamed `trinity-core`.
- `trinity-core` contains all newtypes, all closed-vocab enums,
  all response DTOs, all daemon-publishable domain structs.
- `src/ui_response.rs` and `src/mcp_response.rs` are replaced by
  one ~200-LOC `src/responses.rs`. No `build_*` helpers.
- `impl From<…>` count in `src/` is unchanged from today (zero new
  translation impls). The daemon stores `core::Foo`; endpoints
  publish `core::Foo` directly.
- `Feedback.body_html` does not appear on any wire response. The
  daemon ships `body` (raw markdown); the wasm client renders.
- `daemon's Cargo.toml` no longer depends on `pulldown-cmark` /
  `ammonia`. `frontend/Cargo.toml` does.
- `Plan` is split into `core::PlanData` + daemon-side `Plan {
  data: PlanData, runtime: PlanRuntime }`.
- `PrHintOption.name` is a typed `PrHintOptionKind` enum on the wire.
- MCP error envelope payloads in `src/server/mcp.rs` are typed
  variants of `core::McpErrorPayload`. The Guard A allowlist for
  `src/server/mcp.rs` drops to the small open-vocabulary dispatch
  surface.
- Guards A and B catch a future closed-vocab `String` DTO field
  AND a future frontend `match name {}` against string literals.
- A wire-shape round-trip test confirms byte-stable JSON for every
  major response across the migration.

## Non-Goals

- No transport change. Still JSON over HTTP, SSE, and MCP.
- No new endpoints, no new commands, no semantic changes to the
  agent loop or freeze rules.
- No removal of newtype validation. Newtypes move; they keep their
  `TryFrom<String>` strictness.
- No frontend redesign. The WASM client gains a markdown renderer;
  visual output is byte-identical to today.

## Out Of Scope

- Reworking `Plan.timeline` semantics.
- Changing the feedback verdict vocabulary.
- Replacing the MCP framing protocol with something else.
- Adding a graphical diff renderer or richer markdown extensions.

If a structural cleanup exposes a latent bug — e.g. the
`build_commit_gate` / `build_timeline` divergence ruthless flagged —
fix that bug. Otherwise this plan is purely a structure pass.
