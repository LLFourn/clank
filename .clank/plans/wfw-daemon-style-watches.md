# wfw-daemon-style-watches

## Summary

Two changes to `clank wfw`'s wake strategy, both driven by the
Codex sandbox empirically breaking native notify on the gitdir:

1. **Coarse watches for `.clank/`.** One recursive watch on
   `<repo>/.clank` replaces the three separate watches on
   `.clank/{plans,feedback,finished}`. This part of the change
   is unambiguously a win — fewer roots, simpler code, more
   wakeups.
2. **Polling fallback for git metadata, gated by env / flag.**
   `clank wfw` accepts a new `--poll` flag. When set, wfw
   skips the gitdir watch entirely and uses a short periodic
   refold to pick up commits. The default is `false` (native
   watches), but `CODEX_SANDBOX=seatbelt` flips the default to
   `true`. The env check happens at CLI argument processing,
   not deep in the code — `WfwArgs::resolve_poll()` returns
   the effective bool and `WatchContext` / the watch loop take
   it as a plain parameter.

The empirical story behind the env flag:

- Outside the Codex sandbox: native gitdir watches work fine.
  25+ trials of both shell repros pass with ~230 ms latency.
- Inside the Codex sandbox: native gitdir watches DO NOT work,
  even after broadening to recursive + dropping the
  `EventKind` filter. Codex confirmed two consecutive
  revisions where the in-sandbox repros still timed out at
  ~1.3 s. The "no polling" hard direction the earlier draft
  of this plan claimed turns out to be empirically wrong for
  the sandbox: the sandbox does not deliver gitdir events to
  notify at all, in any kind, at any depth.
- `.clank/` directory events DO still reach notify inside the
  sandbox. Feedback writes and finished snapshots still wake
  wfw on the native path. Only the gitdir is broken.

So the architecture is: native watches where they work,
polling where they don't, both selectable by an explicit flag
with a sensible env-driven default. The plan's earlier
"no polling" rule is dropped — the sandbox empirically
contradicts it.

## Hard Direction

- **`.clank/` is always a native recursive watch.** Both modes
  rely on the native watch to catch feedback / plan-file /
  finished-snapshot writes inside `.clank/`. This works in
  both environments. Replaces the three previous separate
  watches.
- **Two modes for git changes.** Native (default outside the
  sandbox) and polling (default inside).
  - **Native mode.** Watch the resolved worktree gitdir
    recursively. Any write under the gitdir wakes wfw; refold
    figures out what changed. Cheap and immediate where it
    works.
  - **Polling mode.** Don't watch the gitdir at all. Use a
    bounded periodic refold (default 500 ms) as the
    invalidation tick for git state. The fold cache makes
    each tick cheap on warm state; the worst-case latency is
    one tick.
- **Env / flag selects the mode at CLI argument processing.**
  - `--poll` / `--no-poll` is an explicit CLI flag, default
    derived from environment.
  - `CODEX_SANDBOX=seatbelt` flips the unset default to
    `true`. Any other value (or unset) defaults to `false`.
  - The check lives in CLI argument resolution, not in
    `WatchContext`, not in the loop. The watch layer takes a
    plain `poll: bool`.
- **Refold is the only invalidation.** Every wake — native
  event, periodic tick, finalize-race heartbeat — refolds.
  We don't try to classify events.
- **Linked worktrees still work in native mode.** The
  per-worktree gitdir (`<main>/.git/worktrees/<name>/`) gets
  recursive watching like the main case. In polling mode the
  worktree question doesn't matter — we just refold.

## Watch Shape

Native mode (`--no-poll`, the default outside Codex):

```text
<repo>/.clank                            recursive
<resolved worktree gitdir>               recursive
```

Polling mode (`--poll`, the default under
`CODEX_SANDBOX=seatbelt`):

```text
<repo>/.clank                            recursive
# (no gitdir watch — periodic refold instead)
```

Periodic refold interval in polling mode: 500 ms. The
finalize-race heartbeat from `wfw-finish-notification` (1.5 s)
stays in both modes; in polling mode it's redundant but
harmless.

## Why .clank/ stays native in both modes

`<repo>/.clank` recursive catches every write Clank itself
makes or sees: plan files, feedback, finished snapshots, and
any subdirectory created later. Replaces three separate
watches with one — the directory layout is no longer
load-bearing on the watcher.

Empirically the Codex sandbox DOES deliver events for
`.clank/` writes. So we leave that root native in both modes;
polling mode's periodic refold is only for git changes, not
for `.clank/`.

## Implementation

### CLI argument resolution

`crates/cli/src/cli/mod.rs::WfwArgs` gains a tri-state `poll`
field via clap. Three calls a user can make:

```text
clank wfw --poll       # force polling on
clank wfw --no-poll    # force polling off
clank wfw              # env-driven default
```

`WfwArgs::resolve_poll(env: &dyn Environment) -> bool` returns
the effective value. With clap's `Option<bool>` + `--no-<x>`
support, the args struct just holds `poll: Option<bool>`; the
helper consults `CODEX_SANDBOX` only when `poll.is_none()`.

```rust
impl WfwArgs {
    pub fn resolve_poll(&self) -> bool {
        if let Some(b) = self.poll { return b; }
        std::env::var("CODEX_SANDBOX").as_deref() == Ok("seatbelt")
    }
}
```

The env read is bound here, not anywhere downstream. Tests
that exercise the resolve_poll logic can call it directly
after setting / unsetting the env var.

### WatchContext + watch loop

`crates/cli/src/cli/wfw.rs::WatchContext` takes the resolved
bool. Two cases:

- `poll == false` (native mode): existing `clank_root +
  git_dir` recursive watch. Heartbeat stays at 1.5 s.
- `poll == true` (polling mode): only the `clank_root`
  recursive watch is attached. The watch loop's
  `recv_timeout` interval shortens to 500 ms (a "poll tick")
  instead of 1.5 s. Each tick refolds and checks for work.
  Native `.clank/` events still wake the loop early when
  they fire, debounce as today, refold.

The existing finalize-race heartbeat stays as-is. In polling
mode the 500 ms tick IS the heartbeat at higher frequency;
the 1.5 s value just becomes ineffective because the tick
fires sooner.

### Linked worktrees

Native mode preserves the existing behavior — the resolved
worktree gitdir gets recursive watch. Polling mode doesn't
care which worktree we're in; the refold reads HEAD via
`git rev-parse` against the cwd-repo as today.

Drop:

- The per-file `HEAD` / `logs/HEAD` watches.
- The separate `refs` recursive watch on the common gitdir.
- The `packed-refs` watch.
- The three `.clank/{plans,feedback,finished}` watches.
- The `git_common_dir` resolution — if a future need for
  shared-ref wakes appears, add a broad watch root for the
  common directory, not individual files.

Keep:

- `WatchContext::resolve` for the worktree gitdir path (drop
  the `git_common_dir` field).
- The `try_watch` helper (now with only two callers).
- The 1.5 s heartbeat in the `recv_timeout` loop.

## Tests

### Rust integration tests (`crates/cli/tests/wfw_integration.rs`)

The existing 12 tests all exercise the watcher and must
continue to pass under the new shape:

- Reviewer wake on new reviewable commit
- Reviewer wake on code-only commit (the bare git-ref signal)
- Reviewer wake inside a linked worktree
- Master wake on REQUEST_CHANGES
- Reviewer finish wake (human + JSON)
- `--plan` filter finish wake
- Mixed work + finished on one wake
- `--plan` against already-finished
- Plan-only approval → Implement
- Code-only approval → Finalize
- Early-snapshot + late-commit finalize race
- Master/code-only wake routing

`wfw_reviewer_wakes_on_code_only_commit` is one part of the
native-mode proof. `wfw_reviewer_wakes_on_commit_with_index_already_staged`
isolates the commit-boundary write from any preceding
`git add` noise: stage BEFORE parking wfw, then run only
`git commit -m`. Both tests must pass under both modes.

New required test:
`wfw_polling_mode_wakes_on_commit_via_periodic_refold`.

1. Set `CODEX_SANDBOX=seatbelt` in the child's environment
   (or pass `--poll` explicitly) so wfw runs in polling mode.
2. Stage the code change BEFORE parking wfw.
3. Start `clank wfw` and wait for the watcher to attach
   (1.5 s — same as native).
4. Run `git commit -m '[foo] code work'`.
5. Assert wfw exits 0 with a `Reviewer` item within ~2 s.
   The poll tick is 500 ms, so the expected latency is one
   tick (~500 ms) plus refold; the bound is generous.
6. Assert `wfw_test_no_gitdir_watch` (a test-only assertion
   hook) — or simpler, verify by inspection during code
   review — that polling-mode WatchContext doesn't register
   a gitdir watch.

`WfwArgs::resolve_poll` gets its own unit tests:

- `--poll` explicit → true regardless of env.
- `--no-poll` explicit → false regardless of env.
- No flag + `CODEX_SANDBOX=seatbelt` → true.
- No flag + `CODEX_SANDBOX=` (empty) → false.
- No flag + `CODEX_SANDBOX` unset → false.
- No flag + `CODEX_SANDBOX=other-value` → false.

### Script-level Codex-sandbox repro

`/private/tmp/clank-watch-repro/repro-git-commit-timeout.sh`
stays as one canonical out-of-tree repro. The behavior under
Codex's sandbox only appears under that tool sandbox; the Rust
suite covers the deterministic invariants and the shell repros
are the empirical safety nets.

Add a second shell repro that mirrors the new Rust isolation
test (stage-then-park-then-commit-only) so we can verify the
commit-boundary wake under the sandbox too. Both repros need
to pass after the switch.

Acceptance for the repros:

- Outside the sandbox: both pass (this must not regress).
- Inside the sandbox: both pass consistently with sub-second
  latency. No heartbeat fallbacks on the hot path.

## Acceptance

- `clank wfw` accepts `--poll` / `--no-poll`. With neither,
  the default is `false` UNLESS `CODEX_SANDBOX=seatbelt` is
  set, in which case the default is `true`.
- The env check happens ONLY in `WfwArgs::resolve_poll` (or
  equivalent). `grep -rn 'CODEX_SANDBOX' crates/cli/src` shows
  exactly one hit, in the CLI arg layer.
- In native mode, `clank wfw` watches `<repo>/.clank` recursive
  AND the resolved worktree gitdir recursive. Two
  `watcher.watch(...)` calls.
- In polling mode, `clank wfw` watches `<repo>/.clank` recursive
  only. One `watcher.watch(...)` call. The watch loop's
  `recv_timeout` interval is 500 ms instead of 1.5 s.
- No watch is configured anywhere on individual files (HEAD,
  logs/HEAD, packed-refs, etc.) in either mode.
- All existing wfw integration tests pass under native mode
  (the default for the test harness on developer machines).
- The new
  `wfw_reviewer_wakes_on_commit_with_index_already_staged`
  test passes under native mode.
- The new
  `wfw_polling_mode_wakes_on_commit_via_periodic_refold` test
  passes under polling mode (with `--poll` explicit so it's
  deterministic regardless of `CODEX_SANDBOX`).
- `WfwArgs::resolve_poll` unit tests cover all six flag/env
  combinations from the Tests section.
- Both shell repros pass inside the Codex sandbox after
  rebuilding. The Codex environment naturally has
  `CODEX_SANDBOX=seatbelt` set, so the default flips to
  polling and the gitdir watch is skipped.
- Both shell repros continue to pass outside the Codex
  sandbox under native mode (this must not regress).
- `cargo test --workspace` green; `cargo fmt --check` clean.

## Out of Scope

- **Removing the heartbeat.** The 1.5 s finalize-race
  heartbeat from `wfw-finish-notification` stays untouched.
- **Cross-worktree ref pushes in native mode.** A separate
  process updating shared refs without touching the current
  worktree's gitdir is not in scope. The heartbeat / polling
  catches it as a slow fallback if relevant.
- **Switching to a different watcher backend.** `notify`'s
  `RecommendedWatcher` stays.
- **Detecting other sandboxes.** Only `CODEX_SANDBOX=seatbelt`
  flips the default. Other sandbox environments (Docker,
  Podman, gVisor, etc.) need explicit `--poll` or their own
  env-driven heuristic — added in a follow-on plan if needed.
- **Tuning the poll interval.** 500 ms is the chosen value;
  knob added later if real workloads need it.

## Open Questions

None.
