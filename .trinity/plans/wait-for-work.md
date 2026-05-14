# Wait-for-work — long-polling MCP + HTTP

## Summary

Add a tool that **blocks until work is available for a given role**, then returns minimal identifiers for the matching session(s). Exposed identically over MCP and HTTP. The agent's loop becomes:

```
loop {
    let work = wait_for_work({ role: "reviewers" })  // blocks
    for match in work.matches {
        let ctx = get_context({ repo: match.repo, session_id: match.session_id })
        do_review(ctx)
    }
}
```

The `wait_for_work` response is intentionally minimal so a loop's accumulated context stays small even after many wake-ups; the agent fetches full session detail via `get_context` for the one they choose to act on.

No more polling `get_context` every few seconds. The daemon already has a `tokio::sync::broadcast::Sender<LiveEvent>` (`Runtime::events_tx`) for SSE; this tool subscribes to the same channel + checks current state on entry, returning the first match.

Ships before the Leptos frontend rewrite because agents need this in their loops today; the frontend will use it too (e.g. a "work queue" panel for each role).

## Tool shape

### Contract: wake-up only, not context

`wait_for_work` is a thin notifier. Its response carries the **minimum data needed to identify which session to act on**. It does **not** embed `review_gate`, `review_target`, `write_feedback`, `timeline`, or any other field that `get_context` returns. Callers are expected to follow up with `get_context(session_id)` for the session they choose to work on.

This is deliberate: long-poll agents loop and accumulate every response in their context window. A 500-token full-context payload per match becomes 5000 tokens after 10 wake-ups. The minimal shape stays under ~30 tokens per match no matter how often it fires.

### MCP

```json
{
  "name": "wait_for_work",
  "description": "Block until a session in any watched repo is waiting on the caller's role. Returns minimal identifiers; call get_context(session_id) for full state of the chosen session. Re-call after acting.",
  "input_schema": {
    "type": "object",
    "required": ["role"],
    "additionalProperties": false,
    "properties": {
      "role": { "type": "string", "enum": ["master", "reviewers"] },
      "repo": { "type": "string", "description": "Filter to one repo (absolute path)." },
      "session_id": { "type": "string" },
      "exclude_authors": {
        "type": "array",
        "items": { "type": "string" },
        "description": "Reviewer polling only: skip sessions where any of these authors already has a current verdict for the target."
      },
      "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 300, "default": 60 }
    }
  }
}
```

**Role naming.** `role` accepts exactly the strings `master` and `reviewers` — matching the strings emitted by `waiting_on.role.as_str()`. No `reviewer` singular alias; the schema's `enum` rejects it cleanly. Example loops and prose in this plan use `reviewers` consistently.

### HTTP

`POST /api/wait_for_work` with the same JSON body. Same response shape.

### Response (minimal)

```json
{
  "matches": [
    {
      "repo": "/Users/.../trinity",
      "session_id": "filesystem-truth-rewrite",
      "reason": "impl_needs_initial_review"
    }
  ],
  "timed_out": false
}
```

Three fields per match:
- `repo` — the absolute repo path. Required because session ids are repo-scoped.
- `session_id` — the session that needs attention.
- `reason` — the `waiting_on.reason` string (e.g., `impl_needs_initial_review`, `address_plan_request_changes`). Lets callers prioritize without a follow-up call (e.g., a master agent might process `address_*_request_changes` before `commit_done_move`).

Nothing else. No phase, no SHAs, no paths, no descriptions. **The agent calls `get_context({ session_id, repo })` for the session they pick.**

If nothing matches before `timeout_secs`, return `{ "matches": [], "timed_out": true }`. Callers treat `timed_out: true` as "no work yet; loop again." No `matched_at` field — the caller can clock-time the result themselves if they need it.

## Semantics

- **Immediate return.** Compute matches on entry from the current `Trinity` state. If non-empty, return immediately — don't wait.
- **Subscribe + re-check.** Otherwise subscribe to `Runtime::events_tx`. On each event, re-compute matches. Return on first non-empty result.
- **Timeout.** `tokio::time::timeout` wraps the whole select. On timeout, return `timed_out: true` with `matches: []`.
- **Role match.** Exact match on `waiting_on.role.as_str()`. `none` (terminal sessions) never matches.
- **Multi-repo.** When `repo` is unset, match across all known repos. When set, canonicalize via `dunce::canonicalize` and compare against `Trinity.repos` keys (which are already canonical) — symlinks and case-normalized variants all match. **`~/...` is NOT expanded by `dunce::canonicalize`**; callers must pass absolute paths (the same convention `~/.trinity/repos` uses on the daemon side, expanded at startup). Non-existent or unresolvable paths return an empty match list (don't error).
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
    let initial = compute_matches(runtime, &args).await;
    if !initial.is_empty() {
        return WaitResponse { matches: initial, timed_out: false };
    }

    // Block on events until match or timeout.
    let deadline = started_at + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return WaitResponse { matches: vec![], timed_out: true };
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(_event)) => {
                let next = compute_matches(runtime, &args).await;
                if !next.is_empty() {
                    return WaitResponse { matches: next, timed_out: false };
                }
                // No match yet; keep waiting.
            }
            Ok(Err(_lagged_or_closed)) => {
                // Lagged or closed — resubscribe and continue.
                rx = runtime.subscribe_events();
            }
            Err(_elapsed) => {
                return WaitResponse { matches: vec![], timed_out: true };
            }
        }
    }
}
```

### Lock boundary for `compute_matches`

The matcher must consider `plan_worktree_status` correctly — the master waits on `commit_plan_revision` / `commit_done_move` / `restore_or_commit_done_move` are all worktree-state-driven, and missing those wakeups defeats the tool. But `plan_worktree_status` is a disk read (compare working-tree plan body to HEAD's blob hash). We can't do disk reads under the runtime mutex without blocking watchers and other handlers.

**Solution — snapshot under lock, compute outside.** Two phases:

```rust
struct Candidate {
    repo_root: PathBuf,
    repo_state_clone: RepoStateSummary, // small, just identifiers + gate state
    session_id: SessionId,
    session_clone: SessionSummary,       // body_hash, plan_path, feedback lists
}

async fn compute_matches(runtime: &Runtime, args: &WaitArgs) -> Vec<Match> {
    // Phase 1: under lock, collect candidates + the cheap derived bits.
    let candidates = {
        let trinity = runtime.state().lock().await;
        collect_candidates(&trinity, args)
    };
    // Lock released here.

    // Phase 2: per-candidate disk reads (outside lock).
    let with_status: Vec<(Candidate, PlanWorktreeStatus)> = candidates
        .into_iter()
        .map(|c| {
            let status = compute_plan_worktree_status(&c.repo_root, &c.session_clone);
            (c, status)
        })
        .collect();

    // Phase 3: pure matching against the materialized inputs.
    match_candidates(&with_status, args)
}

/// Pure inner matcher. No I/O, no locks. Takes already-resolved candidates +
/// their `plan_worktree_status` and returns the response matches.
fn match_candidates(input: &[(Candidate, PlanWorktreeStatus)], args: &WaitArgs) -> Vec<Match> {
    let mut out = Vec::new();
    for (cand, status) in input {
        let w = projection::waiting_on(
            cand.session_clone.phase,
            *status,
            cand.session_clone.plan_gate.as_ref(),
            cand.session_clone.impl_gate.as_ref(),
        );
        if w.role.as_str() != args.role {
            continue;
        }
        if matches!(args.role.as_str(), "reviewers")
            && args.exclude_authors.iter().any(|a| cand.session_clone.has_current_verdict_from(a))
        {
            continue;
        }
        out.push(Match {
            repo: cand.repo_root.to_string_lossy().into_owned(),
            session_id: cand.session_id.as_str().to_string(),
            reason: w.reason.as_str().to_string(),
        });
    }
    out
}

fn collect_candidates(trinity: &Trinity, args: &WaitArgs) -> Vec<Candidate> {
    let mut out = Vec::new();
    let canonical_filter = args.repo.as_ref().map(canonical_repo_path);
    for (repo_root, repo_state) in &trinity.repos {
        if let Some(filter) = &canonical_filter
            && filter != repo_root
        {
            continue;
        }
        for (session_id, session) in &repo_state.sessions {
            if let Some(filter_sid) = &args.session_id
                && filter_sid.as_str() != session_id.as_str()
            {
                continue;
            }
            let phase = projection::phase(session, &repo_state.attribution);
            let plan_gate = projection::plan_gate_for(session, repo_state);
            let impl_gate = projection::impl_gate_for(session, repo_state);
            out.push(Candidate {
                repo_root: repo_root.clone(),
                session_id: session_id.clone(),
                session_clone: SessionSummary {
                    body_hash: session.body_hash.clone(),
                    plan_path: session.plan_path.clone(),
                    phase,
                    plan_gate,
                    impl_gate,
                    // ...verdict-author lookups precomputed for exclude_authors
                },
                ..
            });
        }
    }
    out
}
```

**Lock-held cost**: one pass over `(repo, session)` pairs, plus a `plan_gate_for` / `impl_gate_for` call per session. Each gate-derivation is a small BTreeMap walk over `plan_feedback` / `impl_feedback`. For ≤10 sessions per repo, sub-millisecond. The lock is held briefly even with many repos.

**Outside-lock cost**: one `compute_plan_worktree_status` per candidate = one `git show HEAD:<plan_path>` (cached blob hash already on `Session`) + one `fs::read(<plan_path>)`. Both are fast (<5ms each), parallelizable if it ever matters.

**Why not cache `plan_worktree_status` on `Session`?** The plan's existing rule says plan_worktree_status is a derived projection computed at every read — `Session` never stores it. Caching it on `RepoState` would invert that invariant and require explicit invalidation on file edits. The watcher's `PlanFileChanged` event already drives a broadcast that triggers `compute_matches` re-runs; the disk read is cheap enough that "recompute each time" beats "cache + invalidate."

**`projection` module additions.** Moving `plan_gate_for`, `impl_gate_for`, and `waiting_on` from `mcp_response.rs` to `projection.rs` is a prerequisite (code-org only, no behavior change). Part of Phase 1.

The MCP dispatcher (`src/server/mcp.rs`) gets a new tool entry that deserializes args and calls into `wait_for_work`. The HTTP route (`src/server/http.rs`) adds `POST /api/wait_for_work` that does the same.

## Prerequisite: `repo` arg on `get_context` and `list_sessions`

`wait_for_work` can return a match in *any* watched repo, not just the caller's cwd-repo. The agent's follow-up `get_context({ repo: match.repo, session_id: match.session_id })` requires `get_context` to accept the repo explicitly — currently it resolves the repo from `req.cwd` via `git rev-parse --show-toplevel`.

Add an optional `repo` arg to both tools:

- `get_context({ session_id, author_label?, repo? })` — when `repo` is provided, canonicalize and look up; otherwise fall back to `req.cwd` resolution.
- `list_sessions({ repo? })` — when set, return only that repo's sessions; otherwise all known repos.

Schema update in `src/tools.rs`; dispatcher update in `src/server/mcp.rs`. Same `dunce::canonicalize` normalization the runtime already uses for `add_repo` / `read_repo`. Phase 1 of this plan includes the change.

## Tool catalog update

Add `wait_for_work` to `src/tools.rs` catalog so the MCP shim advertises it. Tool count goes from 4 to 5. `get_context` and `list_sessions` schemas updated to include optional `repo`.

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

Pure (`match_candidates` against synthetic `Vec<(Candidate, PlanWorktreeStatus)>`):
- Empty input → no matches.
- One candidate, role matches → one match.
- One candidate, role doesn't match → empty.
- Multiple candidates across two repo roots, `repo` filter pre-applied in `collect_candidates` → only that repo's matches (the pure matcher doesn't re-filter; the test exercises pass-through).
- `exclude_authors` drops sessions where the listed author is already a participant in the current target's gate.
- `PlanWorktreeStatus::BodyDirty` on an implementing-ready session yields the expected `commit_plan_revision` / `commit_done_move` master match (the case that motivated the disk-read split — easy to cover here since the pure matcher takes status as input).

Fixture (`collect_candidates` + `compute_plan_worktree_status` together via tempdir):
- One synthetic repo with a committed plan file and a dirty working copy → `compute_matches` returns a master match with the correct dirty-reason.
- Clean working copy → no master dirty-reason match.

These two tempdir tests verify the I/O glue once; the pure matcher gets exhaustive coverage without disk.

Integration (against a running runtime + broadcast). Each test pins a single crisp transition so the wake-up cause is unambiguous:

- **Wake on new plan revision needing review**: state starts with one session in implementing/ready_to_finish (waiting on master). Spawn `wait_for_work({ role: "reviewers" })`. While blocked, commit a new plan revision against another session that lands it in `plan_needs_initial_review` (waiting on reviewers). Assert the wait returns within 2s with that session in `matches`.
- **Wake on REQUEST_CHANGES moving the role to master**: state has a session in `plan_needs_initial_review`. Spawn `wait_for_work({ role: "master" })`. While blocked, drop a feedback file with `REQUEST_CHANGES` for the current plan target. Assert the wait returns within 2s with that session in `matches` and `reason: "address_plan_request_changes"`.
- **exclude_authors skips self-reviewed sessions**: same setup as the first test, but the polling caller is alice and alice already wrote an APPROVE for the current target. Wait should NOT return for that session.
- **Timeout**: spawn the wait, don't deliver any event. After `timeout_secs` (set to 1s), the wait returns `{ matches: [], timed_out: true }`.
- **Fan-out**: two concurrent waits with the same filter. Deliver one event that creates a match. Both unblock with the same `matches`.

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
