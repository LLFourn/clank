# the-suite-runs-in-thirty-seconds

## Why

`cargo test -p clank` took ~615s of test time across 22 targets, with
three binaries accounting for 84% of it:

    184.0s  wait_event_sources
    174.6s  wait_config_reload
    158.1s  wait_for_observer
     50.2s  lib
     38.6s  stop_hook_peek_no_hooks

A ten-minute suite is not a slow suite, it is an ABANDONED one:
iterate against `--lib`, run the full suite once at the end, and
discover an integration contract broken several commits ago. That is
exactly what happened while writing `a-wait-cannot-outlive-its-own-role`
— a deliberate contract in `wait_config_reload` was contradicted for a
whole implementation cycle because the feedback loop was ten minutes.

**Target: the whole suite under 30 seconds.** Anything that genuinely
cannot meet it is feature-gated so the default run stays fast and the
slow cases stay available.

## The diagnosis

Not the refold cadence, which was the first guess. **`notify`'s
FSEvents backend costs ~10s per `.watch()` call on macOS**, measured:

    FSEvents  tmpdir :  14.30 s        Poll  tmpdir :  73.6 µs
    FSEvents  in-repo:  10.02 s        Poll  in-repo: 214.6 µs

    git_state_dir 1.35ms · recommended_watcher 45.8µs
    watch(.clank) 14.74s   <- all of it   · drop 424µs

This was never only a test problem. Native mode makes two `.watch()`
calls, so **every `clank wait` spent ~20s not yet watching anything**,
in every repo, for every agent.

A second defect hid behind it: `next_beat`'s `recv_timeout` was
SYNCHRONOUS inside an async task, parking a runtime worker where
`abort()` cannot reach it and a multi-thread runtime's drop waits for
it. Invisible while attach was slow enough to dominate; with a fast
attach it hung `wait_for_observer` in teardown, after its assertions
had passed.

## What was implemented

- **`PollWatcher`** in place of the platform backend, 500ms interval —
  matching the shortest refold cadence a caller already uses, so the
  watcher is never the slower of the two signals.
- **One async `Beat` bridge**, shared by the work and observer loops:
  a single reader thread does the blocking recv, every await is
  cancel-safe. This also deleted the work loop's inline duplicate.
- **The allowlist in the REGISTERED ROOTS**, not only in
  `is_core_wake`. A native backend filters in the kernel so a
  recursive root is free; a POLLER restats every descendant first. So
  `.clank` is non-recursive (covering `config.json`) plus each
  allowlist dir — never `cache/`, `html/`, `zellij/`, `drafts/`, or
  the nested repos under `worktrees/`. The gitdir is non-recursive
  plus `refs/`, since `objects/` is its bulk and carries no gate
  signal.
- **Linked-worktree coverage**: `HEAD` is per-worktree but the branch
  ref a commit MOVES, and `packed-refs`, live in the shared common
  dir. Both dirs are now registered and both are accepted by
  `is_core_wake`. This gap predated the narrowing.

Root selection is a pure `watch_roots()` so the SELECTION is testable
apart from the registering — the linked-worktree bug lived there, and
a filter-only test passed straight through it.

## Measured

Per-target, each verified in isolation more than once:

    wait_config_reload   174.6s -> 3.8s
    wait_event_sources   184.0s -> 2.6s
    wait_for_observer    141.8s -> 10.3s
    stop_hook_peek        38.6s -> 0.1s

Steady-state watcher cost after narrowing:

    entries restatted per interval   6030 -> 1832
    CPU                              1.7% of a core

## The production trade

`RepoStateWatcher` is the production watcher, so this changes what
every `clank wait` and `clank status` does:

- **Removed**: the ~10-20s attach delay. A production bug in its own
  right, and the original find.
- **Added**: polling CPU where there was none — 1.7% of a core per
  watcher, times concurrent agents.
- **Changed**: detection characteristics. A poll interval bounds how
  quickly a gate change is noticed where a native backend delivered
  events as they happened. A change in KIND, not degree, and the part
  least covered by evidence here.

Production refold cadence (1500ms/500ms) is untouched.

## Why the suite total is not verified here

Every suite-level measurement taken on this machine was contaminated.
Tracking each binary's wall time through a run caught the actual
consumers:

    82.46s  libbincode
    77.64s  frostsnap_widgets     <- a different repo entirely

Other agents build and test frostsnap, frostsnap-ci and secp256kfun
here continuously, and their `rustc` processes saturated the machine
throughout. That accounts for `stash_integration` at 21s against 2.0s
alone, for runs where one arbitrary target "never reported" (a timeout
landing while the machine was busy), and for totals ranging from 39.1s
to past 400s.

**No suite-total figure from this work should be quoted.** Isolation
measurements stand; the total needs an idle machine or CI.

## Remaining work

1. **Verify the under-30s target** on an idle machine or in CI. This
   is the deliverable and it is NOT yet demonstrated.
2. **Delete tests that cannot fail.** A targeted scan of the 1333 test
   functions for the shapes below found NO deletions. The candidates
   it surfaced are all the keep case: `!contains("confirm:")` and
   `!contains("purpose")` in `render.rs` are absence assertions over
   strings the renderer PRODUCES, so re-adding either turns them red,
   and `the_task_list_no_longer_decides_anything` /
   `an_idle_poll_no_longer_self_expires` both break if the behaviour
   they pin returns.

   Stated with its limit: a grep over 1333 tests is not a full audit,
   and none is claimed. The durable check is the reviewed test-list
   diff below, applied whenever tests are removed — not a one-time
   sweep.

3. **Feature-gate whatever genuinely resists.** Nothing does, once
   attach is fast: no target now needs seconds for reasons intrinsic
   to it. No gate is introduced, because a gate with nothing to hide
   is just a second way to run the suite.

### Deleting tests that do not test anything

The criterion is whether a reachable change to the code UNDER TEST
could make the assertion red. Applied to the unit being exercised, not
the system around it: a pure builder inspected directly can be broken
by editing the builder, whatever the runtime downstream would do.

Delete: absence assertions over something the code under test cannot
produce (a field the type no longer has, a flag clap parses for you);
assertions loosened until they cannot fail; exact duplicates; tests
pinning a removed feature's semantics.

Do NOT delete slow tests that still verify something — those get
GATED. The distinction is not "is it slow" but "could it break".

An absence assertion over a value the code PRODUCES is reachable and
stays. `the_in_hook_wait_argv_carries_the_ownership_flag` asserts the
argv omits `--role` and `--timeout`; those look dead but the test
inspects the builder's output before clap runs, so re-adding either
flag turns it red. They were deleted here and restored on review.

Every deletion is named with its reason, and the test-list diff is
REVIEWED: a name that disappears is either gated or deleted-with-a-
reason. A sanctioned delete is exactly when a silent one blends in.

## Out of scope

- Test parallelism or a different runner. The dominant cost was a
  blocking attach, not scheduling.
- Production refold cadence.

## Corrections made along the way (history, not instruction)

Six causal claims were asserted here before one survived. In order:
a pre-existing refold race "unmasked" by the speedup; a leaked pipe
holding stdout; detection latency; watcher drop latency; cold builds;
and finally machine contention, which is the one with direct evidence.
Each of the first five named a cause from whichever component had just
been edited. The refold "race" in particular was a load-induced flake:
neither change alone fails, and the combination passes 3/3.

The lesson, recorded because it was expensive: measure the whole
machine before attributing anything to your own diff.
