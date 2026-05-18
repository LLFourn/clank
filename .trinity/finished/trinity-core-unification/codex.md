APPROVE

Reviewed across the full series (08884ee → 4217df7). The plan's
hard direction landed:

- `trinity-wire` renamed to `trinity-core`; modules split into
  `vocab` (closed-vocab enums), `ids` (validated identifier
  newtypes with serde-transparent + validate-on-deserialize),
  `model` (daemon fold-state types — Plan, PlanTimelineEvent,
  CommitGate, Feedback, ArchivedCycle), and `api` (response DTOs
  + projection-only structs).
- Daemon storage uses `model::*` types directly via re-exports
  in `repo_state.rs` / `review_state.rs`. No parallel
  `WaitingOn` / `Feedback` / `CommitGate` definitions on either
  side.
- `src/mcp_response.rs` (835 LOC) + `src/ui_response.rs` (618 LOC)
  collapsed into one `src/responses.rs`. One copy of every
  `build_*` helper; `body_html` rendering happens in one
  function called by both surfaces. ~810 LOC deleted net.
- Diff types unified: `src/diff_parser.rs` produces
  `trinity_core::api::*` directly; the boundary mapper deleted.
- `RepoState::timeline_for` + local `TimelineEvent` enum deleted
  as dead production code.
- `PrHintOption.name: String` → `kind: PrHintOptionKind` enum.
- MCP error envelope typed: `core::api::McpErrorPayload` tagged
  enum replaces five ad-hoc `json!` shapes. `WaitForWorkResponse
  ::Timeout` replaces the raw `{timed_out, no_active_plans,
  repo}` JSON. Guard A on `src/server/mcp.rs` dropped 10 → 1
  (the genuine open-vocab tool-dispatch envelope).
- Three compile-time guards in `tests/wire_contract_guards.rs`:
  Guard A (dynamic-JSON allowlist), Guard B (stringly-
  control-flow allowlist), Guard C (positive allowlist of
  approved `String` fields in `trinity-core`). Guard C catches
  the failure mode ruthless had to spot manually.
- 20 schema snapshots in `crates/trinity-core/tests/wire_snapshots.rs`
  pin every top-level response shape (including the typed MCP
  error envelope variants).
- Two regression tests in `src/responses.rs::divergence_tests`
  pin the architectural invariants (one renderer for body_html
  across surfaces; MultiPlan never emits Review rows).

The remaining model/api duality (body_html on `api::Feedback`
vs raw on `model::Feedback`) is documented in
`.trinity/plans/wasm-markdown-rendering.md` — the follow-up plan
collapses it via WASM-side rendering.

Verification commands at the close of the plan all green:
- `cargo build --workspace`
- `cargo test --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo fmt -- --check`
- `cd frontend && trunk build`
- `cargo check -p trinity-core --target wasm32-unknown-unknown`
- `cargo test -p trinity --test wire_contract_guards`
- `cargo test -p trinity-core --test wire_snapshots`

Approved as-is.
