# wfw-local

Step 1 of OMEGA: make `trinity wfw` an end-to-end local CLI. No daemon,
no HTTP, no `reqwest`. Fold the repo locally, attach the same
`notify_bridge` watcher the daemon uses, run the existing
`server::wait::wait_for_work` matching loop, print the result, exit.

## Why (OMEGA framing)

The daemon was the historical bridge between MCP-over-stdio and a
filesystem-truth runtime that needed to outlive a single tool call.
Now that:

- The full fold runs in ~20ms warm (`state_cache`, landed in
  `cache-core-fold-and-live-feedback.md`).
- The CLI already does its mutations (`finish`, `purge`, `status`)
  directly without the daemon.
- The MCP shim is the only consumer that still requires a live
  HTTP endpoint, and the only reason it does is to share the
  watcher subscription used by `wait_for_work`.

…the daemon's reason-to-exist has collapsed to "long-running
process holding a notify watcher so wait_for_work doesn't have to
spin one up per call."

OMEGA's endgame: delete the daemon. `trinity mcp` becomes a thin
in-process MCP handler. `trinity wfw` becomes a CLI that does one
wait round-trip and exits. Every tool — `start_plan`, `list_plans`,
`work_context`, `wait_for_work`, … — runs against a per-invocation
Runtime backed by the local repo.

This plan is the **first** step: prove the model by implementing
`trinity wfw` standalone. After it lands and bakes, follow-up
plans rip out HTTP, the frontend bundle, `build.rs`, `include_dir`,
`reqwest`, `axum`, `tower-http`, `rmcp`'s transport shim, and the
daemon-spawn dance in `mcp_shim`.

## What

New CLI subcommand:

```
trinity wfw <plan>
    --role <master|reviewers>
    --author <label>
    [--repo PATH]              # default: git toplevel of cwd
    [--timeout-secs N]         # default: 1800 (matches daemon)
    [--no-cache]               # bypass state cache (timing / debug)
    [--json | -j]              # JSON output (default: human-readable)
```

`<plan>` accepts either `<basename>/<stem>.md` (canonical) or a
bare stem (`wfw-local`, `wfw-local.md`). Bare stems are normalized
to `<cwd-repo-basename>/<stem>.md`.

### Runtime behavior

1. Resolve repo via the existing `cli::resolve_repo` helper.
2. Build an `Arc<Runtime>`. Call `runtime.add_repo(repo)`. This
   reuses the existing `rebuild_repo` path (cache-aware).
3. Spawn `notify_bridge::start(Arc::clone(&runtime), repo.clone())`
   so feedback writes / HEAD changes feed the broadcast channel
   `wait_for_work` is already subscribed to.
4. Build a `wait::WaitArgs` from the CLI args and call
   `wait::wait_for_work(&runtime, args).await`.
5. Print the response. `WaitResponse::Work(payload)` → render
   payload (human or JSON). `WaitResponse::Timeout(_)` →
   `no work; timed out after Ns` to stderr, JSON `{"timed_out":
   true, "no_active_plans": ...}` to stdout under `--json`.
6. Drop watcher (process exit). Notify task dies with the
   `Arc<Runtime>`.

### Human render (default)

```
plan      trinity/wfw-local.md
role      master
action    address_changes (1 review at .trinity/feedback/wfw-local/abc1234/codex.md)
plan      .trinity/plans/wfw-local.md
```

JSON output is `serde_json::to_string_pretty(&WaitResponse)`.

## Implementation outline

- `src/cli/mod.rs`: add `WfwArgs` struct + `pub mod wfw;`.
- `src/cli/wfw.rs`: the runner. Pure orchestration; pulls
  `Runtime`, `server::wait::wait_for_work`, and
  `server::notify_bridge::start` directly. No new abstractions.
- `src/main.rs`: wire `Command::Wfw(args)` → `cli::wfw::run(args)`.

### Plan-id normalization

Trivial: if `<plan>` contains `/`, treat as canonical and validate
via `PlanId::parse`. Otherwise prepend `<basename>/` (stripping a
trailing `.md` if present and re-adding it). The CLI ergonomics
match `trinity finish` / `trinity purge` already.

### Cross-module imports

`server::wait::wait_for_work` and `server::notify_bridge::start`
are already `pub` and don't depend on HTTP. The CLI calls them
directly. No restructure in this plan — moving `server::wait` and
`server::notify_bridge` out of `server/` belongs to the
daemon-removal follow-up, not this one.

## Behavioral notes (architecture-level)

- **Stale-review dedup**: the daemon tracks
  `Trinity.seen_stale_reviews` (`BTreeSet`) so a `wait_for_work`
  match against the master role doesn't re-send the same
  superseded review every poll. CLI `wfw` runs in a fresh process
  every invocation; the set is empty at startup. Net effect: a
  master who calls `trinity wfw` repeatedly will see the same
  stale review attached on each call until the underlying SHA
  changes. **Documented gap, not a fix for this plan.** A
  follow-up can persist the set under `.trinity/cache/wfw-state/`
  if loop ergonomics demand it; for now the agent loop already
  reads each stale review on first sight and idempotency is on
  the consumer side.

- **Opportunistic body cache**: the daemon's
  `Trinity.opportunistic_bodies` deduplicates inline content
  bodies to avoid re-sending them across many HTTP polls. CLI
  has no equivalent problem — one call, one response. v1 always
  inlines content (subject to the existing 64 KiB cap). Removing
  this special-case in the CLI path simplifies the code; the
  field stays optional on the wire so consumers don't change.

- **Plan-id inference** (the daemon's "single active plan in the
  cwd-repo" fallback) is out of v1. Pass an explicit plan. The
  daemon's inference adds complexity around active-selection state
  that the CLI shouldn't carry until we know it's needed.

- **Timeout floor**: defaults to 1800s to match the daemon.
  Floors at 1s. No upper cap (consistent with the daemon).

## Test plan

End-to-end test (gated by `cfg(test)`, lives under `tests/` or in
`src/cli/wfw.rs`'s own module):

1. Init a git repo + plan file via the existing test helpers.
2. Spawn `trinity wfw <plan> --role reviewers --author alice
   --json --timeout-secs 30` as a tokio task (NOT subprocess —
   call `cli::wfw::run` directly so we can assert on the result
   structure rather than parsing stdout).
3. From a second task, write a feedback file that creates work
   for `alice`. (Or commit a reviewable change that wakes
   reviewers.)
4. Assert the call returns within a few seconds with the
   expected `WaitResponse::Work` payload.
5. Negative case: don't generate work; assert
   `WaitResponse::Timeout` after `timeout_secs`.

The existing `wait_for_work` unit tests cover the matching
algorithm; we don't duplicate them. The new test covers the
CLI-wires-watcher-to-wait integration.

## Acceptance

- `trinity wfw <plan> --role <r> --author <a>` runs without the
  daemon: no port bound, no daemon child process spawned, no
  `reqwest` call.
- A reviewer-role wait wakes within debounce-window latency of a
  feedback write under the plan.
- A master-role wait wakes within debounce-window latency of a
  HEAD change.
- `--timeout-secs N` honored.
- `--json` emits a parseable `WaitResponse` JSON document.
- Existing `trinity serve` daemon still works untouched.

## Out of scope (deferred to follow-up OMEGA plans)

- Removing `trinity serve` + `src/server/*` + `build.rs` +
  frontend bundle + `include_dir` + `axum` / `tower-http` /
  `reqwest` / `rmcp` transport bits. Sequenced after `wfw` and
  the in-process MCP server land.
- Replacing the `trinity mcp` stdio shim with an in-process MCP
  server. Next plan.
- Persisting `seen_stale_reviews` / `opportunistic_bodies`
  across CLI invocations.
- Restructuring `server::wait` / `server::notify_bridge` out of
  `server/` into top-level modules. Cosmetic; happens during the
  daemon teardown.
