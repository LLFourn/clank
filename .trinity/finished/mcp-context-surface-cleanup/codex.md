APPROVE

Reviewed the full series (c7e3dc5 plan intro → three plan
revisions → five implementation phases → two review-followup
commits). The plan landed exactly as spec'd, plus a Phase 5
finishing touch that took the redundancy cleanup further than
the original spec.

The architectural goal — "MCP coordinates work; HTTP transports
content" — is now expressed in the type system rather than in
documentation:

- `echo_cwd` no longer surfaces on the public catalog
  (dispatcher branch kept as a curl-able diagnostic).
- `get_context` renamed to `work_context` and narrowed. The old
  `GetContextResponse` had 18 fields covering the full per-plan
  fold (timeline, every commit's feedback, archived cycles, PR
  hints, …). The new `WorkContextResponse` has 8 fields —
  exactly the coordination view. UI consumers keep
  HTTP `/api/plan/<id>` (PlanDetailResponse) unchanged.
- `ExpectedAction` reshaped from a flat string-enum to a tagged
  enum with per-variant payload. `WriteFeedback { path,
  target_sha }`, `AddressChanges { target_sha, rc_paths }`,
  etc. — each variant carries exactly what the caller needs to
  act on it without re-querying. The previously parallel
  fields (`review_target`, `write_feedback`,
  `latest_relevant_commit`) are gone from `WorkContextResponse`
  because their data folded into the variants.
- `set_active_work` / `clear_active_work` solve the multi-active-
  plan ambiguity case. Selection is consulted at the resolver
  (not in `compute_match`), keyed off projected reason rather
  than a parallel `is_finished` check, validated through the
  same lock-then-disk-read pattern as normal candidate
  inference, and dropped on stale (frozen / hidden / missing
  from `plans` map). Stale-drop path is exercised by integration
  test.
- `watch_repo` is the explicit repo-registration primitive.
  `bootstrap_repo` helper consolidates the four-step bootstrap
  (canonicalize + register + .gitignore + registry-persist +
  watcher-spawn). `watch_repo` and `start_plan` both call it.
  The watcher-live half is pinned by integration test (commit a
  new plan after `watch_repo` returns, assert the daemon picks
  it up).
- Typed errors throughout: `set_active_work` rejections use
  `McpErrorPayload::PlanNotActive` / `PlanHidden` instead of
  free-form `ToolError::Invalid` strings. Two new wire-snapshot
  fixtures plus integration tests pin them.
- MCP shim instruction text rewritten to teach the new flow:
  optional `plan_id` inference, `set_active_work` for multi-plan
  disambiguation, explicit `plan_id` only as an override.

Two review cycles improved the implementation: ruthless caught
the `watch_repo`-was-partial-bootstrap regression and the
stale-selection-missing-from-plans drop hole; codex caught the
remaining stringly errors and the under-tested watcher half.
Phase 5 was an additional cleanup beyond the original spec
prompted by a reviewer noticing the redundancy among
`expected_action` + `review_target` + `write_feedback` +
`latest_relevant_commit`.

Verification commands all green at the close of the plan:
- `cargo build --workspace`
- `cargo test --workspace --exclude trinity-frontend`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo fmt -- --check`
- `cargo check -p trinity-core --target wasm32-unknown-unknown`
- `cargo test -p trinity-core --test wire_snapshots`

Approved as-is.
