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

### Variant payloads — extended

`ExpectedAction` exists from the previous plan but its variants
currently lack paths that `src/server/wait.rs::derive_locations`
returns for WFW. Extend each variant so the payload is
actionable without a parallel `locations` list:

```rust
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExpectedAction {
    /// Reviewer: write verdict markdown to `path` against the
    /// commit at `target_sha`.
    WriteFeedback {
        path: String,
        target_sha: String,
    },
    /// Master: address RC feedback. `rc_paths` are the
    /// request-changes files to read; `plan_path` is `Some`
    /// when the RC is plan-side (kind `PlanOnly | Mixed`),
    /// meaning the master needs to revise the plan body as
    /// part of the fix-up commit. `None` for code-side
    /// (`CodeOnly`) RCs.
    AddressChanges {
        target_sha: String,
        rc_paths: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        plan_path: Option<String>,
    },
    /// Master: commit the dirty plan file at `plan_path`.
    CommitPlanRevision {
        plan_path: String,
    },
    /// Master: write the next implementation commit. `plan_path`
    /// is provided so the agent has the plan body to hand without
    /// reconstructing the path from `plan_id`. `previous_commit`
    /// is the SHA of the approved commit being built on.
    StartImplementation {
        previous_commit: String,
        plan_path: String,
    },
    /// Plan is finalized; no further action.
    SessionFinished,
}
```

Each variant carries exactly the paths/SHAs the agent needs to
act. The pre-existing `locations` field on `WorkPayload`
disappears because every path that used to live there is now in
the variant where it's relevant.

`current_path` on `WorkContextResponse` stays — that's the
always-on "where this plan's file lives" pointer, useful for
`SessionFinished` displays and surface routing. It's allowed to
duplicate `plan_path` inside an action variant; the variant
payload is what the agent acts on, `current_path` is the
identity pointer.

## Files touched (sketch)

- `crates/trinity-core/src/api.rs` — define `WorkPayload`;
  rewrite `WaitForWorkResponse::Work(WorkPayload)` to use the
  new struct; delete `WorkAction`; reshape `WorkContextResponse`
  to flatten-embed `WorkPayload` + the state fields.
- `crates/trinity-core/src/vocab.rs` — `CommitKind` stays (used
  elsewhere). No deletions here this round.
- `src/server/wait.rs` — gut the WFW projection. The
  `build_action` / `derive_locations` / `prompt_hint_for`
  triplet collapses into the new shared builder (see below).
- `src/responses.rs::work_context_response_from_snapshot` —
  builds `WorkPayload` via the SAME shared builder, then
  flatten-embeds it alongside the state-context fields.

### Shared builder (mandatory, not "if cheap")

The whole point of this plan is preventing two parallel
"what's the work" hierarchies from drifting. One projection
function:

```rust
// In src/responses.rs (or wherever — the goal is that BOTH
// surfaces call this; no second implementation).
pub fn build_work_payload(
    plan_id: &PlanId,
    repo_root: &Path,
    plan_path: &str,         // canonical plan file path
    waiting: &WaitingOn,     // gives us the reason
    review_target: Option<&CommitSha>,
    review_target_kind: Option<CommitKind>,
    gate: Option<&CommitGate>,
    author: &AgentLabel,
) -> WorkPayload {
    let action = match waiting.reason {
        WaitingReason::CommitNeedsReview => {
            let sha = review_target.expect(...).as_str().to_string();
            ExpectedAction::WriteFeedback {
                path: format!(".trinity/feedback/{}/{}/{}.md", plan_id.key(), sha, author),
                target_sha: sha,
            }
        }
        WaitingReason::AddressCommitChanges => {
            let sha = review_target.expect(...).as_str().to_string();
            let rc_paths = gate.map(|g| g.requesters.iter().map(|a|
                format!(".trinity/feedback/{}/{}/{}.md", plan_id.key(), sha, a)
            ).collect()).unwrap_or_default();
            let plan_side = matches!(review_target_kind,
                Some(CommitKind::PlanOnly | CommitKind::Mixed));
            ExpectedAction::AddressChanges {
                target_sha: sha,
                rc_paths,
                plan_path: if plan_side { Some(plan_path.to_string()) } else { None },
            }
        }
        WaitingReason::CommitPlanRevision => {
            ExpectedAction::CommitPlanRevision { plan_path: plan_path.to_string() }
        }
        WaitingReason::ReadyToStartImplementation => {
            ExpectedAction::StartImplementation {
                previous_commit: review_target.expect(...).as_str().to_string(),
                plan_path: plan_path.to_string(),
            }
        }
        WaitingReason::SessionFinished => ExpectedAction::SessionFinished,
    };
    WorkPayload {
        plan_id: plan_id.to_string(),
        repo: repo_root.to_string_lossy().into_owned(),
        action,
    }
}
```

Both `src/server/wait.rs::wait_for_work` and
`src/responses.rs::work_context_response_from_snapshot` MUST
call this exact function. No parallel "build the action" code
in either surface.

Cross-surface invariant test (mandatory acceptance criterion):
a unit test fixtures a `(Candidate / RepoState snapshot, author)`
and asserts the `WorkPayload` returned by the WFW path equals the
`work_context` path's. Same state in, same payload out. If they
ever diverge, the test fails at compile time (the types are
literally the same) or at runtime (the values must match).
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
  half. Add round-trips for the new payload fields
  (`AddressChanges.plan_path`, `CommitPlanRevision.plan_path`,
  `StartImplementation.plan_path`).
- `src/tools.rs` — rewrite the `wait_for_work` and
  `work_context` tool descriptions to reflect the new flat
  shape. Today's docs advertise `work`, `locations`,
  `commit_kind`, `prompt_hint` for WFW and `expected_action`
  for `work_context`; both become misleading after the wire
  change.
- `src/mcp_shim/mod.rs` — the bootstrap instruction text
  references both tools; check it doesn't promise the old
  shape.

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
- One shared `build_work_payload` function exists and is the
  ONLY constructor of `WorkPayload`. Both `wait_for_work` and
  `work_context` call it.
- A cross-surface equality test asserts that, for the same
  `(snapshot, author)`, the `WorkPayload` returned by the WFW
  path equals the one embedded in `WorkContextResponse`.
- `src/tools.rs` descriptions and `src/mcp_shim/mod.rs`
  instruction text both describe the new flat shape — no
  lingering references to `work`, `locations`, `commit_kind`,
  `prompt_hint`, or the old nested `expected_action` field
  layout.
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
