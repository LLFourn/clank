# Wait-for-work — long-polling MCP + HTTP

## Summary

Add a tool that **blocks until work is available for a given role**, then returns the matching session(s) with everything the caller needs to act. Exposed identically over MCP and HTTP. The agent's loop becomes:

```
loop {
    let work = wait_for_work({ role: "reviewer" })  // blocks
    do_review(work)
}
```

No more polling `get_context` every few seconds. The daemon already has a `tokio::sync::broadcast::Sender<LiveEvent>` (`Runtime::events_tx`) for SSE; this tool subscribes to the same channel + checks current state on entry, returning the first match.

Ships before the Leptos frontend rewrite because agents need this in their loops today; the frontend will use it too (e.g. a "work queue" panel for each role).

## Tool shape

### MCP

```json
{
  "name": "wait_for_work",
  "description": "Block until at least one session in any watched repo is waiting on the caller's role. Returns the match(es) with the canonical write_feedback path and review_target. Re-call after acting.",
  "input_schema": {
    "role": "master | reviewers",
    "repo": "<path>",          // optional — filter to one repo
    "session_id": "<id>",      // optional — filter to one session
    "exclude_authors": ["alice"], // optional — for reviewer polling, skip sessions where caller already left a current verdict
    "timeout_secs": 60          // optional, default 60, max 300
  }
}
```

### HTTP

`POST /api/wait_for_work` with the same JSON body. Same response shape.

### Response

```json
{
  "matches": [
    {
      "repo": "/Users/.../trinity",
      "session_id": "filesystem-truth-rewrite",
      "phase": "implementing",
      "waiting_on": { "role": "reviewers", "reason": "impl_needs_initial_review", "agents": [], "description": "..." },
      "review_target": { "phase": "impl", "commit_sha": "0ea3e04..." },
      "write_feedback": { "phase": "impl", "target_sha": "0ea3e04...", "path": ".trinity/feedback/filesystem-truth-rewrite/impl/0ea3e04.../codex.md" },
      "expected_action": "review_impl"
    }
  ],
  "timed_out": false,
  "matched_at": 1715683200
}
```

Return shape: one entry per session matching the filter. If nothing matches before `timeout_secs`, return `{ matches: [], timed_out: true }`. Caller treats `timed_out: true` as "no work yet; loop again" — they don't have to distinguish "channel closed" from "wait period ended."

## Semantics

- **Immediate return.** Compute matches on entry from the current `Trinity` state. If non-empty, return immediately — don't wait.
- **Subscribe + re-check.** Otherwise subscribe to `Runtime::events_tx`. On each event, re-compute matches. Return on first non-empty result.
- **Timeout.** `tokio::time::timeout` wraps the whole select. On timeout, return `timed_out: true` with `matches: []`.
- **Role match.** Exact match on `waiting_on.role.as_str()`. `none` (terminal sessions) never matches.
- **Multi-repo.** When `repo` is unset, match across all known repos. When set, only that repo.
- **Single-session.** When `session_id` is set, only that session (and `repo` should match if both given).
- **Author exclusion.** When `exclude_authors` is set and `role == "reviewers"`, drop matches where any of the listed authors already has a current verdict for the target (i.e. they're already in `gate.approvals` or `gate.request_changes`). Lets a reviewer poll for "anything I haven't yet reviewed" without being woken by their own work.
- **Deduplication.** When multiple events fire in rapid succession (e.g. a rebuild), the broadcast may deliver duplicates. The match-recompute is idempotent — return shape is the same regardless of how many events fired during the wait. No internal dedup needed.

## Why long-poll, not WebSocket / SSE

- Agents call MCP tools synchronously. Long-poll fits the tool-call model: send request, get response, act. WebSocket requires the agent to maintain a connection across multiple tool calls, which the rmcp SDK doesn't support well.
- The Leptos frontend already uses SSE for live UI updates; that path is unaffected. SSE is for *observing* events as they happen; `wait_for_work` is for *consuming* the "is there anything to do?" question.
- Long-poll is operationally simple: one HTTP connection per outstanding wait. With ≤10 agents per machine, the count is trivially small.

## Server implementation

A new module `src/server/wait.rs` (~150 LOC) exposes:

```rust
pub async fn wait_for_work(
    runtime: &Runtime,
    args: WaitArgs,
) -> WaitResponse;
```

Implementation sketch:

```rust
pub async fn wait_for_work(runtime: &Runtime, args: WaitArgs) -> WaitResponse {
    let timeout = Duration::from_secs(args.timeout_secs.unwrap_or(60).min(300));
    let started_at = Instant::now();
    let mut rx = runtime.subscribe_events();

    // Immediate return path.
    if let Some(matches) = compute_matches(runtime, &args).await {
        return WaitResponse { matches, timed_out: false, matched_at: now() };
    }

    // Block on events until match or timeout.
    let deadline = started_at + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return WaitResponse { matches: vec![], timed_out: true, matched_at: now() };
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(_event)) => {
                if let Some(matches) = compute_matches(runtime, &args).await {
                    return WaitResponse { matches, timed_out: false, matched_at: now() };
                }
                // No match yet; keep waiting.
            }
            Ok(Err(_lagged_or_closed)) => {
                // Lagged or closed — resubscribe and continue.
                rx = runtime.subscribe_events();
            }
            Err(_elapsed) => {
                return WaitResponse { matches: vec![], timed_out: true, matched_at: now() };
            }
        }
    }
}
```

`compute_matches` reads the current `Trinity` state under the lock, iterates `(repo, session)` pairs, applies the role/repo/session/exclude_authors filter, and returns the match list (empty → `None` so we know to keep waiting).

The MCP dispatcher (`src/server/mcp.rs`) gets a new tool entry that deserializes args and calls into `wait_for_work`. The HTTP route (`src/server/http.rs`) adds `POST /api/wait_for_work` that does the same.

## Tool catalog update

Add `wait_for_work` to `src/tools.rs` catalog so the MCP shim advertises it. Tool count goes from 4 to 5.

```rust
ToolDescriptor {
    name: "wait_for_work".to_string(),
    description: "Block until a session needs your role's attention... [see Tool shape above].",
    input_schema: json!({ "type": "object", "required": ["role"], "properties": { ... } }),
}
```

## MCP shim plumbing

The stdio shim already forwards arbitrary tool calls to `/internal/tool_call`. **Two concerns** for long-poll over the shim:

1. **Default request timeout.** The shim builds a `reqwest::Client` with `Duration::from_secs(120)`. Long-poll calls can run up to 300 seconds. Bump per-tool: when `tool == "wait_for_work"`, use `arguments.timeout_secs + 30s` as the request timeout (server timeout + grace).

2. **MCP client timeout.** Some MCP host clients (e.g. Claude Code) have their own tool-call timeout. Document: callers can shorten `timeout_secs` to fit. Default 60s is below typical MCP client thresholds.

The shim is otherwise unchanged.

## Acceptance criteria

- `wait_for_work` returns immediately when matching state exists at entry (no needless blocking).
- `wait_for_work` returns within one debounce window after an event changes a session's `waiting_on.role` to the caller's role.
- `wait_for_work` honors the `repo` and `session_id` filters strictly.
- `wait_for_work` with `role: "reviewers"` and `exclude_authors: ["codex"]` skips sessions where codex already left a current verdict, even if other agents still need to review.
- `timeout_secs` is honored exactly. Returns `timed_out: true` with `matches: []` on the boundary.
- HTTP endpoint and MCP dispatch produce byte-identical JSON responses for the same inputs.
- Concurrent waits (multiple agents polling at once) all unblock on a single event when their filters match. tokio::sync::broadcast handles fan-out.

## Tests

Pure (compute_matches against synthetic RepoState):
- Empty `Trinity` → no matches.
- One session, role matches → one match.
- One session, role doesn't match → empty.
- Two repos, `repo` filter → only that repo's matches.
- `exclude_authors` drops sessions where the listed author is already a participant in the current target's gate.

Integration (against a running runtime + broadcast):
- Spawn a `wait_for_work({ role: "reviewers" })` task. State has no matches. Drop a plan-touch commit + new feedback file that makes a session need reviewer attention. Assert the wait returns within 2s.
- Spawn the wait. Don't deliver any event. After `timeout_secs`, the wait returns `timed_out: true`.
- Two concurrent waits with the same filter. Deliver one event. Both unblock with the same matches.

End-to-end (HTTP):
- Curl `POST /api/wait_for_work` with `timeout_secs: 1` against an idle daemon. Response = `{ matches: [], timed_out: true }` after ~1s.

## Non-goals

- **No push beyond what the runtime already broadcasts.** If a new event type is needed (e.g. for cross-repo notifications), add it to the broadcast separately; this tool subscribes to whatever's there.
- **No persistent queue.** If two agents call `wait_for_work` at the same moment a single match appears, both unblock with the same match. They coordinate via "who writes the feedback file first" + Trinity's auto-organize. No reservation, no exclusive ownership.
- **No per-agent identity.** The caller is anonymous from Trinity's POV. `exclude_authors` is a hint, not enforcement.
- **No streaming partial matches.** One request → one response, then the caller calls again. Server doesn't push incremental updates inside a single wait.
- **No webhook delivery.** Agents pull; daemon doesn't push to external URLs.

## Risks

- **Connection / tool-call timeouts.** The MCP host or HTTP proxy in front of the daemon may cut connections under 300s. Mitigation: default 60s; document the tradeoff.
- **Broadcast lag.** `tokio::sync::broadcast` with a 256-slot buffer drops messages if a slow subscriber falls behind. We resubscribe on lag, then re-compute matches from current state — so we may miss intermediate events but won't miss the final state. Acceptable.
- **Match thrash on noisy repos.** Many file watcher events firing rapidly could re-trigger compute_matches in a tight loop. Each compute is one mutex lock + small map walk; with ≤10 sessions per repo it's negligible. Add tracing if it becomes a problem.

## Open questions

- **Role naming.** The plan's case table uses `master` / `reviewers` / `none`. We use the same strings here. Should `none` be acceptable in the request as a way to "block until everything is idle"? Probably not — leave it out of the schema.
- **Wake-on-no-change.** If a session was already in `master` waiting state when the agent's previous tool call ran (say, `get_context`), should the next `wait_for_work({ role: "master" })` return immediately or block waiting for a state change? **Return immediately** — the immediate-return path handles this. The agent is responsible for taking the action; if they call again without doing the work, they'll see the same match.
