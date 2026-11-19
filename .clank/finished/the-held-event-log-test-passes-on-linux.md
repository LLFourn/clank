# the-held-event-log-test-passes-on-linux
# The held-event-log test passes on Linux

## Problem

The v0.3.0 release PR (#38) fails Linux CI in exactly one test. It failed the
same way on both runs, and the test passes on macOS:

```
cli::monitor::custody::turn_tests::a_held_event_log_stalls_no_serve_and_the_retry_presents_it
panicked at crates/cli/src/cli/monitor/turn_tests.rs:3215:5:
claude's serve is left to the retry
  left: [true]
 right: [true, false]
```

`first` comes from `served_retried_then_retired`, which lists the
`unavailable` flag of every client still held after `serve_reports`. On Linux,
the agent that is owed nothing (the second client) has been answered with a
terminal verdict and dropped from `clients`. On macOS it is still held,
presumably parked with a non-terminal verdict. So the two platforms decide that
agent's verdict differently, or the client leaves for some other reason. Until
the cause is known, either the test or production could be wrong.

## Change

- Reproduce on Linux. The Lima VM `repro-a` (aarch64, vz) is available, and CI
  runs Ubuntu 24.04. Find what makes the second client's verdict terminal there
  and parked on macOS. Name the platform difference, for example socket
  behaviour, the filesystem, timing or the environment.
- If production behaves differently across platforms, and the test expectation
  is the right one, fix production. If the test depends on something that isn't
  part of its contract, fix the test so it asserts the contract (claude is left
  for the retry; the agent owed nothing is answered), whether or not that agent
  is still held.
- Assert the mechanism, not an ordering that happens to hold on macOS.

## Acceptance

- The test passes on Linux, run in the VM or a Linux container, and on macOS.
- The commit message names the cause.
- Run `cargo test -p clank --lib monitor` on macOS, and the same module on
  Linux.
