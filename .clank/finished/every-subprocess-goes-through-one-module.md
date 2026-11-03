# every-subprocess-goes-through-one-module
# Every subprocess goes through one module that bounds it and keeps fds from leaking

## Why

In `~/tmp/backup-bip-experiment` (2026-11-03), `qwen-v4-hp` was added to
the roster at 21:32 and its pane never opened. Its TUI (pid 3370,
started 17:28:55) never retitled a pane either. Diagnosis, from live
evidence:
- **The planning is right.** Run read-only against the live listing,
  `plan_panes` plans `add: ["qwen-v4-hp"]`.
- **The worker is blocked.** A `sample` shows the TUI's
  `ReconcileWorker` thread parked in `open_zellij::rename_pane_checked`
  → `Command::output()` → `read`. The `zellij rename-pane` child is
  gone; the TUI's only child is its monitor.
- **The monitor holds the pipe.** `lsof`: the TUI's fd 23 reads a pipe
  whose write end is fd 24. The monitor (pid 3393, spawned by this TUI
  in the same second) holds both fd 23 and fd 24, at the same numbers,
  with nothing at 17–22. It inherited the rename's output pipe.
- **The mechanism.** Rust std on macOS makes pipes with `pipe()` and
  sets `FD_CLOEXEC` in a separate step
  (`library/std/src/sys/pal/unix/pipe.rs`, 1.88). A process spawned on
  one thread inside that window, on another thread, inherits the pipe.
  A long-lived process (the monitor) then keeps it open forever, and an
  unbounded `output()` waiting for EOF never returns.

This is not one bad call site. About 167 places spawn processes
directly (`std`/`tokio` `Command::new`), and each makes its own rules:
- some are bounded and reaped (`open_zellij::run_open_query`);
- some read to EOF with no bound (`rename_pane_checked`);
- detached long-lived spawns (`monitor::spawn_monitor`, the web bridge)
  get stdio redirected but keep every other inherited fd.

Lloyd asked why the architecture allows this class of bug. The answer
is that process spawning has no owner. Git access had the same problem,
and `git_io`/`git_plumbing` plus the `git_boundary` gate fixed it.

## Inventory (read-only sweep, 2026-11-03)

There are 56 production spawn sites in `crates/cli/src`, and none in
`crates/core`. No production code calls raw `libc::pipe`, `fork`,
`posix_spawn` or `dup`. By lifetime:

| Class | n | Examples |
|---|---|---|
| EXEC | 2 | `run.rs` `clank run`, `agent.rs` `exec_composed` (identity scrubbed) |
| INTERACTIVE | 3 | `open_zellij::spawn_zellij` (attach), `diff.rs` `--wait` (SIGINT forwarded), `html.rs` opener |
| DETACHED | 3 | `monitor::spawn_monitor` (own group, a reaper thread waits), `diff.rs` no-wait (setsid, never reaped), `status_tui::open_in_browser` (**never reaped: a zombie per open**) |
| OWNED | 5 | `source_group` anchor + producers (**producers JOIN the anchor's group**; `Owners` records children, does `WNOHANG` collection, and retires TERM→KILL); `bridge_instance::start_host` (stdin/stdout readiness line, `shutdown\n`, 5s wait, then kill); `subscribe_panes` (kill+wait on drop); `github/relay` `gh webhook forward` (own group, `kill_on_drop`, stderr ring) |
| DUPLEX | 2 | `grok_launch::acp_rpc` (JSON-RPC over stdio, 1 MiB cap, 10s/30s budgets, group kill + 200ms reap); `stop_hook` `OwnedWait` (stdin = ownership channel, stdout = verdict frames, stderr 64 KiB, 10s abandon grace) |
| ACTION | 19 | the git_plumbing helpers (13, **no budget**, some with inherited stdio), zellij actions (`run_pane_write` 3s bounded; `rename_pane_checked`, `rename_tab`, `zellij_action` unbounded), `block.rs` user hook (unbounded), `owned_callback` (bounded, owner recorded), `opencode api` (unbounded) |
| QUERY | 22 | git_io (5; `bounded_git_stdout` is byte- but not time-bounded), `run_open_query` (3s, 1 MiB, group kill), `ps`/`zellij list-*`/`gh`/`sqlite3`/`claude agents`/`claude --version` (mostly unbounded) |

**Unbounded reads to EOF:** every git helper (`fetch_refspec` is a
network call), `gh pr view`, `claude agents --json`, `sqlite3`, both
`ps`, most zellij queries and actions, `opencode api`, and the `block`
hook. Most are sync calls made from long-lived processes (the TUI
worker, the monitor) as well as from the CLI.

## What changes

### 1. No child inherits a descriptor it was not given

**The mechanism lives at the spawn boundary.** It works for any
program, before that program starts. Every spawn through `proc_run`
installs one `pre_exec` hook. In the forked child, after std has wired
stdio onto 0–2 and before exec, it marks every fd from 3 up to a bound
`FD_CLOEXEC`. It marks them rather than closing them:
- The marked fds close at exec.
- An exec failure is still reported, because std's exec-error pipe is
  already CLOEXEC and is left working until exec.
- Fds the spec deliberately passes are mapped onto 0–2 by std before
  the hook runs. No current site passes anything above 2. If one ever
  needs to, the spec's `keep_fds` exempts it.

How the marking is done. It covers the descriptors actually open, not
a range up to a limit. An fd opened before the soft `RLIMIT_NOFILE` was
lowered can sit above it; codex reproduced fd 256 surviving a 3..64
loop.
- **Linux:** `close_range(3, ~0U, CLOSE_RANGE_CLOEXEC)` covers every
  descriptor whatever its number. On `ENOSYS`/`EINVAL` (a kernel before
  5.11), the hook fails the spawn with that error rather than fall back
  to a partial loop. clank targets current kernels; the error names the
  requirement.
- **macOS:** `proc_pidinfo(getpid(), PROC_PIDLISTFDS, …)` lists the
  child's own open fds into a buffer the parent allocated before the
  fork. The child then marks each listed fd ≥ 3 that is not in
  `keep_fds`. `proc_pidinfo` is a single syscall into caller memory,
  with no allocation or locks, so it is safe after fork.
  - If the listing fills the buffer, it may be incomplete. The hook
    then returns an error. `proc_run` retries the spawn once with a
    buffer sized from a fresh parent-side count times two, and reports
    the failure if that fills too.
  - Any `fcntl` error other than `EBADF` (an fd closed after the
    listing) fails the spawn.
  - The cost scales with the fds actually open, not with the limit.
- Both paths call only async-signal-safe syscalls after the fork.
- Exec-failure reporting stays intact: std's error pipe is already
  CLOEXEC, and nothing here closes it before exec.

Using `pre_exec` moves std off `posix_spawn` and onto fork/exec. That
is accepted for every spawn.

This replaces the earlier "monitor sheds fds at entry", which ran too
late (the tokio runtime and tracing have already initialised) and
cannot apply to programs that are not clank.

### 2. One module owns process mechanics; protocols stay with their owners

`crates/cli/src/proc_run.rs` is the only production code that names
`std::process::Command`, `tokio::process::Command`, `Child`, or the raw
`spawn`/`output`/`status`/`exec` calls.

**`Spec`** describes a command:
- program and argv;
- cwd;
- env set and env remove;
- identity, as an explicit policy:
  - `Identity::Inherit`;
  - `Identity::ScrubInherited`: remove the inherited
    `SESSION_IDENTITY_VARS`, then apply the spec's env. This is today's
    agent launch, so the intended `CLANK_AGENT` and session vars
    survive;
  - `Identity::Forbid`: remove them after the spec's env too. This is
    today's ACP and preparation path, so a profile cannot re-insert an
    identity.
- stdin: `Null | Inherit | Bytes(Vec<u8>) | Piped`;
- stdout and stderr, each: `Null | Inherit | Capture { cap } | Piped |
  Ring { bytes }`;
- group: `New | Join(Pgid) | Inherit`;
- `keep_fds`.

**Lifetimes**, one entry point each:
- **`run(spec, budget) -> Result<Output, RunError>`**, in async and
  blocking forms, for QUERY and ACTION. It drains stdout and stderr
  concurrently while the child runs, so a full pipe cannot deadlock it.
  - **Complete** is decided by the child's exit and the pipes' actual
    state, with no extra grace period.
    - Once the child has exited, every byte it wrote is already in the
      pipe. So after the exit `run` drains each capture with
      non-blocking reads until either EOF (complete) or `EAGAIN` with
      the pipe empty.
    - An empty pipe that is not at EOF has another writer still holding
      it, and that is `Incomplete`. This is decided at once and
      deterministically, not after a timer.
    - A reader scheduled late after the exit is not penalised. Only the
      caller's overall budget bounds the work.
  - **Over a cap:** `RunError::Overflow`. A truncated capture is never
    returned as a whole one.
  - **EOF not reached within the drain:** `RunError::Incomplete`. A
    descendant, or anything else, still holds a pipe, so the output is
    not known to be whole. With §1 in place, that can only be the
    child's own descendants.
  - **Budget passed:** `RunError::Timeout`.
  - On every error path the `New` group is SIGKILLed, the child is
    reaped (bounded), and the reader tasks and fds are gone before
    return. The budget is required.
  - Long legitimate operations (`git fetch`, `rebase`, the `block`
    hook, `git commit` running user hooks) get large, named budgets.
    Each site's budget is a const next to it.
  - Existing bounds are kept as they are: the zellij 3s and 1 MiB,
    `owned_callback`'s deadlines, `probe_launch_claude`'s 5s,
    `ACQUIRE_TIMEOUT`.
- **`start(spec) -> Managed`**, for OWNED, DUPLEX and DETACHED. It owns
  the child and hands out its piped stdin, stdout and stderr, and its
  pid/pgid.
  - **`Managed::wait` / `try_wait`:** the reaping primitives.
  - **Dropping a `Managed` under `group: New`** kills the group. It
    then reaps within a short bounded wait, and if the child is not
    reaped by then, transfers the pid to the reaper. A child is never
    silently lost and never waited on forever on the caller's path.
  - **The reaper** is multiplexed and event-driven. It registers each
    transferred or detached pid with the existing
    `proc_identity::exit_watch` (kqueue `EVFILT_PROC`/`NOTE_EXIT` on
    macOS, pidfd + epoll on Linux), and calls `waitpid(pid, WNOHANG)`
    only for pids that have signalled exit.
    - A long-lived child, such as the monitor, never blocks the reaping
      of a later short-lived one.
    - Each pid has exactly one wait owner. Tokio children keep tokio's
      own orphan reaping; `std` children go to this reaper.
    - `source_group` keeps its own `WNOHANG` collection of its children,
      and those pids are never registered with the reaper.
    - At runtime shutdown, children still registered stay registered
      until process exit. The reaper holds no task the runtime must
      join.
  - **Under `Join(pgid)`**, dropping kills nothing beyond the pid's own
    reaping. The group's owner keeps signalling and retirement. This
    preserves `source_group::Owners`: the anchor/producer join, its
    `children` accounting and `WNOHANG` collection, and `retire_after`.
  - **`Managed::detach()`** keeps the process running past its owner.
    The reaper owns its wait, so `open_in_browser` and `diff`
    no-wait stop leaving zombies, and the monitor's existing
    `supervise` thread becomes this.

  The domain protocols stay where they are, built on `Managed`'s
  handles:
  - the bridge's readiness, `shutdown\n` and 5s stop;
  - ACP JSON-RPC;
  - `OwnedWait`'s ownership channel;
  - the anchor's arm handshake;
  - the relay's stderr ring.
- **`interactive(spec) -> ExitStatus`**: terminal inherited, waited on
  with no budget, on purpose. `diff --wait` keeps its SIGINT
  forwarding.
- **`exec(spec) -> io::Error`**: on success it never returns; on
  failure it returns the error, as `CommandExt::exec` does. This keeps
  `agent::launch_consuming_fork`'s restore of consumed fork intent
  when the exec fails.

`wait::GroupKill` and `signal`'s group primitives stay, and `proc_run`
uses them. `run_open_query`, `run_pane_write`, `bounded_git_stdout`,
`scan_sessions`' reader threads and the various ad-hoc rings become
`run` or `start` calls. Each git helper in `git_io`/`git_plumbing`
builds a `Spec` and calls `proc_run`, keeping its typed API. Every
site keeps its current stdio: `git_plumbing::run`'s inherited stdio,
which user hooks may use, stays `Inherit`. The migration changes
mechanics, not contracts. Where the inventory shows inherited stdin on
an otherwise captured read (`bounded_git_stdout`, `ScratchIndex::git`),
that is kept too unless a caller-specific reason is recorded in "As
landed".

### 3. The gate

`crates/cli/tests/it/process_boundary.rs` scans production code across
the workspace. It fails when anything outside `proc_run.rs` names
`std::process::Command`, `tokio::process::Command`, or
`process::Command`, through any `use`, alias or qualified path, or
calls `CommandExt::pre_exec`/`exec`. Its negative fixtures cover:
- `use std::process::Command;`;
- `use tokio::process::Command as C;`;
- `std::process::Command::new`;
- a glob `use std::process::*;` followed by `Command::new`.

`git_boundary`, `signal_boundary` and the `zellij_*` gates stay. The
git gate still confines git to its two modules, and those modules now
reach the OS only through `proc_run`.

### 4. One implementation commit

All 56 sites move in one implementation commit, with the inventory
above as its checklist. The "As landed" section records any site whose
class changed.

## Acceptance

- **No inheritance, with real children** (each fd test runs in a
  spawned child, never closing the test runner's own fds):
  - the parent opens a sentinel fd without CLOEXEC, then spawns a
    small fixture program through `proc_run` that reports its open
    fds. The sentinel is absent, and 0–2 are present;
  - a `keep_fds` fd is present;
  - a nonexistent program reports its spawn error, so exec-failure
    reporting is intact.
- **The bug itself.** A fixture child forks a long-lived grandchild
  that keeps the inherited stdout write end, then exits. `run` returns
  `Incomplete` at exit plus the drain, well inside its budget, and the
  group kill takes the grandchild. Restoring "wait for EOF" makes the
  test hit its own bound and fail.
- **Large output and slow readers.** A child writing 8 MiB with a 16
  MiB cap returns complete. Over the cap, the result is `Overflow`,
  never truncated `Ok`. A child that writes its output slowly, with
  its total time inside the budget, still completes.
- **Cleanup on every exit:**
  - timeout;
  - async cancellation (dropping the future);
  - spawn error;
  - a DUPLEX protocol error;
  - a normal exit with a live descendant.

  In each case the child is reaped (`waitpid` reports ECHILD after) and
  a `New` group is empty. An unrelated process group, created by the
  test, is untouched.
- **fd coverage:**
  - an inheritable fd at 256, opened before the soft limit is lowered
    to 64, is absent in the child;
  - `keep_fds` is honoured on both platform paths;
  - a listing that fills the buffer fails and is then retried;
  - an injected non-`EBADF` marking error fails the spawn;
  - a before/after timing of a small `ps` spawn is recorded in "As
    landed", with the acceptable bound stated there.
- **Completion:** a child exits and the reader starts over 100ms later,
  still inside the budget. The output is complete and not rejected.
  The inherited-writer fixture still returns `Incomplete`.
- **Reaper:** child A is alive and child B has exited. B is reaped
  while A still runs. Cancellation, runtime shutdown and `detach()`
  each leave no zombie.
- **Launch:**
  - a nonexistent program given to `exec` returns its error, and the
    consumed fork intent is restored;
  - a real agent launch has the inherited identities absent and the
    intended `CLANK_AGENT` present;
  - ACP/preparation has no agent identity.
- **Git stdio:** `git_plumbing::run` still inherits stdio, pinned by a
  spec assertion.
- **Managed:**
  - a fake duplex executable answers over stdin/stdout;
  - `Join(pgid)` puts the child in the given group, and dropping it
    does not signal that group;
  - `detach()`ed children are reaped by the reaper with no
    zombie.
- **source_group:** the existing anchor/producer, arm and retirement
  tests pass unchanged.
- **The TUI.** `rename_pane_checked` and every reconcile-worker zellij
  call return within their budget when the child never exits.
- **The gate.** `process_boundary` passes on the tree, and each
  negative fixture fails it.
- **Mutation checks:** each of these fails a test:
  - the CLOEXEC hook removed;
  - waiting for EOF instead of exit plus drain;
  - an overflow returned as `Ok`;
  - the group kill on timeout removed;
  - a serial blocking reaper (B stranded behind A);
  - looping to the soft limit instead of the listing;
  - a fixed post-exit grace;
  - `exec` exiting instead of returning its error;
  - `Join` dropping signalling the group;
  - the gate missing aliased imports.
- **Test runs:** the touched modules' `-p clank --lib` filters
  separately; `--test it` `process_boundary`, `git_boundary`,
  `signal_boundary`, `zellij_cost_boundary` and
  `zellij_ownership_boundary`; then the whole lib.

## Live remedy (not part of the code)

Restarting a TUI whose monitor inherited a pipe releases it. On
2026-11-03 the backup-bip TUI and monitor were restarted out of band,
and `qwen-v4-hp` then opened.

## As landed

The structure is as planned: `proc_run.rs` owns every spawn, with `Spec`, `run`/`run_blocking`, `start -> Managed`, `interactive`, a fallible `exec`, and the multiplexed reaper. `tests/it/process_boundary.rs` exempts only `proc_run.rs`. Where it differs from the plan:

**Descriptors on macOS.** macOS does not use the `pre_exec` fd listing in §1. It uses `posix_spawn` with `POSIX_SPAWN_CLOEXEC_DEFAULT`, which closes inherited descriptors in the kernel with no listing, no retry, and no fork. Each part of the spec has a native equivalent:

| Spec | posix_spawn |
|---|---|
| `New` / `Join` group | `SETPGROUP` |
| `Session` | `SETSID` |
| stdio | `adddup2` / `addopen("/dev/null")` |
| `Inherit`, `keep_fds` | `addinherit_np` |
| cwd | `addchdir_np` |
| `exec` | `SETEXEC` |

The program is resolved with execvp's rules, from the child's own directory and `PATH`. This process's own directory is consulted only when the child changes directory and the match is still relative. Linux keeps `close_range` in `pre_exec` as planned. It has not been compiled, because this machine has no Linux toolchain.

**flock leases.** Every macOS spawn path briefly shares the parent's descriptors with the child before closing them: fork, std's `posix_spawn`, and ours. While it does, a lease that was just released can still read as held. In 3000 cycles against concurrent spawns, std's `posix_spawn` showed 65–80 spurious Busy results and fork about 400. So every non-blocking lease take now goes through `clank_user_config::lease`:
- `take` retries Busy for 250ms.
- `take_now` makes a single attempt, for callers that already poll to a deadline: the monitor ownership poller, the repair permit, the status lease probe, and the IngestLease contender loop.
- When another process genuinely holds the lease, a one-shot take now costs 250ms.

**Runs.** One deadline set at spawn bounds the whole run, including accepting completion. Each pass reads a bounded amount from each stream. Completeness is decided while the exited child is still an unreaped zombie (`waitid WNOWAIT`), after draining, and only then is the group killed and the child reaped. Nothing signals a reaped pid or group.

**Lifetime changes:**
- `diff --wait` uses `start` plus its own SIGINT forwarding instead of `interactive`.
- `owned_callback` uses `start` instead of `run`, because it records its owner between spawn and the stdin write.
- `scan_sessions` runs one `run_blocking` per session on scoped threads.
- Captured sites that used to run in clank's process group now get a `New` group.

**Spec additions.** `Stdin::Fd` is used for the source_group anchor and `Group::Session` for detached diff. `Stdin::Bytes` is not implemented, because no site needs it.

**web-cloudflare.** The tunnel's spawn reaches `proc_run` through an injected `tunnel::Spawn`, passed in from the cli binary and the status TUI.

**Spawn cost** (`/usr/bin/true`, debug build): 714µs against std's 682µs, about 5% more. The fork-based draft cost about 2×.
