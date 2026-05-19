APPROVE

Reviewed the full series (4873895 plan intro → 0f29ff8 plan
revision → 038da19 implementation). The two-headed
"what's the work" hierarchy is now one shape, projected by one
function, used by both surfaces.

The wire collapse:

- **`WorkAction` is gone.** `ExpectedAction` is the sole action
  enum, tagged by `kind` (snake_case) on the wire.
- **`ExpectedAction` variants extended** with the paths that
  used to live in `WorkPayload.locations`:
  - `AddressChanges { target_sha, rc_paths, plan_path: Option<String> }`
    — `plan_path` is `Some` iff the target's `CommitKind` is
    plan-side (`PlanOnly | Mixed`).
  - `CommitPlanRevision { plan_path }`.
  - `StartImplementation { previous_commit, plan_path }`.
- **`WorkPayload`** is `{ plan_id, repo, #[serde(flatten)]
  action: ExpectedAction }` — used flat on
  `WaitForWorkResponse::Work` and `#[serde(flatten)]`-embedded
  into `WorkContextResponse`. The work-prefix on both surfaces
  is byte-identical by construction.
- **`WorkPayload.locations` deleted.** Every path it used to
  carry now lives on the variant where it's relevant.
- **`commit_kind` and `prompt_hint` deleted** from the response.
  The agent runs `git show <target_sha>` to see what changed;
  prompt-hint guidance lives once in the tool description, not
  on every response payload.

The shared projection:

- `src/responses.rs::build_work_payload(WorkPayloadInputs)` is
  the sole constructor of `WorkPayload`. The `WorkPayloadInputs`
  struct lets `wait_for_work` (from its `Candidate` snapshot) and
  `work_context_response_from_snapshot` (from a `Plan` in hand)
  funnel into the same builder.
- All of `build_action`, `derive_locations`, `prompt_hint_for`,
  `feedback_path`, `rc_feedback_paths`, and the `WorkItem`
  wrapper in `src/server/wait.rs` are gone — subsumed by the
  shared builder.
- Cross-surface invariant test
  `wait_for_work_and_work_context_agree_on_work_payload` in
  `src/server/wait.rs::integration_tests` calls both surfaces
  for the same `(snapshot, author)` and asserts
  `wait_payload == work_context.work`. Guards against any future
  regression that introduces a second projection path.

Surface descriptions and shim text rewritten to describe the new
flat shape — no lingering references to `work`, `locations`,
`commit_kind`, `prompt_hint`, `expected_action` (as a nested
field), `review_target`, or `write_feedback`.

Wire snapshots regenerated; the new shapes match the spec:

```json
// wait_for_work happy path
{
  "plan_id": "trinity/foo.md",
  "repo": "/abs/path",
  "kind": "write_feedback",
  "target_sha": "abc",
  "path": ".trinity/feedback/foo/abc/codex.md"
}
```

```json
// work_context: same work-prefix + state context
{
  "plan_id": "trinity/foo.md",
  "repo": "/abs/path",
  "kind": "write_feedback",
  "target_sha": "abc",
  "path": ".trinity/feedback/foo/abc/codex.md",
  "current_path": ".trinity/plans/foo.md",
  "lifecycle": "active",
  "phase": "planning",
  "plan_worktree_status": "clean",
  "waiting_on": { ... }
}
```

Verification commands all green at the close of the plan:
- `cargo build --workspace --all-targets`
- `cargo test --workspace --exclude trinity-frontend`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo fmt -- --check`
- `cd frontend && trunk build`
- `cargo test -p trinity-core --test wire_snapshots`
- `cargo test -p trinity-core --test round_trip`

Approved as-is.
