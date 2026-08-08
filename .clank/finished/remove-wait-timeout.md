# remove-wait-timeout

Remove `clank wait --timeout`, the per-agent `wait_timeout` config, and
everything that exists only to serve them.

## Why

The flag promises "Maximum wait" — a hard bound the process never
enforced: the deadline is armed only after unbounded setup (startup
fold + watcher attach), and the dropped
wait-deadline-honoured-under-slow-refold plan measured a 2s timeout
returning at 9.5s through the shipped control flow. Making the bound
honest needs a cancellable or safely abandonable fold/attach boundary;
scheduling is not cancellation, and that engineering is
disproportionate for a flag whose only consumer is our own stop hook.

The wait's real contract is "park until work arrives", and removing
the flag collapses three behaviours (explicit `wait_timeout` /
default / `0` = indefinite) into one.

The one honest bound an indefinite wait still needs is death-with-
owner. `kill_on_drop` is NOT it: it fires only when the `Child`
handle drops during orderly cancellation — if the hook runner
forcibly kills the stop-hook process, destructors never run and an
indefinite wait is orphaned for good (the runner's ceiling kills the
hook, not the child). So removal must be paired with the ownership
design below (codex on 045089c).

## Owner-death reaping: an inherited sentinel, not a sampled pid

Sampling `getppid()` cannot deliver the guarantee (codex on da631a8),
and both failures are real:

- **Startup race.** If the owner dies between spawn and the child's
  first instruction, the child records the REAPER as its parent and
  then waits forever. Unfixable from inside the child — by the time
  it runs, its original parent is already unknowable.
- **Blind during blocking setup.** A heartbeat check cannot fire
  while the main thread is inside `notify::watch`, which this plan
  records at 7.5-11.3s under libtest. Exactly the window where an
  owner is most likely to be killed.

Both dissolve if ownership is an INHERITED HANDLE rather than a
number to poll:

The spawner passes the wait child a pipe as its **stdin** and holds
the write end. `clank wait --die-with-owner` watches that fd on a
DEDICATED thread blocked in `read(2)`; EOF means every copy of the
write end is closed, which the kernel guarantees on orderly exit AND
on SIGKILL alike. Then:

- No startup race — if the owner is already dead, EOF is pending
  before the child reads, so it exits on its first look.
- No blind window — the watcher thread is independent of whatever
  the main thread is blocked in.
- No PID reuse hazard, and no polling latency: EOF is an event.

**Opt-in, and deliberately so.** Reaping happens only under the
flag. An interactive `clank wait` gets no ownership semantics — and
must not, since `clank wait < /dev/null` would otherwise see instant
EOF and exit immediately. This answers the objection to imposing
die-with-parent on every wait: only a spawner that opts in gets it,
and today that is our stop hook.

`kill_on_drop` stays as the orderly-cancellation fast path.

**Implementation consequence, easy to miss:** the hook currently
spawns the wait with `.stdin(Stdio::null())` and `cmd.output()`.
`output()` closes the child's stdin immediately, which under this
design reads as instant owner death. The hook must switch to
`spawn()`, `take()` the `ChildStdin`, hold it alive for the whole
wait, and collect stdout via `wait_with_output()`.

## Scope — delete, don't deprecate

- CLI: `WaitArgs.timeout`, `WaitTimeout` and the exit-2 contract.
  `clank wait` parks until a wake, a real error, or owner death
  (the sentinel above).
  The duration PARSER survives: `parse_timeout` currently also backs
  `parse_duration_str`, which github_events.rs uses for
  `poll_interval` — rename/relocate it (and keep its unit tests),
  do not delete it with the flag. Its user-facing STRINGS must stop
  naming the removed flag: wait.rs:1654 "invalid --timeout `{raw}`",
  wait.rs:1663 "invalid --timeout unit", and the doc at wait.rs:1636.
  After this plan its only consumer is github `poll_interval`, so a
  bad interval must not error with the name of a flag that no longer
  exists.
- Stop hook: `compute_wait_outcome` stops passing `--timeout`; delete
  `codex_wait_timeout`, the exit-2 → `SilentReason::WaitTimeout`
  mapping, and the variant if orphaned. The in-hook poll then parks
  until a wake, the hook-runner ceiling, or owner death — say so in
  the module doc.
- Config: drop `wait_timeout` from `crates/core/src/agent_config.rs`
  (serde's default ignores the key in existing config.json files, so
  no migration), from `clank auto` (set, show, the `AutoArgs` flag),
  the fork carbon-copy, the agent_store preserve-on-flip handling, the
  status_tui note, stale mentions in agent.rs comments, and
  README.md:123.
- Tests: delete every test whose subject is the flag or the config
  (stop-hook timeout mapping and expiry tests, auto serialization,
  fork carry, agent_store preserve round-trip, the field in doctor /
  fork fixtures). Tests that used a short `--timeout` AS the assertion
  mechanism ("stays parked to timeout", observer "parks until
  timeout") must assert parked-ness without the flag: sample that the
  wait is still alive after a bounded window, then abort it (the
  window is constrained — see Acceptance).
  The sentinel is tested IN-PROCESS (this repo bans spawning the
  clank binary in tests): drive the watcher with a real pipe and
  assert it signals on EOF, covering BOTH orderings — write end
  already closed before the watcher starts (the startup race), and
  closed while it is parked. Assert the hook's spawn wiring by
  construction (stdin piped, the flag present, the handle held past
  the wait) rather than by launching processes, and pin that WITHOUT
  the flag an EOF on stdin is ignored.

## Carried measurements (from the dropped plan, 2026-08-07)

- `notify`'s `watcher.watch(.clank, Recursive)` costs 7.5–11.3s inside
  the cargo test binary vs ~2ms from a standalone crate (same notify
  version, machine, path; mechanism inside libtest unidentified).
  Four concurrent attaches → 17–34s.
- Consequence: wait integration tests that attach watchers fail under
  parallel libtest runs at any commit, independent of this plan.
  Reworked tests that still attach watchers must serialise attaches
  (a file-local mutex) and/or calibrate outer deadlines to a
  once-per-binary attach measurement — the dropped plan's
  wait_config_reload.rs / wait_for_observer.rs patch did both and can
  be reconstructed from this note, but do not reintroduce it verbatim
  where a test no longer parks to a timeout.

## Acceptance

- `clank wait --help` shows no `--timeout`;
  `rg 'wait_timeout|WaitTimeout|\-\-timeout' crates/ README.md` is
  empty except this plan's own text — the narrower
  `wait_timeout|WaitTimeout` pattern cannot see the stale `--timeout`
  strings in the surviving parser, so it would pass with them left
  in. The parser's tests and its github `poll_interval` callers are
  green under whatever name/location it keeps.
- The reworked parked-assertion tests pass under a normal parallel
  `cargo test`, several consecutive runs. Their "still parked"
  window must EXCEED this environment's watcher-attach cost (the
  same once-per-binary measurement the note above requires) or
  assert parking by a positive signal: sampling aliveness at 2s
  while attach takes 7.5-11.3s observes a wait that has not begun
  to park, and would pass even for a wait wedged in setup.
- Codex stop hook: an idle in-hook poll no longer self-expires (no
  exit 2); a wake still delivers items; the sentinel signals on EOF
  in both orderings above, and the hook holds the write end for the
  whole wait (a regression here kills every wait instantly, so it
  needs its own assertion).
