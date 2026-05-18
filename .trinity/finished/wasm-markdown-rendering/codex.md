APPROVE

Reviewed across the full series (b594fa2 → 6883251). The plan
landed exactly as enumerated:

- Wasm-side renderer at `frontend/src/markdown.rs` —
  `pulldown-cmark` with the same options the daemon previously
  used (`ENABLE_TABLES | ENABLE_STRIKETHROUGH | ENABLE_TASKLISTS
  | ENABLE_FOOTNOTES`) plus `ammonia::Builder::default()` with
  one allowed `class` attribute. 10 unit tests pin verdict-marker
  stripping, code fences, tables, strikethrough, and the
  sanitization edge cases.
- `api::Feedback` collapsed onto `model::Feedback` via
  `pub use crate::model::Feedback` in `trinity-core::api`.
  `model::Feedback` now carries an `author: AgentLabel` field;
  the gate-map key / value-author invariant holds structurally
  (every gate writer derives the map key from `feedback.author`,
  no sibling `let author = parsed.author` bindings remain).
- `api::CommitGate` and `api::ArchivedCycle` similarly collapse
  to `pub use crate::model::*`; `repo_state.rs` re-exports
  `ArchivedCycle` from `model` not `api` (the model is canonical).
- Five enumerated wire-shape drops: `body_html` removed from
  Feedback, CommitGate, and FinalizeApproval; `plan_body_html`
  + `plan_body_truncated` collapsed to `plan_body` on
  PlanDetailResponse; `body_html` + `body_raw` collapsed to
  `body` on PlanRevisionResponse.
- Daemon `Cargo.toml` no longer carries `pulldown-cmark` or
  `ammonia` (`cargo tree -p trinity` confirms). The
  `render_markdown` helper and the `build_feedback` /
  `build_commit_gate` projection wrappers were deleted.
- `wire_contract_guards.rs` Guard C allowlist no longer mentions
  `body_html` / `body_raw`; if those names re-appear they fail
  the guard at compile time.
- The `commit_gate_feedback_key_matches_value_author` fold-
  boundary test in `disk_snapshot.rs` walks the gate map post-
  fold and verifies key == value.author end-to-end; now backed
  by a real construction property rather than tautological
  per-writer assertions.
- The `divergence_tests::multi_plan_event_never_emits_review_rows`
  invariant from trinity-core-unification survives intact. The
  sibling `body_html_is_one_renderer_across_both_surfaces` test
  became meaningless once the wire stopped carrying `body_html`
  and was dropped cleanly.
- Daemon binary bundles `frontend/dist/` via `build.rs` +
  `include_dir!`; release wasm bundle is 2.4 MB (was ~1.4 MB
  pre-rendering — the +1 MB delta is documented as acceptable for
  a local dev tool and would brotli-compress to ~250–400 KB if
  ever served over a real network).

Verification commands all green at the close of the plan:
- `cargo build --workspace`
- `cargo test --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo fmt -- --check`
- `cd frontend && trunk build --release`
- `cargo check -p trinity-core --target wasm32-unknown-unknown`
- `cargo test -p trinity-core --test wire_snapshots`

Approved as-is.
