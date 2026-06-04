# git-io-sync-signatures
# Drop the vestigial `async fn` surface from git_io after the gix migration

## Problem

After `replace-git-io-with-gix` (FINISHED `52a47f1`), every function in `crates/cli/src/git_io.rs` has shape `pub async fn foo(...) -> Result<T, GitIoError>` with a body that does `tokio::task::spawn_blocking(move || { gix work })`. That async-with-spawn_blocking pattern is a leftover from when the implementation was `tokio::process::Command` — async subprocess management was real async work.

With gix as the backend, there is no async work. The signature lies about what the function does. `spawn_blocking` is overhead-on-overhead — pretending to do async, while actually offloading to a thread to *avoid* blocking an async runtime we don't need.

Concretely:
- Every git_io call pays microseconds of spawn_blocking + join overhead.
- Every caller threads `.await` through code that doesn't perform async work.
- The `async fn` signature obscures the truth: this is CPU + local FS work, milliseconds at most.
- New contributors reading `git_io` will assume async-correctness matters, when it doesn't.

This plan removes the async surface.

## Verified before promotion (audit 2026-06-04)

- `pub async fn` count in `git_io.rs`: **12** (every public function).
- `tokio::task::spawn_blocking(` count: **11** (one function — `snapshot` — composes others and inherits async through them, no direct spawn_blocking).
- External call sites of `git_io::*`: **22** across the workspace (verified by `grep -rn "git_io::" crates/ --include="*.rs" | grep -v "/git_io.rs:"`).
- **Concurrent-caller audit**: `grep -rn "join!\|try_join!\|FuturesUnordered" crates/` returns nothing. Zero callers parallelize git_io. Step "audit concurrent use" collapses to "nothing to migrate."
- **Tokio still load-bearing elsewhere**: `crates/cli/src/runtime.rs` uses `tokio::sync::{Mutex, broadcast}` for the in-memory state coordination; `cli/wfw.rs` uses async filesystem watching via `notify`. So `#[tokio::main]` STAYS — this plan does NOT drop the tokio runtime.
- **`tokio::process::Command` from `git_io.rs`**: already gone (deleted during step 5/6/7 of the gix migration along with `run`/`run_ok`/`run_ok_raw`).

## Approach

1. **Flip every `pub async fn` in `git_io.rs` to `pub fn`.** Remove the `tokio::task::spawn_blocking` wrapper, the `move || { ... }` boundary, and the `.await.map_err(|e| GitIoError::Spawn(format!("blocking task join failed: {e}")))?` tail. The bodies become straight-line sync code — same gix calls, same `?`-propagation, no thread hop.

2. **Update every caller** to drop the `.await`. Verified call-site list (22 total) across:
   - `crates/cli/src/preview.rs`
   - `crates/cli/src/rebuild.rs`
   - `crates/cli/src/cli/wfw.rs`
   - `crates/cli/src/cli/status.rs`
   - `crates/cli/src/cli/open.rs`
   - `crates/cli/src/cli/rewrite.rs` (uses `block_in_place` already)
   - `crates/cli/src/cli/purge.rs`
   - `crates/cli/src/cli/finish.rs`
   - Test code with `#[tokio::test]` calling git_io directly

   The change is mechanical. Compiler tells you where. After the flip, the compiler errors are the worklist.

3. **Tokio stays.** `runtime.rs` + `wfw.rs` need it (verified above). `#[tokio::main]` is untouched. Test helpers using `#[tokio::test]` that call git_io become candidates for switching to plain `#[test]` — but that's not required by this plan and can be a follow-up to keep the diff focused.

4. **Drop the `Spawn` variant of `GitIoError`?** With no `spawn_blocking`, the `GitIoError::Spawn("blocking task join failed: ...")` paths are dead. Whether to delete the variant or leave it as a vestigial fallback is a call this plan makes during implementation:
   - **Delete** if no other path constructs `Spawn`. Tightens the error model.
   - **Keep** if any other code (or future test) might construct it. Costs nothing.

   Audit before flipping: `grep -n "GitIoError::Spawn" crates/cli/src/`. If only the deleted spawn_blocking sites used it, delete the variant. The plan's implementation log records the choice.

## Out of scope

- Removing tokio from the workspace entirely (still needed for runtime.rs + wfw.rs).
- Replacing the existing `async fn` surface in other modules (`agent_store`, `wfw`, `runtime`, etc.). Those have legitimate async work.
- Repository handle caching across calls (separate optimization; the per-call `gix::open` cost is real but orthogonal to async surface).
- Switching test helpers from `#[tokio::test]` to `#[test]` (optional follow-up).

## Acceptance

- Every function in `git_io.rs` has signature `pub fn ... -> Result<T, GitIoError>` (no `async`).
- No `tokio::task::spawn_blocking` calls in `git_io.rs`.
- All callers updated; `cargo build --workspace` clean.
- `cargo test --workspace` passes — no test or runtime path relied on parallelism via `git_io::*` futures (already verified by the audit).
- Net LoC: `git_io.rs` shrinks (one spawn_blocking shell per function = ~6 lines each × 11 = ~60 lines saved).

## Tests

Existing tests are the spec. They flip from `.await`-on-git_io to direct calls; no new test cases needed for the signature change itself.

Tests that use `#[tokio::test]` SOLELY to call git_io (i.e. don't await anything else) can switch to `#[test]` opportunistically, but the plan doesn't require it.

## Related history

- `replace-git-io-with-gix` (FINISHED `52a47f1`): backend swap that made this plan possible. The async signature was preserved during that migration to keep call-site changes out of scope. This plan picks up the cleanup.
