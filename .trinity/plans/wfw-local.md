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
trinity wfw [plan]
    --role <master|reviewers>
    --author <label>
    [--repo PATH]              # default: git toplevel of cwd
    [--timeout-secs N]         # default: 1800 (matches daemon)
    [--no-cache]               # bypass state cache (timing / debug)
    [--json | -j]              # JSON output (default: human-readable)
```

`[plan]` is optional. When supplied, it accepts either
`<basename>/<stem>.md` (canonical) or a bare stem (`wfw-local`,
`wfw-local.md`); bare stems are normalized to
`<cwd-repo-basename>/<stem>.md`. When omitted, the CLI uses the
existing `cli::plan_resolve::resolve_plan` to infer the single
active visible plan in the cwd-repo, raising a structured error
with the candidate list when multiple are active — exactly the
behavior `trinity finish` / `trinity purge` already implement.
`wfw` is the most-repeated command in the agent loop; it must
not require more typing than `finish`.

### Runtime behavior

1. Resolve repo via the existing `cli::resolve_repo` helper.
2. Build an `Arc<Runtime>`. Call
   `runtime.add_repo_with_policy(repo, policy)` where `policy`
   is `CachePolicy::Bypass` under `--no-cache`, else
   `CachePolicy::Use`. This requires a small additive API on
   `Runtime`: `add_repo_with_policy(repo, CachePolicy)`.
   `add_repo(repo)` becomes a thin wrapper that delegates with
   `CachePolicy::Use`. No daemon-side caller changes.
3. **Arm the watcher next**: spawn
   `notify_bridge::start(Arc::clone(&runtime), repo.clone()).await?`.
   `Debouncer::watch()` is sync and returns once the OS watch is
   registered; from that point forward, filesystem changes
   produce signals.
4. **Re-fold after arming** to close the race window: any change
   that landed between step 2's fold and step 3's arm has not
   been observed by either state OR the watcher. Synthesize one
   `FilesystemSignal::HeadChanged` and feed it to
   `runtime.handle_signal(...)`. That re-runs `rebuild_repo`
   inside the runtime lock, after which any further change is
   guaranteed to be delivered by notify.
   - Invariant the implementation MUST satisfy: by the time
     `wait_for_work` enters its initial `compute_match`, the
     runtime state is at-or-newer than the watcher's
     subscription point.
5. Build a `wait::WaitArgs` from the CLI args and call
   `wait::wait_for_work(&runtime, args).await`.
6. Print the response. `WaitResponse::Work(payload)` → render
   payload (human or JSON). `WaitResponse::Timeout(_)` →
   `no work; timed out after Ns` to stderr, JSON `{"timed_out":
   true, "no_active_plans": ...}` to stdout under `--json`.
7. Drop watcher (process exit). Notify task dies with the
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
- `src/runtime.rs`: add
  `Runtime::add_repo_with_policy(repo, CachePolicy)` and make
  `add_repo` delegate. This is the only Runtime-facing API
  change required by this plan.
- `src/main.rs`: wire `Command::Wfw(args)` → `cli::wfw::run(args)`.

### Plan-id normalization / inference

- `<plan>` omitted → call `cli::plan_resolve::resolve_plan(state,
  basename, None)` against the post-fold `RepoState`. Returns the
  single active visible plan or a structured ambiguity error
  with candidates. Same UX as `trinity finish` / `trinity purge`.
- `<plan>` contains `/` → treat as canonical; validate via
  `PlanId::parse`.
- `<plan>` is a bare stem (with or without `.md`) → prepend
  `<basename>/` and re-add `.md` if missing. Then validate.

Implementation note: plan resolution runs AFTER step 2's
`add_repo_with_policy`, so it operates on the same fold the
watcher will see. The snapshot is read via
`runtime.snapshot_repo(...)` to avoid threading a second
`RepoState` through.

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

End-to-end tests (gated by `cfg(test)`, calling `cli::wfw::run`
directly — NOT subprocess — so we assert on the typed
`WaitResponse` rather than parsing stdout). The CLI piece under
test is "local notify event wakes wait_for_work into the right
role-specific work." Each test pairs a wake signal with the role
that actually consumes it:

1. **Reviewer wakes on a new reviewable commit.** Init repo,
   commit a plan_intro authored by `master`. Start
   `cli::wfw::run` for `alice`, `role: reviewers`,
   `timeout_secs: 30` in one task. From a second task, commit a
   plan_revision (or impl commit) targeting the plan. Assert
   the call returns `WaitResponse::Work` whose action is
   `WriteFeedback` with `path` pointing at
   `.trinity/feedback/<stem>/<sha>/alice.md`.

2. **Master wakes on a feedback write.** Init repo, commit a
   plan_intro authored by `master`, then commit so a reviewable
   commit exists with a reviewer participant. Start
   `cli::wfw::run` for `master`, `role: master`,
   `timeout_secs: 30`. From a second task, write a feedback
   file under `.trinity/feedback/<stem>/<sha>/alice.md` with
   `REQUEST_CHANGES`. Assert the call returns
   `WaitResponse::Work` whose action is `AddressChanges`.

3. **Timeout.** Same starting state as (1) or (2) but no event
   in the second task. Assert `WaitResponse::Timeout` after
   `timeout_secs`.

4. **Race-window regression.** Before starting the watcher,
   plant a feedback file that creates master work. Run
   `cli::wfw::run` with `role: master`, `timeout_secs: 2`.
   Assert the call returns `WaitResponse::Work` immediately
   (via the post-arming re-fold), not `Timeout`. This is the
   guard for the watcher/fold ordering invariant.

The existing pure `wait_for_work` tests cover the matcher; the
new tests only prove "local notify is wired to wake the right
role."

## Acceptance

- `trinity wfw [plan] --role <r> --author <a>` runs without the
  daemon: no port bound, no daemon child process spawned, no
  `reqwest` call.
- **Reviewer-role** wait wakes within debounce-window latency
  of a new reviewable commit landing under the plan.
- **Master-role** wait wakes within debounce-window latency of
  a feedback file landing under the plan (or a Finalize commit,
  via the `SessionFinished` terminal-state path).
- `[plan]` omission falls through to single-active-plan
  inference; ambiguity returns a structured error matching
  `finish`/`purge` shape.
- `--timeout-secs N` honored.
- `--no-cache` is fully honored: under it the wfw fold uses
  `CachePolicy::Bypass` and the on-disk cache is neither read
  nor written by the run.
- `--json` emits a parseable `WaitResponse` JSON document.
- Race-window regression test passes (see test 4).
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
