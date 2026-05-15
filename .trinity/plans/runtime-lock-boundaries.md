# Runtime Lock Boundaries

## Summary

Fix the daemon architecture so filesystem and git I/O cannot happen while the global `Runtime` / `Trinity` mutex is held.

The immediate symptom is the web interface freezing for seconds while requests render. The root cause is not that Trinity has a mutex; it is that `Runtime::read_repo` accepts an arbitrary closure over `RepoState`, and several callers use that closure to invoke response builders that perform disk reads. That makes the lock boundary invisible and easy to violate.

The target architecture is:

1. Lock the runtime only long enough to copy cheap in-memory data into an owned snapshot.
2. Release the lock.
3. Perform filesystem and git reads from the snapshot.
4. Assemble MCP / HTTP / UI responses after the lock is gone.

`wait_for_work` already follows this pattern. Generalize that pattern and make the incorrect pattern harder to write.

## Problem

`Runtime::read_repo(repo, |state| { ... })` currently holds the global mutex for the whole closure. That closure looks like an in-memory read, but callers can call functions that do I/O inside it.

Known offenders:

- `src/server/http.rs` homepage and session routes call `list_sessions_response` / `get_context_response` inside `Runtime::read_repo`.
- `src/server/mcp.rs` dispatch calls `get_context_response` inside `Runtime::read_repo`.
- `src/mcp_response.rs` response builders call `compute_plan_worktree_status`, which reads plan files from the working tree.

Under load, one slow plan-file read or git operation blocks:

- web page renders,
- `wait_for_work` recomputes,
- watcher signal handling,
- SSE state refreshes,
- MCP `get_context` calls.

This is a global serialization bug.

The write path is not the main target of this plan. `Runtime::handle_signal` already has the right broad shape: rebuild or read files before taking the mutex, then hold the mutex only while applying the derived mutation. Audit it during implementation, but keep this plan focused on read-path response building.

## Design Rule

No function passed to `Runtime::read_repo` may perform filesystem I/O, git I/O, async work, sleeps, blocking process calls, or call any helper that does those things.

Enforce the rule structurally rather than relying on discipline:

- Response builders that need worktree status must accept owned snapshot data, not `&RepoState` under a lock.
- Disk-aware helpers must live outside the lock-taking closure.
- The generic `read_repo` helper should be demoted to tests and small pure projections, or replaced with named snapshot methods.

## New Snapshot Types

Add a module such as `src/runtime_snapshot.rs` with owned types cloned under the lock:

```rust
pub struct RepoSnapshot {
    pub root: PathBuf,
    pub head: Option<CommitSha>,
    pub sessions: Vec<SessionSnapshot>,
    pub attribution: BTreeMap<CommitSha, AttributionResult>,
    pub plan_touches: BTreeMap<CommitSha, Vec<(SessionId, PlanTouchKind)>>,
    pub commit_order: Vec<CommitSha>,
}

pub struct SessionSnapshot {
    pub id: SessionId,
    pub plan_path: PathBuf,
    pub body: String,
    pub body_hash: ContentHash,
    pub plan_intro: CommitSha,
    pub plan_intro_parent: Option<CommitSha>,
    pub plan_feedback: BTreeMap<(CommitSha, AgentLabel), Feedback>,
    pub impl_feedback: BTreeMap<(CommitSha, AgentLabel), Feedback>,
    pub held_plan_feedback: Vec<HeldFeedback>,
}

pub struct SessionSnapshotBundle {
    pub root: PathBuf,
    pub head: Option<CommitSha>,
    pub session: SessionSnapshot,
    pub attribution: BTreeMap<CommitSha, AttributionResult>,
    pub plan_touches: BTreeMap<CommitSha, Vec<(SessionId, PlanTouchKind)>>,
    pub commit_order: Vec<CommitSha>,
}
```

`plan_touches` mirrors the current `RepoState::plan_touches` index. Keep the `Vec<(SessionId, PlanTouchKind)>` value shape: a single commit can touch multiple plan files, while implementation attribution remains singular through `AttributionResult`.

`SessionSnapshotBundle` deliberately carries the repo-level indexes needed by pure projections. That keeps the first implementation close to the current projection helpers: snapshot under the lock, release the lock, then derive `plan_revisions`, `implementation_commits`, `timeline`, gates, and waiting state from the owned bundle. A later optimization may precompute per-session lists inside the snapshot, but do not reintroduce I/O under the mutex.

These snapshots may clone more than the absolute minimum at first. The important invariant is that cloning in-memory maps is bounded and predictable; disk and git I/O are not. A typical repo snapshot is on the order of tens of kilobytes (sessions, short SHAs, feedback metadata), which is far below the cost and unpredictability of filesystem and git reads.

If clone cost becomes visible later, optimize snapshot shape by endpoint. Do not reintroduce I/O under the mutex.

## Runtime API

Add explicit snapshot APIs:

```rust
impl Runtime {
    pub async fn snapshot_repo(&self, repo_root: &Path) -> Result<RepoSnapshot, RuntimeError>;
    pub async fn snapshot_session(
        &self,
        repo_root: &Path,
        session_id: &SessionId,
    ) -> Result<Option<SessionSnapshotBundle>, RuntimeError>;
}
```

`snapshot_session` returns `Ok(None)` for an unknown session in a known repo; callers must surface that as the same 404 / `session_not_committed` shape they use today. `RuntimeError` remains reserved for repo-level failures such as an unknown repo root.

Then migrate high-traffic callers:

- `GET /` / `list_sessions`: snapshot all sessions, release lock, compute each `plan_worktree_status`, assemble rows.
- `GET /sessions/:id` / `get_context`: snapshot one session bundle, release lock, compute `plan_worktree_status`, assemble context.
- MCP `get_context`: same as HTTP.
- `GET /sessions/:id/plan_revisions/:sha`: audit only if it keeps using a pure session-existence/path snapshot before calling git outside the lock.
- `GET /sessions/:id/commits/:sha`: audit only if it keeps using a pure session-existence snapshot before calling git outside the lock.
- Any future `/api/sessions/*` UI endpoint: use snapshot APIs from the start.

Keep `Runtime::read_repo` only where the closure is trivially pure and in-memory. Prefer making it private if practical.

## Response Builder Refactor

Split response building into pure and disk-aware parts.

Current shape:

```rust
runtime.read_repo(repo, |s| get_context_response(repo, s, sid, author))
```

New shape:

```rust
let snapshot = runtime.snapshot_session(repo, sid).await?;
let worktree_status = compute_plan_worktree_status_parts(
    repo,
    &snapshot.session.plan_path,
    &snapshot.session.body_hash,
)?;
let response = get_context_response_from_snapshot(snapshot, worktree_status, author);
```

Do the same for `list_sessions_response`.

Use `compute_plan_worktree_status_parts(repo_root, plan_path, body_hash)` as the canonical post-lock helper for plan working-tree status. It already accepts the minimum copied inputs and is used by `wait_for_work`.

The response builders should become pure functions over:

- owned snapshot data,
- precomputed `PlanWorktreeStatus`,
- caller label / request parameters.

## Tests

Add regression coverage at the architecture boundary:

1. `snapshot_repo` / `snapshot_session` return the same logical fields currently used by `list_sessions_response` and `get_context_response`.
2. `list_sessions` and `get_context` responses are byte-for-byte compatible where their public schema is intentionally unchanged.
3. A synthetic slow worktree-status read does not hold the runtime mutex. Practical test shape:
   - introduce a small test seam such as:

```rust
trait PlanStatusReader {
    fn compute(
        &self,
        repo_root: &Path,
        plan_path: &Path,
        body_hash: &ContentHash,
    ) -> std::io::Result<PlanWorktreeStatus>;
}
```

   - production uses the disk reader backed by `compute_plan_worktree_status_parts`,
   - the test reader blocks on a channel after snapshotting,
   - concurrently call a pure runtime read or signal handler that needs the mutex,
   - assert that second operation completes before unblocking the fake status reader.
4. `wait_for_work` behavior stays unchanged.
5. Homepage and session routes still render.

Do not add timing-sensitive tests that depend on machine speed. Use a controlled fake reader or channel gate.

## Implementation Steps

1. Add snapshot structs and conversion helpers from `RepoState`.
2. Add `Runtime::snapshot_repo` and `Runtime::snapshot_session`.
3. Refactor `mcp_response` builders so they have pure `*_from_snapshot` entry points.
4. Update `server/http.rs` and `server/mcp.rs` to snapshot first, then compute worktree status outside the lock.
5. Audit remaining `Runtime::read_repo` call sites and classify each as either pure-safe or migrate it.
6. Demote `Runtime::read_repo` to `pub(crate)` if external callers are gone, or keep it only for tests and explicitly pure in-memory projections.
7. Add tests for multi-session list, single-session context, and the no-I/O-under-lock invariant.
8. Keep the public MCP / HTTP response shapes unchanged unless a separate plan explicitly shrinks them.

## Acceptance Criteria

- No route or MCP tool computes `plan_worktree_status` inside a `Runtime::read_repo` closure.
- `rg "compute_plan_worktree_status\\b" src/server src/mcp_response.rs` shows no full-session status calls inside lock-held closures; `_parts` calls happen only after snapshotting.
- `rg "runtime.read_repo" src/server src/mcp_response.rs src/mcp_shim.rs` shows no non-test route or MCP dispatch callers that can reach disk-aware helpers.
- `wait_for_work` still snapshots and computes disk status outside the lock.
- `cargo test` passes.
- `cargo clippy --all-targets -- -D warnings` passes.
- Manual check: loading `/` and `/sessions/<id>` remains responsive while another request performs repeated worktree-status reads.

## Non-Goals

- No Leptos work.
- No change to the filesystem-truth state model.
- No MCP schema changes.
- No new persistence or migrations.
- No broad async runtime rewrite.

## Follow-Up

After this lands, the Leptos UI plan can build on the snapshot APIs and UI-specific response builders without carrying mutex-boundary work in its scope. New `ui_response::*` builders should be shaped as `*_from_snapshot` functions from day one.
