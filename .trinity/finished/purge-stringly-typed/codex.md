APPROVE

Reviewed across the full series (08884ee → 4ec43dc). The plan's
original intent is delivered: `trinity-wire` exists as the single
source of truth for closed-vocabulary enums and response DTOs;
daemon and frontend import the same types; the two compile-time
guards (Guard A: dynamic-JSON sites; Guard B: stringly-control-flow
sites) drained to only the documented exceptions
(`mcp_shim/mod.rs` transport, MCP envelope/dispatch, tool
input-schema JSON).

Key architectural moves:

- `#[serde(flatten)]` over tagged enums on `CommitDetailResponse` /
  `LiveEvent` / `TimelineEvent` made kind-dependent payload shapes
  structural — eliminating the `finalize_snapshot: null` regression
  class.
- `RepoEventKind` / `PlanEventKind` collapsed into the payload
  enum's serde discriminator (one source of truth for "what kind
  of event").
- The address-changes on 744e4bf caught the only real architectural
  leak: `ReviewGate.state: String` in the wire crate, plus a guard
  scope that excluded `crates/trinity-wire/src`. Both fixed in
  4ec43dc; the guard now scans the wire crate too.

A separate follow-up plan will collapse the daemon's two parallel
response-builder modules (`ui_response.rs` + `mcp_response.rs`)
into a single From-impl boundary — that's an architectural
deduplication, not a stringly-typed leak, and out of scope here.

Approved as-is.
