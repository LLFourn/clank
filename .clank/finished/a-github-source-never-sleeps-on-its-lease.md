# a-github-source-never-sleeps-on-its-lease

Invariants: I9.

## Why

`github/supervisor.rs:166` takes the patient `IngestLease::try_acquire` inside a hosted GitHub source's async task. That call sleeps the thread for up to `PATIENCE` (250 ms) while another process holds the lease. In a monitor this is not the serve loop, which is why I9 holds without it. But it parks a runtime worker, and the command source in `wait.rs` already avoids this with `try_acquire_now` plus `when_free`.

## Build

The supervisor uses `try_acquire_now`. When the lease is held, it waits for it to be free asynchronously, the way the command source does (`when_free`). It never sleeps a runtime thread. I9's `Scope` drops its mention of the hosted GitHub task.

## Acceptance

- With the lease held elsewhere, using the test-only `elsewhere` registry, the supervisor's acquisition never sleeps a thread (`lease_waited` is false), and it takes the lease once it is released.
