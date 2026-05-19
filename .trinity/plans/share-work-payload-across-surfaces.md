# share-work-payload-across-surfaces

`wait_for_work` and `work_context` both answer "what should I do
next?" but ship different shapes. Two diverging concept hierarchies
for one domain. Unify them: one `WorkPayload` (plan_id + repo +
the typed action) used by both surfaces — flat on `wait_for_work`,
flat-embedded into `work_context`'s state-rich response.

## Why

Current state of the two responses, side-by-side:

**`wait_for_work` happy path** (`WaitForWorkResponse::Work(WorkPayload)`):
```json
{
  "plan_id": "trinity/foo.md",
  "repo": "/abs/path",
  "locations": [".trinity/feedback/foo/abc/codex.md"],
  "work": "review_commit",
  "target_sha": "abc",
  "commit_kind": "plan_only",
  "prompt_hint": "Review the latest commit on this plan."
}
```

**`work_context`**:
```json
{
  "plan_id": "trinity/foo.md",
  "repo": "/abs/path",
  "current_path": ".trinity/plans/foo.md",
  "lifecycle": "active",
  "phase": "planning",
  "plan_worktree_status": "clean",
  "waiting_on": {...},
  "expected_action": {
    "kind": "write_feedback",
    "target_sha": "abc",
    "path": ".trinity/feedback/foo/abc/codex.md"
  }
}
```

The two answer the same domain question. Differences:

1. **Two action enums for one concept.** `WorkAction` (tagged
   `work`, variants `ReviewCommit`/`AddressCommitChanges`/...) on
   WFW; `ExpectedAction` (tagged `kind`, variants
   `WriteFeedback`/`AddressChanges`/...) on `work_context`.
2. **`locations: Vec<String>`** on WFW duplicates the same paths
   that `ExpectedAction` variants carry (`WriteFeedback.path`,
   `AddressChanges.rc_paths`).
3. **`commit_kind` + `prompt_hint`** on every WFW variant are
   noise. The agent can run `git show <target_sha>` to see what
   changed; the prompt-hint string is agent-system-prompt
   material, not per-response payload.

The user wants the work half identical on both surfaces.
`work_context` is "state context + the work"; WFW is "block,
return just the work or a timeout."

## What

One typed payload, shared.

### 1. Define `WorkPayload`

```rust
/// The action the caller should take, with the surrounding
/// identity. Used flat on `wait_for_work` and flat-embedded
/// into `work_context` via `#[serde(flatten)]`.
pub struct WorkPayload {
    pub plan_id: String,
    pub repo: String,
    #[serde(flatten)]
    pub action: ExpectedAction,
}
```

### 2. `wait_for_work` happy path is `WorkPayload`

```rust
#[serde(untagged)]
pub enum WaitForWorkResponse {
    Work(WorkPayload),       // was: WorkPayload (old shape)
    Timeout(WaitTimeout),    // unchanged
}
```

Wire happy-path shape becomes:

```json
{
  "plan_id": "trinity/foo.md",
  "repo": "/abs/path",
  "kind": "write_feedback",
  "target_sha": "abc",
  "path": ".trinity/feedback/foo/abc/codex.md"
}
```

Drops `locations`, `commit_kind`, `prompt_hint`. Discriminator
field on the action renames from `"work"` to `"kind"` (unified
with `ExpectedAction`).

### 3. `work_context` embeds `WorkPayload`

```rust
pub struct WorkContextResponse {
    #[serde(flatten)]
    pub work: WorkPayload,
    pub current_path: String,
    pub lifecycle: PlanLifecycle,
    pub phase: Posture,
    pub plan_worktree_status: PlanWorktreeStatus,
    pub waiting_on: WaitingOn,
}
```

Wire shape:

```json
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
  "waiting_on": {...}
}
```

The work-half prefix is **byte-identical** to WFW's response.
Anything keying on `kind`/`target_sha`/`path` / etc. works on
both endpoints without branching.

### 4. Delete `WorkAction`

`vocab::WorkAction` (the parallel tagged enum) and all its
projection paths (`prompt_hint_for`, `derive_locations`,
`build_action` in `src/server/wait.rs`) go away. `ExpectedAction`
is the sole "what to do" enum.

### Variant payloads (recap)

`ExpectedAction` already exists from the previous plan:

```rust
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExpectedAction {
    WriteFeedback { path: String, target_sha: String },
    AddressChanges { target_sha: String, rc_paths: Vec<String> },
    CommitPlanRevision,
    StartImplementation { previous_commit: String },
    SessionFinished,
}
```

Five variants, no shared-payload smell. WFW returns these too.

## Files touched (sketch)

- `crates/trinity-core/src/api.rs` — define `WorkPayload`;
  rewrite `WaitForWorkResponse::Work(WorkPayload)` to use the
  new struct; delete `WorkAction`; reshape `WorkContextResponse`
  to flatten-embed `WorkPayload` + the state fields.
- `crates/trinity-core/src/vocab.rs` — `CommitKind` stays (used
  elsewhere). No deletions here this round.
- `src/server/wait.rs` — gut the WFW projection. The
  `build_action`/`derive_locations`/`prompt_hint_for` triplet
  collapses to one builder that takes the candidate snapshot
  and emits `WorkPayload { plan_id, repo, action: ExpectedAction }`.
  Most of the per-variant `(target_sha, commit_kind, prompt_hint)`
  threading vanishes.
- `src/responses.rs::work_context_response_from_snapshot` —
  build the `WorkPayload` (sharing the new builder with WFW
  if cheap) and embed it in the response struct.
- `src/server/wait.rs::integration_tests` — update existing
  tests that destructure `WorkAction::ReviewCommit { ... }` etc.
  Action match arms reshape onto `ExpectedAction` variants.
- `tests/end_to_end.rs` — tests that read
  `body["result"]["work"]` (the old discriminator) now read
  `body["result"]["kind"]`. Tests that check `locations[0]`
  / `target_sha` etc. continue working since the variant fields
  surface at the top via flatten.
- `crates/trinity-core/tests/wire_snapshots.rs` — fixtures for
  `wait_for_work_work` and `work_context_response` regenerated.
- `crates/trinity-core/tests/round_trip.rs` — drop the old
  `work_action_*_round_trips` tests; the `ExpectedAction`
  round-trips from the previous plan already cover the action
  half.

## Rules

- One action enum. `ExpectedAction` is it. `WorkAction` is
  deleted; no parallel enum.
- One identity-carrying struct for "this is the work."
  `WorkPayload` is it. Both endpoints flatten it into their
  top-level response.
- Wire discriminator name is `kind` on both surfaces. The legacy
  `work` discriminator goes away with `WorkAction`.
- `WaitTimeout` is unchanged. The untagged
  `WaitForWorkResponse` keeps its field-presence discrimination
  on `timed_out` vs `kind`.

## Testing

- `cargo test --workspace --exclude trinity-frontend`.
- `cargo test -p trinity-frontend`.
- `cargo clippy --workspace --all-targets -- -D warnings`.
- `cargo fmt -- --check`.
- `cd frontend && trunk build --release`.
- Wire snapshots regenerated; manual diff confirms the shape
  matches the spec above.
- Live: `just restart`, call `mcp__trinity__wait_for_work`
  (after the shim restarts) and `mcp__trinity__work_context`,
  confirm both responses are flat-shaped with `kind` at top
  level.

## Acceptance criteria

- `wait_for_work` happy path returns
  `{ plan_id, repo, kind, ...action fields }`. No `locations`,
  `commit_kind`, `prompt_hint`, or `work` discriminator.
- `work_context` returns
  `{ plan_id, repo, kind, ...action fields, current_path,
  lifecycle, phase, plan_worktree_status, waiting_on }`. The
  work-prefix matches WFW byte-for-byte.
- `WorkAction` no longer exists in `crates/trinity-core/src/api.rs`.
- `WorkPayload` is defined and used by both surfaces.
- Existing integration tests in `src/server/wait.rs` updated
  to match the new variant shapes.
- All workspace + frontend tests green.

## Non-goals

- Touching `WaitTimeout`. Its conditional-presence serde attrs
  (`no_active_plans`, `repo` Option) stay.
- Adding new variants to `ExpectedAction`. The five from
  `mcp-context-surface-cleanup` cover everything WFW returns.
- Renaming `wait_for_work` or `work_context` tools. Just the
  payload shape.
- HTTP surface (`/api/plan/<id>` → `PlanDetailResponse`). UI
  consumers stay untouched.

## Trade-off honest record

The big call: **delete `WorkAction`**. Some prior integration
tests pattern-match against it (e.g.
`WorkAction::SessionFinished` in `wait.rs` tests). Migrating to
`ExpectedAction::SessionFinished` is straightforward but
mechanical across a dozen sites.

The smaller call: drop `prompt_hint`. It was a useful "tell the
agent how to read this" string, but it's better placed on the
tool description / agent system prompt where it lives once
instead of being re-shipped on every response. If a per-response
hint turns out to be actually needed, it can come back as a
variant payload field on the relevant `ExpectedAction` variant.

The `locations` field went away because the action variants
carry the same paths. Wire shape strictly shrinks.
