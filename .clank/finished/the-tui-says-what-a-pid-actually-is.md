# the-tui-says-what-a-pid-actually-is
# The TUI says what a process actually is, not just its number

## Why

The wait detail panel offers to kill a process and tells you almost nothing about it:

```
doing      v3 gates
task       b0ufmfqmt
started    11m/2h ago
process    pid 95810, running
```

`pid 95810, running` is the entire description of the thing the panel is about to SIGTERM.
A number and a liveness bit. To decide whether to stop it you have to know what it IS — and
the answers are one `ps` away but never shown:

- **The command.** What binary is this? `claude`, `codex`, a `wait_events` command the agent
  configured, a hook?
- **The arguments.** `--agent <profile>`, the repo, the model — the argv is what distinguishes
  one agent from another and a hook from the thing it hooked.
- **How it was started.** By clank (which command), by a stop hook, by the user in a pane?
  Its parent and its process group say a lot that the pid alone does not.
- **What it is doing now** beyond "running": sleeping, stopped, a zombie.

The panel already does the hard part — it resolves a *safe, identified* target
(`Attended::killable_pid`, validated against a `ProcToken` so a recycled pid cannot be
signalled). Having gone to that trouble it then shows the one field that does not identify
anything.

## What to show

The panel is narrow, so this is a selection problem rather than a dump. A first proposal,
to be argued with:

```
process    pid 95810 · running
command    claude --agent reviewer
started    14:22:07 · 11m
parent     clank wait (pid 95702)
group      95702
```

- **command** — argv0 plus arguments, elided from the MIDDLE rather than the end, because a
  long absolute path makes the end (the arguments) the part that matters.
- **started** — the process's own start time, which is also the identity `ProcToken` holds.
  The panel already shows an age for the WAIT; this is the PROCESS, and they are not the
  same thing when a wait outlives a respawn.
- **parent / group** — how it got here. The group matters because killing is deliberately
  single-process (`signal_process`, never the group), and seeing a group with other members
  tells the user what will be left behind.

Expand or collapse the detail if the panel is too narrow; do not drop the command.

## Where the data comes from

`crate::proc_identity` already reads per-process birth identity cross-platform (macOS
`proc_pidinfo`, Linux `/proc/<pid>/stat` + boot id) and is what proves a pid is still the
process clank recorded. argv and the parent are a second question it does not answer.

Whatever is added must be read through a module that owns it, not ad-hoc at the call site —
the repo has two such boundaries already (`git_io`/`git_plumbing` for git, `crate::signal`
for signal targets), and process inspection deserves the same treatment rather than a `ps`
subprocess sprinkled through the TUI.

Note the existing constraint, discovered the hard way: `ps -E` cannot read another
process's environment on current macOS. Argv is readable; the environment is not.

## Acceptance

- The panel names the command and its arguments for the process it is offering to stop.
- A process whose details cannot be read says SO, and still offers what it has. "Unknown"
  beside a field is an answer; a blank line is not, and a panel that hides the kill control
  because one field is missing is worse than the current state.
- The identity shown is the identity CHECKED. If the panel prints a start time, it is the
  same one `ProcToken` compares — not a second reading that could disagree.
- Reading process details never blocks the TUI's render loop.
- Cross-platform: macOS and Linux both, through one function with one stated contract, and
  "this platform cannot say" is an answer rather than a guess.
- Driven by tests on the FORMATTING and the elision given a known process shape, not by
  running `ps` in a test.

## Out of scope

- Changing what the kill does. It stays a single-process SIGTERM against a validated,
  token-checked target.
- A process tree view, or killing a group.
- Showing this anywhere but the wait detail panel; if the log or the header wants it too,
  that is a separate plan.

## Implementation contracts

The current code confirms the scope: `status_tui/render.rs::render_wait_page` prints
`pid ..., running`, `input.rs::WaitPage` tracks an attendance instance, and
`status_tui/mod.rs::resolve_kill_target` rereads the roster and attendance after confirmation.
Preserve that final signal-validation path. This work adds explanatory observations;
cached display data never becomes authority to signal.

1. **One process-inspection boundary.** Extend `proc_identity` or add a companion module
   that owns platform reads and exposes typed observations to the TUI. Given a positive
   PID and the recorded `ProcToken`, it returns details of the matching process or an
   explicit unavailable/mismatched outcome. Bound input sizes and any subprocess lifetime.
   Prefer per-process native reads; never enumerate all processes to describe one target.
   Do not read process environments.
2. **Checked identity and partial results.** Check the recorded token around reads of
   independently obtained fields. A changed or unverifiable identity discards those
   details rather than attaching a stranger's command to the recorded wait. Failure to
   read argv, state, parent or group alone is a per-field unknown, not an identity failure
   and not a reason to remove an otherwise valid kill action. Distinguish loading,
   unavailable details, ended process and identity mismatch. Process state is an
   observation at inspection time; neither a sleeping process nor a zombie is described
   as actively running merely because `kill(pid, 0)` succeeds.
3. **Start and provenance.** Label the existing `at` age as the wait's age and process
   birth separately. macOS birth comes from the checked seconds/microseconds; Linux
   birth comes from the checked boot ID and start ticks. A wall-clock rendering on Linux
   must document its conversion and approximation, or show an explicit unavailable
   value; never substitute the attendance timestamp. Parent PID and process group are
   observed relationships, not proof that a particular Clank command launched the
   target. If showing a parent's command, check that parent observation too and allow
   unknown or changed parents. Do not infer launch provenance or enumerate group members.
4. **On-demand, bounded work.** Inspect when the wait page opens, off the render/input
   loop, and deliver completion through the TUI's existing event channel. Bind the
   request to a generation plus attendance instance, PID and token; ignore late results
   after closure, replacement or navigation. Permit a deliberate user refresh to learn
   changed state. No periodic process polling, per-frame reads, inspection on every
   status refresh, idle worker activity or unbounded concurrent requests. Coalesce
   requests and bound outstanding work even under rapid navigation. Cached details
   are visibly an observation, and remain independent of fresh confirmation checks.
5. **Readable at supported sizes.** Command and arguments must remain reachable when
   the page is narrow or short. Apply middle elision to the available value width after
   the field label, using terminal display width, and provide wrapping/scrolling or an
   equivalent way to inspect the full command. Sanitize control characters in OS text
   before rendering. Separate unavailable fields from empty-but-valid arguments. Keep
   the existing back and stop actions reachable.

## Milestones and evidence

1. Implement the typed inspection boundary and platform adapters. Use deterministic
   fixtures for Linux stat/cmdline parsing, macOS buffer decoding, identity changes,
   partial permissions failures, invalid PIDs and bounded/truncated input. Existing
   native identity tests may be reused; formatting tests must not depend on `ps`.
2. Integrate an on-demand request/completion state with the wait page. Test late and
   replaced requests, closure, explicit refresh and bounded scheduling through an
   injectable inspection seam, including no requests while the page is closed.
3. Render known and partial observations at narrow and short sizes, with Unicode and
   control-character fixtures. Retain existing confirmation and recycled-PID tests;
   demonstrate that display failures cannot authorize a kill or disable one solely
   because explanatory details are missing. Run focused affected suites and establish
   macOS/Linux adapter build coverage. No provider, roster, hook or monitor changes
   are needed for this TUI-only feature.

## Progress — typed inspection boundary

Milestone 1 is implemented in `proc_identity::inspect`. Its public `inspect(pid,
recorded_token)` returns verified observations, an identity mismatch, an explicitly
ended process, or an unavailable identity. Verified observations retain the exact
checked birth token and observation time, with independent results for argv, state,
parent PID and group. They cannot authorize a signal. The wait page and its existing
post-confirmation roster/attendance/token validation are unchanged in this milestone.

The identity read brackets every explanatory read; a changed or unreadable final
identity discards the entire result. Invalid PIDs and unrecognized token versions
are rejected before inspection. Native adapters use only selected-process reads:
macOS shares the existing BSD birth decoder, reads task running-thread counts to
avoid labelling sleeping tasks as running, and decodes the native argument response;
Linux shares one bounded stat decoder between birth identity and inspection and
reads the selected process's cmdline. Parent and group are observed numeric IDs;
there is no launch-provenance guess or parent-command lookup.

Argument bytes and empty arguments are retained for subsequent safe rendering.
Reads are limited to 1 MiB of argument bytes and 16,384 arguments, 16 KiB for Linux
stat and 128 bytes for boot identity. macOS requests the bounded kernel argument
maximum because a short native buffer can return the tail of an argument block.
Its combined response can contain an environment suffix: the decoder stops at
exactly argc and never decodes or retains that suffix. No environment lookup,
subprocess, retry, process-table scan, resident worker, timer or polling was added.
Linux wall-clock birth conversion is deliberately absent; the returned ticks and
boot ID are the exact recorded identity, ready for an honest presentation.

Validation:

- `cargo test -p clank --lib proc_identity:: --locked --offline`: 22 passed,
  covering both platform decoders, birth replacement and disappearance, denied
  fields, invalid inputs, bounded reads, malformed arguments, empty/non-UTF8
  arguments, complex Linux comm fields, OS state mapping, existing persisted-token
  semantics, and a native macOS inspection of the test process. Baseline 21 test
  warnings remain; no new warnings were introduced.
- Existing wait-page happy-path and recycled-PID refusal tests: 2 passed. No
  signal was sent by these resolver tests.
- Linux adapter build coverage: an isolated temporary Cargo harness symlinks the
  actual `proc_identity.rs` and child module sources and uses exactly this lockfile's
  libc/serde/time versions. `cargo check --target x86_64-unknown-linux-gnu --offline
  --locked --lib` passes with no warnings. This is a type-check, not Linux runtime
  testing. The initial whole-CLI cross-check could not build its unrelated C
  dependencies because `x86_64-linux-gnu-gcc` is absent; no compiler was installed.
- Formatting and `git diff --check` passed. No provider, roster, hook, monitor,
  runtime-state, or installed-binary changes were made.

Next milestone: the bounded on-demand wait-page request/completion state, followed
by narrow/short rendering, explicit refresh and full-command navigation.

### Review correction — macOS disappearance errno

The review of `5b2a4fc0c216` identified a gap in the disappearance fixtures:
macOS libproc reports a missing PID with raw ESRCH, which this Rust build does
not map to `ErrorKind::NotFound`. The earlier synthetic NotFound fixtures
therefore proved the inspection state machine but missed the native adapter.

The macOS process-read boundary now translates only ESRCH to NotFound before
generic I/O classification, retaining the original error as its cause. BSD
birth/relationship reads and task-state reads share that translation. Other
errnos, including permission failures and EINVAL, keep their original raw value
and kind; unavailable identity still makes no liveness claim. Both initial
disappearance and death between birth checks now produce `Inspection::Ended`,
with no details retained. Public `token_for` continues returning None for an
unreadable or absent process. No new reads, retries or polling were introduced.

Validation: all 26 `proc_identity::` tests pass. New evidence covers raw ESRCH
translation without reclassifying EPERM/EACCES/EINVAL/EIO, raw ESRCH at either
birth check, raw ENOENT at either check, and an actual native macOS lookup of a
nonexistent PID returning Ended. The existing Linux adapter harness still
type-checks the actual module sources successfully with no warnings. Formatting
and `git diff --check` pass; the async TUI integration remains the next milestone.

## Progress — bounded on-demand wait-page integration

Milestone 2 is implemented. Opening a wait page requests one native inspection
off the input/render loop; `r` requests a deliberate refresh. The completion
wakes the existing TUI event channel, including when a document overlay hides
the page. The page now reports loading, ended/replaced/unavailable observations,
or the observed OS state with a fixed UTC observation time. It does not describe
sleeping processes as running, and failed explanatory reads leave the existing
freshly validated stop control independent of cached display data.

`wait_process::Controller` owns one running request and one current desired
slot. While a native read runs, navigation/refresh overwrites the desired slot;
the next dispatch takes only that latest request. Each dispatch starts one
short-lived thread, which exits after sending its result. There is no resident
worker, periodic wake, process-table scan, retry loop or new timer. Repaints,
resizes, successful status refreshes and unchanged completed pages request no
more reads. Missing, invalid and unsupported recorded identities require no
thread or native inspection. Thread creation failure and an unwinding inspector
produce unavailable results instead of permanently reserving the running slot.

Every request binds a session-wide generation and the attendance's label, task,
timestamp, PID and full recorded birth token. Closing a page drops the desired
slot; an already dispatched read can finish but its answer is discarded. The
same page reopened or explicitly refreshed has a different generation. A
completion for a different request cannot clear a newer running reservation.
Snapshot reconciliation only retains, relocates or invalidates a page; it does
not schedule new work. PID/token changes invalidate the page even if task and
timestamp are unchanged, and rendering checks the selected attendance against
the cached request before using its observation. The existing post-confirmation
roster/disk attendance/token validation remains authoritative; the attendance
comparison now also includes the PID and birth captured when the page opened.
The signal remains a single-process SIGTERM.

Validation:

- 12 focused request-path tests passed: no closed/idle/completed-page work,
  1,000 rapid replacements coalesced to the latest request, closure cancellation,
  reopening/refresh generation rejection, all attendance/birth key components,
  unsupported identities, a deterministically blocked fake inspector allowing
  foreground navigation, inspector panic completion, status-refresh/reorder
  behavior, PID/token replacement invalidation, completion batching behind an
  overlay, and partial-error rendering preserving the stop action.
- All four affected unit modules (`status_tui::tests`, `input::tests`,
  `render::tests`, `wait_process::tests`): 325 passed, including existing
  confirmation and recycled-PID refusal coverage. Baseline 21 test warnings
  remain; no new warnings were introduced.
- Git and signal boundary modules: 4 passed. The production library retains
  its seven baseline warnings. Formatting and `git diff --check` passed.
- No provider, roster, hook, monitor, persistent runtime-state or installed
  binary changes were made; no Linux platform code changed in this milestone.

Next milestone: present command/arguments, checked process birth and observed
parent/group, with safe terminal text, middle elision, full-command navigation
and a short/narrow layout that keeps actions reachable. This milestone stores
those native fields but intentionally does not yet add their presentation.

## Progress — complete process presentation

Milestone 3 is implemented. The command is the first fact and uses cell-aware
middle elision after subtracting its label's width. `c` opens the complete
escaped argv in a read-only document; arrows, Page Up/Down, Home/End and the
existing document back keys work there. Empty arguments, argument boundaries,
repeated spaces, quotes, backslashes and non-UTF8 bytes remain distinguishable.
Control characters, terminal escape sequences, bidi controls and zero-width
characters are visible escapes, never active terminal input. The compact
preview preserves spaces rather than applying word wrapping to argv.

The inspector thread prepares the command representation and its display width
once. The accepted slot and full-command document share that immutable
allocation by Arc. Compact rendering visits only the visible prefix/suffix;
full-command rendering and forward/reverse navigation visit only the requested
window of bytes. Neither repaint nor resize re-encodes or wraps the whole
potentially large command, and there is no full-command line index. The native
read limits and one-running/latest-desired request contract remain unchanged.
No periodic inspection, retry, process-table scan, subprocess, resident worker
or new timer was introduced.

`born` is derived solely from the verified observation's full checked token:
macOS displays UTC birth with microsecond precision; Linux displays the exact
boot ID and start ticks with wall-clock time explicitly unknown. This avoids
guessing a wall-clock conversion or substituting attendance time. `wait age`
is separate. Parent PID and process group are numeric OS observations, not
inferred launch provenance or group membership. A reported zero is explained;
unreadable fields carry explicit reasons, including permission failures and
unknown OS states. Cached process state includes its fixed observation time.

Facts scroll separately from the action rows. Layout reserves reachable stop
and back controls before facts or chrome; at a single-row height it displays
the selected action and up/down still reaches the other action. Narrow layouts
put labels above values, shorten the action labels and retain the full command
through `c`. Page Up/Down and Home/End navigate the facts. Display failures do
not change kill eligibility, confirmation, fresh roster/attendance/token
validation or the single-process SIGTERM target.

Validation:

- All 474 `cli::status_tui::` tests passed, including existing confirmation and
  recycled-PID guards. New fixtures cover escaped controls/bidi/non-UTF8 argv,
  empty arguments and repeated spaces, Unicode cell budgets, exact full-command
  reconstruction, bounded windows over a 1 MiB command, precise checked birth,
  partial metadata, action reachability across 56 pane sizes and both selections,
  and off-thread preparation with the same cached allocation retained by the page.
- Git and signal boundary modules: 4 passed. Formatting and `git diff --check`
  passed. The existing seven production/21 lib-test warnings are unchanged.
- An initial sandboxed TUI run passed 463 tests and denied socket creation in
  ten remote-view fixtures. Those ten passed with local socket access; the final
  474-test run also passed with that access. An earlier overly broad `wait_`
  filter selected three unrelated monitor/Grok socket fixtures; all three
  likewise passed when rerun with socket access. No real providers were invoked.
- macOS native compilation remains covered by the affected unit and integration
  builds. This milestone changes no platform adapter; the earlier successful
  isolated Linux adapter type-check and documented missing cross C compiler
  limitation still apply. No installed binary, provider, roster, hook, monitor
  or persistent runtime-state changes were made.

All three implementation milestones are complete; finalization remains gated
on reviewer feedback.
