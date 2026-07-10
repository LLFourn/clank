# tui-reconcile-off-loop
# move zellij pane reconciliation off the TUI loop

`PaneReconciler::observe` runs synchronously inside the TUI event loop
after every snapshot build. A reconcile pass is a chain of zellij
subprocesses — list-panes, new-pane/close-pane/override-layout, the
verifying re-list, focus captures and restores — so the TUI freezes
(no render, no input) for the whole chain, and the user reads that as
"promote is slow". Move the reconciliation to a dedicated worker so
the loop never blocks on zellij.

## Shape

- One reconciler WORKER (a plain `std::thread` — the work is
  subprocess-bound, not async) owning the `PaneReconciler` state and
  all zellij I/O. The TUI loop's only job after a snapshot build is a
  non-blocking send of the fresh roster view over an `mpsc` channel —
  microseconds, no subprocesses.
- **Coalescing, scoped precisely** (codex 8c467f4): views QUEUED
  before a pass starts collapse — the worker drains the channel to the
  newest view at pass start. Views arriving DURING an in-flight pass
  cannot cancel its side effects; they collapse into exactly ONE
  follow-up pass against the newest view. So a burst of N edits costs
  at most: the pass already running, plus one. Pinned by a test that
  blocks a pass with injected I/O, sends several newer views, and
  asserts the exact number and order of reconciliations.
- **Serialization for free**: one worker means passes can't overlap,
  preserving the current single-flight behavior without locks.
- Convergence semantics unchanged and stay in the worker: verified
  convergence, retry-on-failure, startup pass on the first view,
  focus preservation, best-effort everything.
- **Shutdown, deterministic** (codex 8c467f4): the TUI owns a worker
  HANDLE (sender + `JoinHandle`). At a defined shutdown point — after
  restoring the terminal from the alt-screen, so a slow in-flight pass
  never delays giving the screen back — it drops the sender and JOINS
  the thread. The worker finishes its in-flight pass (including the
  focus restore, so an interrupted reconcile can't strand focus
  mid-steal) and exits when the drained channel disconnects. Dropping
  a `JoinHandle` detaches, so the join is explicit; pinned by a test
  where shutdown blocks until an injected in-flight pass completes and
  the thread is provably gone.
- A worker panic must never take the TUI down (thread boundary already
  guarantees that) and must not scribble on the alt-screen: the worker
  never prints.

## Out of scope

- The tab/pane RENAME mirroring (`TabIndicator`/`PaneStatus`) stays on
  the loop: its zellij ops are one cheap subprocess on change with a
  cached pane list, and moving it would race the reconciler's retitles.
  If renames ever measurably block the loop, that's its own plan.
- Any change to reconciliation semantics or the pane primitives.

## Acceptance

- No zellij subprocess runs on the TUI event-loop thread for
  reconciliation: after this plan, `observe` on the loop is a channel
  send.
- A slow relocation no longer freezes rendering/input (manual dogfood
  note in the plan on a live promote).
- Burst coalescing: N views queued before a pass reconcile once
  against the newest; arrivals during a pass produce exactly one
  follow-up pass (exact count + order pinned with injected `PaneIo`).
- Existing reconciler tests keep passing against the worker-owned
  state; startup convergence still runs.
- TUI exit JOINS the worker after the alt-screen is restored — no
  detached thread survives `run_tui` returning, and an in-flight
  pass's focus restore always runs.
- fmt/clippy at the 18/6 baseline; tests green; no binary-spawning
  tests.
