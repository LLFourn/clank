# poll-times-are-wall-clock

## Why

After the Mac slept for ~20h, `clank status` reported the GitHub watch
"no completed poll for 20h (expected every 1m)" while its polls were
completing every minute (the watch's poll state files were rewritten
seconds earlier, and the monitor held a live connection to GitHub).

`crates/cli/src/cli/github/progress.rs` `Progress::at` converts a tokio
`Instant` to Unix seconds as `epoch + instant - start`, where `epoch` is
the wall time at the poll loop's start. On macOS tokio's clock (std
`Instant`, `CLOCK_UPTIME_RAW`) does not advance while the machine sleeps,
so every published time (`checked`, `PollError::since`, `retry_at`) lags
wall time by the total sleep since the monitor started. `PollRecord::health`
then compares that lagging `checked` against real `now`, so once the sleep
exceeds `STALE_INTERVALS` intervals the source reads as failing forever,
until the monitor restarts.

The model error: one value serves as both a scheduling instant (correctly
the monotonic clock) and a published timestamp compared with wall time
(must be the wall clock). The conversion exists so paused-clock tests
"see the times their own schedule makes", a test convenience that gave
the published value the wrong meaning in production.

## Build

1. `Progress` stamps published times from the wall clock, through a
   clock seam tests can drive (no `#[cfg(test)]` behaviour divergence in
   the production path); scheduling stays on tokio's `Instant`.
   `retry_at` is the wall time now plus the remaining monotonic delay.
2. Delete `Progress::at`'s epoch-plus-elapsed conversion; nothing else
   converts a monotonic instant to Unix time (swept: it is the only site),
   so a grep gate is not needed, but say so in the test's doc.
3. Tests:
   - a suspend: the monotonic clock and the wall clock diverge by hours
     between two completed polls; the published `checked` is the later
     poll's wall time and `health` is Healthy;
   - a source that really stops completing polls still turns failing after
     `STALE_INTERVALS` intervals of wall time;
   - the existing failure/retry timing tests hold on the new clock.
   Mutation-check by putting the epoch-plus-elapsed `at` back: the suspend
   test must fail.

## Acceptance

- A machine sleep never makes a polling source read as failing.
- Published poll times are wall times; schedule waits are monotonic.
