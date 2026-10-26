# the-tui-shows-the-monitor
# The TUI shows whether the repo's monitor is running

## Problem

In ~/src/clankstack the master stayed asleep with a promote owed. Its monitor, pid 11481, had exited days earlier, but its runtime descriptor was still there. `clank status` printed `monitor: proto 18, build unknown (legacy)`. That reads as a running monitor, because `monitor::build_line` (crates/cli/src/cli/monitor.rs:5960) never asks whether the published pid is alive, and an `Other` descriptor carries no pid at all. The status TUI shows nothing about the monitor. The user had a TUI open on that repo and could not tell that nothing was there to deliver its wake.

## Change

There is one answer to "is this repo's monitor running", computed in `monitor.rs`, with three cases:
- **Running:** the published pid is alive. Where an observation record exists, its recorded birth must still hold for that pid, so a reused pid doesn't count.
- **Gone:** a descriptor exists but its process is dead.
- **None:** nothing is published.

`Other` descriptors are parsed for their pid where the body allows it, so a legacy monitor gets a liveness answer too. The result has a fourth case: **Unknown**, for a published descriptor whose pid is missing or unreadable. It is never None (nothing published) or Running. An `Untrusted` namespace stays outside the answer, exactly as `published` treats it today.

This reports whether the process exists. It does not claim that a live monitor is responsive.

`clank status`'s `monitor:` line uses it:
- `pid N, …` only while the monitor is running;
- `gone (pid N exited)` for a dead one;
- `none` when nothing is published, instead of omitting the line.

The status TUI gets a `monitor` row beside `dirty` and `remote`. It shows the monitor's pid and its build, the short id from the descriptor's `build.running` and whether it is current or replaced by the installed binary (e.g. `pid 7705 · 71e4c5c1 current`), dim when running, and `none`, `gone` or `unknown` in the warning colour otherwise.

**Keeping the row current.** Reading the observation during a snapshot rebuild is not enough. Today a quiet TUI only repaints its cache on the idle timeout. `close_turn` probes `attendance_fingerprint`, and a rebuild is kept only if `status::InputSignature` changed. Neither covers the monitor, so a monitor dying without touching its descriptor would leave a green row forever. The observation therefore travels through both halves:
- **Scheduling:** the existing rate-limited process-health probe on the idle/close-turn path also takes the monitor observation. When that differs from the one in the current snapshot, a rebuild is requested.
- **Reuse:** the monitor observation is part of `InputSignature`, so a rebuild whose only change is the monitor is kept, not discarded by the unchanged-signature gate.

So a quiet open TUI changes, within the probe's existing rate limit, after a monitor dies without rewriting its descriptor. A descriptor appearing or being replaced is observed the same way, with no repo edit. There is no new timer thread, no socket attach and no repeated full refold, and liveness is never read on an animation paint.

## Acceptance

- Unit tests on the liveness function: a live pid gives Running, a dead pid's descriptor gives Gone, no descriptor gives None, and a reused pid with a mismatched birth is not Running.
- A status render test shows `gone` for a descriptor whose pid is dead.
- A TUI render test shows the `monitor` row in each state.
- A deterministic transition test at the refresh/reuse seam:
  - it starts with Running, then changes only the supplied process observation to Gone, leaving the descriptor bytes, repo inputs, attendance and source account unchanged or absent;
  - the loop requests the rebuild, keeps it, and renders `gone`;
  - the same seam covers None to Running.
