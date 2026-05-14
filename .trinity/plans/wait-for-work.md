# Wait-for-work — long-polling MCP + HTTP

## Summary

Add a tool that **blocks until one named session needs the caller's role**, then returns the action to perform and the file paths to act on. Exposed identically over MCP and HTTP.

```
loop {
    let r = wait_for_work({
        role: "reviewers",
        session_id: "my-feature",
        author_label: "codex",
    });
    if r.timed_out { continue; }
    // r.work == "review_impl"
    // r.locations[0] == ".trinity/feedback/my-feature/impl/<sha>/codex.md"
    // Read the impl commit + write your verdict to that path.
}
```

Single-session, single-repo by design: an agent has one shell, one cwd, one project. Cross-repo wake-ups aren't actionable from a single agent's POV, so the tool doesn't surface them. If an agent wants to watch multiple sessions in parallel they call `wait_for_work` once per session_id.

No more polling `get_context` / `list_sessions` on a timer. The daemon already has a `tokio::sync::broadcast::Sender<LiveEvent>` (`Runtime::events_tx`) for SSE; this tool subscribes to the same channel + checks current state on entry.

Ships before the Leptos frontend rewrite because agents need this in their loops today; the frontend will use it too (e.g. a "work queue" panel for a session).

## Tool shape

### Contract: action + locations, not full context

`wait_for_work` returns the **action** and the **paths to read or write**. It does NOT embed `review_gate`, `timeline`, plan body, or any other field that `get_context` returns. Callers who need richer context call `get_context({session_id, repo?})` after deciding to act.

This is deliberate: long-poll agents loop and accumulate every response in their context window. A 500-token full-context payload becomes 5000 tokens after 10 wake-ups. The minimal `{work, locations}` shape stays under ~50 tokens per response no matter how often it fires.

### MCP / HTTP request schema

```json
{
  "name": "wait_for_work",
  "input_schema": {
    "type": "object",
    "required": ["role", "session_id", "author_label"],
    "additionalProperties": false,
    "properties": {
      "role":         { "type": "string", "enum": ["master", "reviewers"] },
      "session_id":   { "type": "string" },
      "author_label": { "type": "string" },
      "repo":         { "type": "string", "description": "Optional over MCP (defaults to cwd-repo); required over HTTP." },
      "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 300, "default": 60 }
    }
  }
}
```

- `role` accepts exactly `master` or `reviewers`. The schema's `enum` rejects anything else.
- `session_id` is required; the response is always for this one session.
- `author_label` is required because it's baked into the canonical write path for `review_*` work (`.trinity/feedback/<sid>/<phase>/<sha>/<author>.md`). No silent `anonymous` fallback. The MCP shim caches it across calls (same mechanism `get_context` already uses), so callers usually pass it once per shim lifetime.
- `repo` is optional over MCP — the dispatcher falls back to the caller's cwd-repo via `git rev-parse --show-toplevel`. It's required over HTTP (no cwd context to fall back on).

### HTTP

`POST /api/wait_for_work` with the same JSON body. Same response shape.

### Response

Two possible shapes — an untagged enum in the JSON.

Work available:

```json
{
  "work": "review_impl",
  "locations": [".trinity/feedback/my-feature/impl/abc123/codex.md"]
}
```

Timed out:

```json
{ "timed_out": true }
```

`work` is one of the imperative actions in the [`expected_action` projection](../../src/projection.rs). `locations` is a list of repo-relative paths. Meaning depends on `work`:

| `work` | `locations` |
| --- | --- |
| `review_plan` | `[<canonical write path for caller's plan-phase feedback>]` |
| `review_impl` | `[<canonical write path for caller's impl-phase feedback>]` |
| `address_plan_request_changes` | `[<each REQUEST_CHANGES feedback on current plan target>, <plan file>]` |
| `address_impl_request_changes` | `[<each REQUEST_CHANGES feedback on current impl target>]` (no plan file — the caller is addressing in code) |
| `commit_plan_revision` | `[<plan file>]` |
| `commit_done_move` / `restore_or_commit_done_move` | `[<plan file>]` |
| `implement_and_commit` | `[<plan file>]` |
| `move_to_done` | `[<plan file>]` |

## Semantics

- **Immediate return.** Compute the work item on entry from the current state. If a match exists, return immediately.
- **Subscribe + re-check.** Otherwise subscribe to `Runtime::events_tx`. On each event, re-compute. Return on the first match.
- **Timeout.** `tokio::time::timeout` wraps the whole select. On timeout, return `{timed_out: true}`.
- **Role match.** Exact match on `waiting_on.role`. `none` (terminal sessions) never matches.
- **Repo resolution.** When `repo` is unset on the MCP path, resolve from cwd via `git rev-parse --show-toplevel`. When set, canonicalize via `dunce::canonicalize`. `~/...` is NOT expanded; pass absolute paths. The HTTP route rejects requests without `repo` because it has no cwd context.
- **Unknown session.** If the named `session_id` doesn't exist in the target repo, return an error (`session not found`) rather than blocking forever — the caller has a bug we should surface.
- **Deduplication.** When multiple events fire in rapid succession (e.g. a rebuild), the broadcast may deliver duplicates. The match-recompute is idempotent — return shape is the same regardless of how many events fired during the wait.

## Why long-poll, not WebSocket / SSE

- Agents call MCP tools synchronously. Long-poll fits the tool-call model: send request, get response, act. WebSocket requires the agent to maintain a connection across multiple tool calls, which the rmcp SDK doesn't support well.
- The Leptos frontend already uses SSE for live UI updates; that path is unaffected. SSE is for *observing* events as they happen; `wait_for_work` is for *consuming* the "is there anything to do?" question.
- Long-poll is operationally simple: one HTTP connection per outstanding wait. With ≤10 agents per machine, the count is trivially small.

## Server implementation

A new module `src/server/wait.rs` exposes:

```rust
pub async fn wait_for_work(
    runtime: &Runtime,
    args: WaitArgs,
) -> Result<WaitResponse, WaitError>;
```

Sketch:

```rust
pub async fn wait_for_work(runtime: &Runtime, args: WaitArgs) -> Result<WaitResponse, WaitError> {
    // ... validate role, session_id, author_label, repo ...
    let mut rx = runtime.subscribe_events();
    if let Some(w) = compute_match(runtime, &repo, &session_id, role, &author).await? {
        return Ok(WaitResponse::Work { work: w.work.into(), locations: w.locations });
    }
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() { return Ok(WaitResponse::Timeout { timed_out: true }); }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(_)) | Ok(Err(broadcast::error::RecvError::Lagged(_))) => {
                if let Some(w) = compute_match(runtime, &repo, &session_id, role, &author).await? {
                    return Ok(WaitResponse::Work { work: w.work.into(), locations: w.locations });
                }
            }
            Ok(Err(broadcast::error::RecvError::Closed)) | Err(_) => {
                return Ok(WaitResponse::Timeout { timed_out: true });
            }
        }
    }
}
```

### Lock boundary for `compute_match`

The matcher must consider `plan_worktree_status` correctly — the master waits on `commit_plan_revision` / `commit_done_move` / `restore_or_commit_done_move` are all worktree-state-driven, and missing those wake-ups defeats the tool. But `plan_worktree_status` is a disk read (compare working-tree plan body to HEAD's blob hash). We can't do disk reads under the runtime mutex without blocking watchers and other handlers.

**Solution — snapshot under lock, compute outside.** Three phases:

```rust
async fn compute_match(
    runtime: &Runtime,
    repo: &Path,
    session_id: &SessionId,
    role: WaitingRole,
    author: &AgentLabel,
) -> Result<Option<WorkItem>, WaitError> {
    // Phase 1: under lock, snapshot the candidate.
    let candidate = {
        let trinity = runtime.state().lock().await;
        collect_candidate(&trinity, repo, session_id)
    };
    let Some(candidate) = candidate else { return Err(WaitError::UnknownSession(...)) };

    // Phase 2: disk read (outside lock).
    let status = compute_plan_worktree_status_parts(&candidate.repo_root, &candidate.plan_path, &candidate.body_hash)?;

    // Phase 3: pure derivation.
    let w = projection::waiting_on(candidate.session_phase, status, candidate.plan_gate.as_ref(), candidate.impl_gate.as_ref());
    if w.role != role { return Ok(None); }
    Ok(Some(WorkItem {
        work: projection::expected_action(w.reason),
        locations: derive_locations(&candidate, w.reason, author),
    }))
}
```

`Candidate` carries the cheap, lock-friendly fields: `repo_root`, `session_id`, `plan_path`, `body_hash`, `session_phase`, `plan_gate`, `impl_gate`, `plan_target`, `impl_target`. `derive_locations` is a pure function over the candidate + reason + author.

**Lock-held cost**: one repo + session lookup, plus `plan_gate_for` / `impl_gate_for` (a small BTreeMap walk over `plan_feedback` / `impl_feedback`) and the two latest-commit walks. Sub-millisecond.

**Outside-lock cost**: one `compute_plan_worktree_status_parts` per poll = one `fs::read(<plan_path>)`. Fast (<5ms).

**`projection` module additions.** `plan_gate_for`, `impl_gate_for`, `all_plan_revisions`, `all_implementation_commits`, `latest_plan_touching_commit`, `latest_impl_commit`, and `expected_action` live in `projection.rs` so `wait.rs` and `mcp_response.rs` share them.

The MCP dispatcher (`src/server/mcp.rs`) gets a new tool entry that deserializes args, fills `repo` from cwd if absent, and calls into `wait_for_work`. The HTTP route (`src/server/http.rs`) adds `POST /api/wait_for_work` with the same body shape; it returns 400 if `repo` is absent.

## Repo arg on `get_context`

`get_context` accepts an optional `repo` (absolute path). When set, canonicalize and look up; otherwise fall back to `req.cwd` resolution. Same `dunce::canonicalize` normalization the runtime already uses for `add_repo` / `read_repo`.

`list_sessions` keeps its current cwd-repo default (a single agent works in one repo). The `repo` arg is optional for the rare case of inspecting another watched repo.

## Tool catalog update

Add `wait_for_work` to `src/tools.rs` catalog so the MCP shim advertises it. Tool count goes from 4 to 5. `get_context` updated to include optional `repo`.

## MCP shim plumbing

The stdio shim already forwards arbitrary tool calls to `/internal/tool_call`. **Two concerns** for long-poll:

1. **Default request timeout.** The shim builds a `reqwest::Client` with `Duration::from_secs(120)`. Long-poll calls can run up to 300 seconds. Per-tool override: when `tool == "wait_for_work"`, set the per-request timeout to `args.timeout_secs + 30s`.

2. **`author_label` autofill.** Extend `label_arg_for` so `wait_for_work` participates in the existing cache-on-first-use mechanism (same as `get_context`). Callers pass it once; subsequent calls inherit.

3. **MCP client timeout.** Some MCP host clients (e.g. Claude Code) have their own tool-call timeout. Document: callers can shorten `timeout_secs` to fit. Default 60s is below typical MCP client thresholds.

## Acceptance criteria

- `wait_for_work` returns immediately when matching state exists at entry.
- `wait_for_work` returns within one debounce window after an event changes the named session's `waiting_on.role` to the caller's role.
- `work` matches `expected_action(waiting_on.reason)` exactly.
- `locations` for `review_*` is the single canonical write path with the caller's `author_label` baked in.
- `locations` for `address_*_request_changes` lists every RC feedback file on the current target, in deterministic order; plan-phase adds the plan file at the end.
- Unknown `session_id` returns a `not found` error (does not block).
- Missing required fields return `400` over HTTP and the equivalent `invalid` error over MCP.
- `timeout_secs` is honored exactly. Returns `{timed_out: true}` on the boundary.
- HTTP endpoint and MCP dispatch produce byte-identical JSON responses for the same inputs (modulo MCP's `{result: ...}` envelope).

## Tests

Pure (`derive_locations` + `parse_role` against synthetic candidates):
- `review_plan` returns the canonical write path with the requested author baked in.
- `review_impl` uses the impl target SHA.
- `review_plan` returns empty when there's no plan target (defensive).
- `address_plan_request_changes` lists every RC author's feedback file, then the plan file.
- `address_impl_request_changes` lists every RC author's feedback file (no plan file).
- `commit_plan_revision` / `ready_to_finish` / `ready_to_implement` return the plan file.
- `session_done` returns empty locations.
- `parse_role` accepts `master` / `reviewers`, rejects everything else (including `reviewer` singular).

Integration (against a real `Runtime` over tempdir git repos):
- **Immediate review_plan after first commit.** New session, no reviews → `review_plan` with the canonical write path.
- **`body_dirty` yields `commit_plan_revision`.** The disk-read split exists for this case: in-memory state thinks the plan is clean, but the working copy has uncommitted edits.
- **`address_plan_request_changes` lists RC files + plan file.** Two RC feedbacks land via `FeedbackWritten` signals; the response lists both in deterministic order then the plan file.
- **Timeout.** Polling `master` while only reviewers have work → `{timed_out: true}` after `timeout_secs`.
- **Wakes on `PlanFileChanged`.** Spawn the wait; while blocked, edit the plan + dispatch `PlanFileChanged`; the wait returns with `commit_plan_revision`.
- **Unknown session** errors with `WaitError::UnknownSession`.
- **Missing repo / author_label** errors with `WaitError::MissingRepo` / `WaitError::MissingAuthorLabel`.

End-to-end (live daemon):
- `POST /api/wait_for_work` with `timeout_secs: 1` against an idle daemon returns `{timed_out: true}` after ~1s.
- Same request against a session in `ready_to_finish` returns `{work: "move_to_done", locations: [<plan file>]}`.

## Non-goals

- **No multi-session matches.** One request → one session. Callers run multiple `wait_for_work` calls in parallel if they want to watch several sessions.
- **No cross-repo matches.** One request → one repo. An agent's shell is repo-scoped.
- **No push beyond what the runtime already broadcasts.** If a new event type is needed, add it to the broadcast separately; this tool subscribes to whatever's there.
- **No persistent queue.** If two agents call `wait_for_work` at the same moment a single match appears, both unblock with the same work item. They coordinate via "who writes the feedback file first" + Trinity's auto-organize.
- **No streaming partial matches.** One request → one response, then the caller calls again.
- **No webhook delivery.** Agents pull; daemon doesn't push to external URLs.

## Risks

- **Connection / tool-call timeouts.** The MCP host or HTTP proxy in front of the daemon may cut connections under 300s. Mitigation: default 60s; document the tradeoff.
- **Broadcast lag.** `tokio::sync::broadcast` with a 256-slot buffer drops messages if a slow subscriber falls behind. We resubscribe on lag, then re-compute from current state — so we may miss intermediate events but won't miss the final state. Acceptable.
- **Match thrash on noisy repos.** Many file watcher events firing rapidly could re-trigger `compute_match` in a tight loop. Each compute is one mutex lock + small map walk + one disk read; with ≤10 sessions per repo and bounded disk I/O it's negligible.

## Open questions

- **Wake-on-no-change.** If the session is already in the caller's role state when they call `wait_for_work`, the immediate-return path fires the same work item. The caller is responsible for taking the action; if they call again without doing the work, they'll see the same response. Document, don't fight.
- **Per-author location ordering for `address_*_request_changes`.** Today it's BTreeMap iteration order over `(target_sha, author)` keys → sorted by author name. Stable, deterministic, good enough.
