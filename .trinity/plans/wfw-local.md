# wfw-local

Step 1 of OMEGA: make `trinity wfw` an end-to-end local CLI. No daemon,
no HTTP, no `reqwest`. Fold the repo locally, attach the same
`notify_bridge` watcher the daemon uses, run the repo-scoped
matcher landed by `commit-first-review-model`, print the result,
exit.

## Sequencing

This plan **depends on** `commit-first-review-model.md`. That
precursor changes the model so the reviewable unit is the
commit, `wait_for_work` accepts an optional `plan_id` filter,
and ad hoc commits are first-class. `wfw-local` is the first
consumer of the new repo-scoped matcher and ships only after
the model refactor lands.

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

This plan is one step in OMEGA: prove the model by implementing
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

`[plan]` is an OPTIONAL filter, not a required key. It mirrors
the repo-scoped contract `commit-first-review-model` lands on
the matcher:

- `trinity wfw --role reviewers --author codex` → wait for the
  next reviewable commit codex hasn't reviewed, anywhere in the
  repo, plan or ad hoc.
- `trinity wfw <plan> --role reviewers --author codex` → narrow
  to commits attributed to that plan.
- `trinity wfw --role master --author claude` → wait for the
  next master action across the repo: address-changes on any
  plan, a pending ad hoc review when configured, a stuck
  state — whichever fires first.

When `[plan]` is supplied, it accepts `<basename>/<stem>.md`
canonical form or a bare stem (`wfw-local`, `wfw-local.md`);
bare stems are normalized to `<cwd-repo-basename>/<stem>.md`.

The CLI does NOT fall back to "single active plan inference"
(the `finish`/`purge` style). `wfw` is a review/work driver,
not a plan operation; its natural default is repo scope.

### Runtime behavior

The implementation MUST satisfy three invariants. The numbered
steps below are one valid ordering; reviewers should evaluate
against the invariants, not the exact sequence.

**Invariants**

- **I1 (HEAD-pin per fold)**: every fold resolves the target
  commit SHA once at the start and folds relative to that
  exact SHA for the whole rebuild. A later HEAD-change event
  triggers a new rebuild, but that rebuild also starts by
  pinning the observed SHA. Feedback files are a live overlay
  read at the end of a fold; the git-graph side is stable per
  fold.
- **I2 (watcher precedes wait)**: by the time
  `wait_for_work` enters its initial `compute_match`, the
  runtime state is at-or-newer than the watcher's
  subscription point. Any change between fold-start and
  wait-entry is either already in the state OR queued in the
  notify channel.
- **I3 (cache policy is threaded end-to-end)**: under
  `--no-cache`, neither the initial fold NOR any subsequent
  rebuild (including the post-arm re-fold) reads or writes
  `.trinity/cache/repo-state/`. The flag is honored or it
  isn't shipped.

**One valid sequence**

1. Resolve repo via `cli::resolve_repo`.
2. Build an `Arc<Runtime>`. Call
   `runtime.add_repo_with_policy(repo, policy)` where
   `policy` is `CachePolicy::Bypass` under `--no-cache`,
   else `CachePolicy::Use`. Additive runtime API:
   `add_repo_with_policy(repo, CachePolicy)`. `add_repo`
   becomes a wrapper delegating with `CachePolicy::Use`.
3. **Arm the watcher**: spawn
   `notify_bridge::start(Arc::clone(&runtime), repo.clone()).await?`.
   `Debouncer::watch()` is sync; events from that point
   forward queue into the broadcast channel.
4. **Re-fold under the chosen policy** (satisfies I2 and I3):
   call a new policy-aware refresh helper
   `runtime.refresh_with_policy(repo, policy).await?`.
   - This does NOT go through `handle_signal(HeadChanged)` —
     that path calls `rebuild_repo` (default policy) and
     would silently re-enable cache use under `--no-cache`.
   - Implementation: a small additive method on `Runtime`
     that calls `rebuild_repo_with_policy(repo, policy)` and
     stores the result, mirroring `handle_signal`'s storage
     logic but with explicit policy.
5. Build `wait::WaitArgs` from the CLI args. `[plan]`
   supplied → `Some(plan_id_str)`; absent → `None`
   (repo-scoped per the precursor).
6. Call `wait::wait_for_work(&runtime, args).await`.
7. Print the response. `WaitResponse::Work(payload)` →
   render payload (human or JSON). `WaitResponse::Timeout(_)`
   → `no work; timed out after Ns` to stderr; under
   `--json`, the JSON timeout payload to stdout.
8. Drop watcher (process exit). Notify task dies with the
   `Arc<Runtime>`.

### Human render (default)

```
repo      /Users/llfourn/src/trinity
role      master
plans     trinity/wfw-local.md
action    address_changes (1 review at .trinity/feedback/wfw-local/abc1234/codex.md)
plan      .trinity/plans/wfw-local.md
```

For ad hoc commits the `plans` line is empty / `(none)` and the
feedback path is under `.trinity/feedback/_/<sha>/…`. The wire
type `WorkPayload` after the precursor exposes `plans:
Vec<String>` instead of a singular `plan_id`; the renderer
prints that list.

JSON output is `serde_json::to_string_pretty(&WaitResponse)`.

## Implementation outline

- `src/cli/mod.rs`: add `WfwArgs` struct + `pub mod wfw;`.
- `src/cli/wfw.rs`: the runner. Pure orchestration; pulls
  `Runtime`, `server::wait::wait_for_work`, and
  `server::notify_bridge::start` directly. No new
  abstractions.
- `src/runtime.rs`: add
  - `Runtime::add_repo_with_policy(repo, CachePolicy)`
  - `Runtime::refresh_with_policy(repo, CachePolicy)` (the
    post-arm policy-aware refresh from step 4 / I3).
  - `add_repo` delegates to `add_repo_with_policy` with
    `CachePolicy::Use`.
- `src/main.rs`: wire `Command::Wfw(args)` →
  `cli::wfw::run(args)`.

### Plan-filter parsing (no inference)

- `[plan]` omitted → pass `None` to the matcher. Repo scope.
- `[plan]` contains `/` → treat as canonical; validate via
  `PlanId::parse`.
- `[plan]` is a bare stem (with or without `.md`) → prepend
  `<basename>/` and re-add `.md` if missing; validate via
  `PlanId::parse`.

No `cli::plan_resolve::resolve_plan` lookup. That helper is for
plan operations (`finish`, `purge`); `wfw` is repo-scoped
work-driver.

### Cross-module imports

`server::wait::wait_for_work` and `server::notify_bridge::start`
are already `pub` and don't depend on HTTP. The CLI calls them
directly. No restructure in this plan — moving `server::wait` and
`server::notify_bridge` out of `server/` belongs to the
daemon-removal follow-up.

## Behavioral notes

- **Stale-review dedup**: the daemon tracks
  `Trinity.seen_stale_reviews` (`BTreeSet`) so a master wfw
  match doesn't re-send the same superseded review every poll.
  CLI `wfw` runs in a fresh process every invocation; the set
  is empty at startup. A master who calls `trinity wfw`
  repeatedly will see the same stale review on each call
  until the underlying SHA changes. Documented gap. A
  follow-up can persist the set under
  `.trinity/cache/wfw-state/<author>.bin` if loop ergonomics
  demand it.

- **Opportunistic body cache**: the daemon's
  `Trinity.opportunistic_bodies` deduplicates inline content
  across HTTP polls. CLI has no equivalent problem — one call,
  one response. v1 always inlines content (subject to the 64
  KiB cap). The optional field stays optional on the wire.

- **Timeout floor**: defaults to 1800s. Floors at 1s. No upper
  cap. Matches daemon.

## Test plan

End-to-end tests (gated by `cfg(test)`, calling `cli::wfw::run`
directly — NOT subprocess — so we assert on the typed
`WaitResponse` rather than parsing stdout). The CLI piece under
test is "local notify event wakes wait_for_work into the right
role-specific work." Each test pairs a wake signal with the role
that consumes it:

1. **Reviewer wakes on a new plan-attributed reviewable
   commit.** Init repo, commit a plan_intro authored by
   `master`. Start `cli::wfw::run` for `alice`, `role:
   reviewers`, no `[plan]`, `timeout_secs: 30`. From a second
   task, commit a plan_revision targeting the plan. Assert
   the call returns `WaitResponse::Work` whose action is
   `WriteFeedback` with `path` pointing at
   `.trinity/feedback/<stem>/<sha>/alice.md`.

2. **Reviewer wakes on an ad hoc commit (repo scope).** Init
   repo with a feedback-history author `alice`. Start
   `cli::wfw::run` for `alice`, `role: reviewers`, no
   `[plan]`. From a second task, commit a code change that
   touches no plan files. Assert the call returns
   `WaitResponse::Work` for that commit with feedback path
   under `.trinity/feedback/_/<sha>/alice.md`.

3. **Master wakes on a feedback write.** Init repo, commit a
   plan_intro authored by `master`, then commit so a
   reviewable commit exists with a reviewer participant.
   Start `cli::wfw::run` for `master`, `role: master`. From
   a second task, write a feedback file with
   `REQUEST_CHANGES`. Assert
   `WaitResponse::Work { action: AddressChanges, ... }`.

4. **Plan filter narrows the wait.** Set up TWO active plans
   in one repo. A reviewable commit lands on plan B. Start
   `cli::wfw::run` for `alice`, `role: reviewers`, `[plan]
   = trinity/plan-A.md`, `timeout_secs: 2`. Assert
   `WaitResponse::Timeout` — the commit on plan B does not
   wake a plan-A-filtered wait. Then unset the filter and
   confirm the same setup wakes immediately at repo scope.

5. **Timeout.** Same starting state as (1) but no event in
   the second task. Assert `WaitResponse::Timeout` after
   `timeout_secs`.

6. **Race-window regression.** Before starting the watcher,
   plant a feedback file that creates master work. Run
   `cli::wfw::run` with `role: master`, `timeout_secs: 2`.
   Assert the call returns `WaitResponse::Work` immediately
   via the post-arm re-fold (I2). The test variant under
   `--no-cache` must also pass and must not touch
   `.trinity/cache/repo-state/` (I3 regression guard).

7. **HEAD-pin under flap.** Start `cli::wfw::run`. From a
   second task, flap HEAD (commit, then revert) several
   times before letting it settle on a known SHA. Assert
   the final work payload's `target_sha` matches the
   settled SHA, never a transient.

## Acceptance

- `trinity wfw [plan] --role <r> --author <a>` runs without
  the daemon: no port bound, no daemon child process spawned,
  no `reqwest` call.
- **Reviewer-role** repo-scoped wait wakes within
  debounce-window latency of any new reviewable commit
  (plan-attributed OR ad hoc) the author hasn't reviewed.
- **Reviewer-role** plan-filtered wait ignores commits
  outside the filter.
- **Master-role** wait wakes within debounce-window latency
  of a feedback file landing under any plan or ad hoc
  commit (subject to the precursor's config knobs).
- `--timeout-secs N` honored.
- `--no-cache` is fully honored: under it the wfw fold AND
  any post-arm re-fold use `CachePolicy::Bypass`. The
  on-disk cache is neither read nor written by the run
  (I3 regression test).
- `--json` emits a parseable `WaitResponse` JSON document.
- Race-window regression test passes (I2 guard).
- HEAD-pin invariant holds under flap (I1 guard).
- Existing `trinity serve` daemon still works untouched.

## Out of scope (deferred to follow-up OMEGA plans)

- Removing `trinity serve` + `src/server/*` + `build.rs` +
  frontend bundle + `include_dir` + `axum` / `tower-http` /
  `reqwest` / `rmcp` transport bits. Sequenced after `wfw`
  and the in-process MCP server land.
- Replacing the `trinity mcp` stdio shim with an in-process
  MCP server. Next plan.
- Persisting `seen_stale_reviews` / `opportunistic_bodies`
  across CLI invocations.
- Restructuring `server::wait` / `server::notify_bridge` out
  of `server/` into top-level modules.
