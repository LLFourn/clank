# wasm-markdown-rendering

## Summary

Move markdown rendering from the daemon into the WASM frontend.
Drop `pulldown-cmark` + `ammonia` from the daemon entirely. Wire
responses carry only raw markdown (`body`, `plan_body`,
approval `body`). The frontend renders in-browser when it
displays the body.

The architectural payoff:
- `api::Feedback` and `model::Feedback` are ONE type (~50 LOC of
  projection deleted in `responses.rs`).
- `api::CommitGate` and `model::CommitGate` are ONE type.
- The remaining model/api duality from `trinity-core-unification.md`
  (server-side `body_html` rendering) goes away. The narrower
  invariant after this plan: domain state is the canonical typed
  model; wire responses don't carry rendered HTML or duplicate
  semantic concepts; small endpoint-shape wrappers are still fine
  when a response position genuinely needs extra context (none
  remain after Issue 1's resolution, but the door is left open).
- Daemon `Cargo.toml` loses `pulldown-cmark` and `ammonia`.
- No new endpoint to maintain; no daemon-side cache; no
  per-render round trips.

## Bundle-size measurement (verified 2026-05-18)

Tested by adding `pulldown-cmark = "0.13"` + `ammonia = "4"` as
direct frontend deps with a tiny `render_md_probe(input)` call
in `main.rs` to defeat tree-shaking, then `trunk build --release`:

| Build | Release wasm |
|---|---|
| Baseline (no markdown deps) | 1.4 MB |
| + pulldown-cmark + ammonia | 2.4 MB |
| **Delta** | **+1.0 MB** |

Both crates compile cleanly to `wasm32-unknown-unknown` today.
The earlier "ammonia not wasm-clean" speculation (during
`trinity-core-unification.md` planning) was outdated.

Cost analysis for a local dev tool:
- On loopback, +1.0 MB download is microseconds.
- Brotli compresses wasm ~3-4x; over a real network this would
  be ~250-400 KB, well under a "single image" budget.
- Wasm parse + instantiate overhead on first page load:
  ~50ms additional on a modern laptop. One-time cost.
- Subsequent renders: no round trip, no cache miss, no
  serialization. Strictly faster than a server endpoint.

The bundle cost is real but not blocking. The architectural
upside (smaller daemon, one type per concept, no new endpoint)
outweighs the +1MB.

## Resolving the `author` ownership

`model::Feedback` today has no `author` field — author identity
is the map key in `CommitGate.feedback: BTreeMap<AgentLabel,
Feedback>`. `api::Feedback` carries `author: String` because
response positions like `CommitRow.feedback: Vec<api::Feedback>`,
`CommitDetail::{PlanOnly, CodeOnly, Mixed}.feedback`, and
`PlanRevisionResponse.feedback` need the author name without a
map context to read it from.

Dropping `body_html` alone is not enough to collapse the two
types — author ownership has to be resolved.

**Decision**: add `author: AgentLabel` to `model::Feedback`.

- Storage becomes `BTreeMap<AgentLabel, Feedback>` where every
  value's `author` field equals its map key. The redundancy is
  bounded — there is exactly one writer
  (`disk_snapshot.rs::apply_commit`, which builds the gate from
  feedback files on disk) and one consumer of the read path
  (`responses.rs::feedback_for_target` and the `build_*` helpers
  it calls, before they're deleted). Both can `debug_assert!` the
  invariant.
- The wire is already redundant here — the existing
  `api::CommitGate.feedback: BTreeMap<String, api::Feedback>` has
  the same map-key/value-field duplication. No new wire shape;
  storage just stops being the odd one out.
- After this change, `model::Feedback` and `api::Feedback` ARE
  the same type. One Feedback. No wrapper struct, no keyed-map
  vs vec ambiguity.

The alternatives (a `AuthoredFeedback { author, feedback }`
wrapper for Vec positions; or moving Vec positions to maps) were
considered. The wrapper adds a type for one redundancy, and the
map move changes wire shape from JSON array to JSON object — both
are more change for less clarity than just paying the redundancy.

## Wire-shape changes (enumerated)

1. **`api::Feedback`** drops `body_html`. Renames `body_raw → body`
   to match the model side. Collapses with `model::Feedback`
   (which gains `author: AgentLabel` per the resolution above).

2. **`api::CommitGate`** collapses with `model::CommitGate` via
   the unified `Feedback`.

3. **`api::FinalizeApproval`** drops `body_html`, adds `body`
   (raw markdown).

4. **`api::PlanDetailResponse`** drops `plan_body_html` /
   `plan_body_truncated`. Adds `plan_body` (raw markdown).
   Frontend computes truncation locally.

5. **`api::PlanRevisionResponse`** drops `body_html`, renames
   `body_raw → body`.

6. **`api::TimelineEvent::Review`** drops `body_html` if it
   carries one. Audit during Phase 2.

Every change is a field removal, rename, or (for #1) a
storage-side field addition that already exists on the wire.
Schema snapshots register the wire changes once per affected
endpoint.

## What goes where after this plan

- `pulldown-cmark` + `ammonia` in `frontend/Cargo.toml`.
- New `frontend/src/markdown.rs` (or `util/markdown.rs`):
  pure-Rust `render(input: &str) -> String` using the existing
  pipeline (pulldown-cmark with the same options + ammonia with
  the same sanitizer config the daemon uses today).
- Verdict-marker stripping (`strip_marker_line` /
  `skip_marker_tail`) moves to the frontend too — it's a
  presentation concern, frontend already has the verdict.
- Daemon's `src/responses.rs` loses `render_markdown` /
  `render_feedback_body` / `strip_marker_line` /
  `skip_marker_tail` / `build_feedback` / `build_commit_gate`.
  The `model::CommitGate → api::CommitGate` projection becomes
  `gate.clone()` (or `pub use model::CommitGate as api::CommitGate`).
- Daemon's `Cargo.toml` loses `pulldown-cmark` and `ammonia`.

## Implementation Phases

### Phase 1: Add wasm renderer; daemon still emits rendered fields

- Add `pulldown-cmark` + `ammonia` to `frontend/Cargo.toml`.
- New `frontend/src/markdown.rs` with `render(input, verdict_hint)
  -> String`. Pull the stripping helpers from
  `daemon/src/responses.rs` and translate to wasm-side code.
- Add a wasm-side test pinning the rendered HTML for a handful
  of representative bodies (verdict markers, code fences, links,
  ammonia sanitization edge cases). The fixture should match the
  daemon's existing render output during this phase.

After Phase 1: frontend has a working renderer; nothing yet
consumes it from the response side. Bundle size baseline now
includes the deps.

### Phase 2: Collapse `api::Feedback` with `model::Feedback`

- Drop `body_html` from `api::Feedback`. Rename `body_raw → body`.
- `api::CommitGate.feedback` becomes `BTreeMap<AgentLabel, Feedback>`
  with the unified type. `api::CommitGate` collapses with
  `model::CommitGate`.
- `responses.rs::build_feedback` deleted. `build_commit_gate`
  becomes `gate.clone()` (or eliminated via `pub use`).
- Frontend `feedback_card.rs` calls `markdown::render` at
  render-time instead of reading `body_html`. No new state — the
  render is synchronous because it's local.
- `TimelineEvent::Review` audit: drop `body_html` if present;
  rename `body_raw → body`.
- Wire snapshots register the field changes.

### Phase 3: Plan body + revision body + finalize approval

- `api::PlanDetailResponse` drops `plan_body_html` /
  `plan_body_truncated`. Adds `plan_body`. Frontend
  `plan_preview.rs` renders locally and computes truncation.
- `api::PlanRevisionResponse` drops `body_html`, renames
  `body_raw → body`. Frontend `plan_revision.rs` renders.
- `api::FinalizeApproval` drops `body_html`, adds `body`.
  `frontend::finalize_snapshot.rs` and
  `commit_diff.rs::FinalizeSnapshot` render locally.
- Schema snapshots register the changes.

### Phase 4: Drop daemon-side rendering deps

- Delete `render_markdown` / `render_feedback_body` /
  `strip_marker_line` / `skip_marker_tail` from `responses.rs`.
- Remove `pulldown-cmark` and `ammonia` from the daemon's
  `Cargo.toml`. Confirm `cargo build --workspace` builds without
  them.
- Drop the two divergence regression tests from
  `responses.rs::divergence_tests` (or move them to assert on
  the model side directly). `body_html_is_one_renderer_across_both_surfaces`
  becomes trivially true — there's only one model::Feedback.

### Phase 5: Tidy + final audit

- Run the wire-snapshot suite; confirm only the enumerated
  changes diff.
- Verify `model::Feedback` and `api::Feedback` resolve to the
  same type (`pub use` re-export confirms at compile time).
- `trinity-core-unification.md` is already finished; this plan
  does NOT touch it. The body_html-on-wire caveat noted in that
  plan's commentary becomes outdated by this plan's existence,
  not by edits to its file.

## Rules

- `trinity-core` stays serde-only. Rendering crates live in the
  FRONTEND now, not the daemon, not the core.
- Wire shape changes only happen at the six enumerated
  removals/renames.
- Frontend renders synchronously. No `LocalResource`, no loading
  state — `markdown::render(body)` returns a `String` at
  view-build time.
- Failed render (panic in pulldown-cmark / ammonia) is treated
  as a programming bug; render is deterministic over arbitrary
  bytes. No fallback needed.

## Testing

- `cargo test --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo fmt -- --check`
- `cd frontend && trunk build` (and `trunk build --release` to
  pin the bundle size delta)
- `cargo check -p trinity-core --target wasm32-unknown-unknown`
- Wire snapshot suite — every enumerated change registers as one
  schema diff per affected endpoint.
- Frontend render tests pin the HTML for representative bodies
  (verdict markers, code fences, links, the ammonia
  sanitization edge cases). Move from `daemon::responses` tests
  to `frontend::markdown` tests.

## Acceptance Criteria

- `frontend/Cargo.toml` has `pulldown-cmark` + `ammonia`.
- `daemon/Cargo.toml` has neither (verified via `cargo tree`).
- `api::Feedback` has no `body_html`. Same type as
  `model::Feedback`.
- `api::CommitGate` has no body_html-carrying feedback. Same
  type as `model::CommitGate`.
- `api::FinalizeApproval` has no `body_html`.
- `api::PlanDetailResponse` has no `plan_body_html` /
  `plan_body_truncated`. Has `plan_body`.
- `api::PlanRevisionResponse` has no `body_html`. Has `body`.
- `responses.rs::build_feedback` deleted; `build_commit_gate`
  deleted or collapsed to `.clone()`.
- No remaining markdown-render helpers in the daemon.
- Wire snapshots match the enumerated changes; no other diffs.
- Release wasm bundle is ~2.4 MB (was ~1.4 MB pre-rendering;
  +1 MB delta is acceptable for a local dev tool).

## Non-Goals

- No transport change (still JSON over HTTP / MCP).
- No frontend redesign. The visible output is unchanged.
- No server-side rendering "fallback path." Frontend renders;
  that's it.
- No custom markdown extensions beyond what the daemon currently
  uses (`ENABLE_TABLES | ENABLE_STRIKETHROUGH | ENABLE_TASKLISTS
  | ENABLE_FOOTNOTES`).

## Trade-off honest record

This plan **adds ~1 MB to the release WASM bundle**. For a
local dev tool over loopback, that's a one-time cost on first
page load (~50ms parse overhead). Brotli compresses wasm ~3-4x
if Trinity were ever served over a real network, putting the
delta at ~250-400 KB on-the-wire.

If the bundle size ever becomes a constraint — Trinity gets a
hosted-service variant, or a user runs on a bandwidth-limited
environment — the alternative is the daemon-side render
endpoint (`POST /api/markdown` with a cache). That's the
recommended fallback. But for the current local-dev-tool use
case, the bundle cost is acceptable and the architectural
simplification is the bigger win.
