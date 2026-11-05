# unsafe-lives-in-named-modules
# Unsafe code lives in a few named modules, and the rest of clank forbids it

## Why

No crate rejects unsafe code today: there is no `forbid` or `deny` of
`unsafe_code` anywhere. There are about 180 `unsafe` blocks, spread over
roughly 40 files: 45 in `cli/monitor.rs`, 42 in `proc_run.rs`, 18 in
`proc_identity/exit_watch.rs`, and one to ten in each of the rest. All
of them are libc calls:
- `getuid`, 43 times;
- raw-fd handling (`as_raw_fd`, `from_raw_fd`, `fcntl`);
- `flock`;
- `kill`, `killpg`, `waitpid` and `getpgid`;
- posix_spawn;
- `poll`, `kqueue`, `ioctl` and `tcsetattr`.

`core` and `web` have none.

A public crate's unsafe surface is one of the first things a reviewer
checks. Scattered `unsafe` means every module is a place memory safety
can break. Most of these calls have safe wrappers in `rustix`, which is
already in `Cargo.lock` (1.1.5, through dependencies).

## Inventory (2026-11-05, after `security-review-for-release` landed)

About 180 `unsafe` blocks across about 50 files. By kind:

- **Trivially safe libc calls**, about 60:
  - `getuid` / `geteuid` / `getppid`, in monitor, source_status, the web
    question modules, codex_interrupt, wake_receipt, status_tui,
    monitor_upgrade and tests;
  - `flock`, in agent_store, stash, setup, question_state,
    plan_mutation, pane_owners, pane_input, user_command/store,
    status_tui/lease, github/log, user-config `lib.rs` and `lease.rs`,
    and web-cloudflare `door.rs`;
  - `access`, in post_rewrite and build_stamp;
  - `futimens`, in claude_trust;
  - `ttyname_r`, in source_diagnostic;
  - `read`, in status_tui;
  - `fcntl` and `poll`, in monitor and source_diagnostic.
- **Processes:**
  - `getpgid` and `waitpid` in source_group, wait and command_source;
  - `kill` and `killpg` in `signal.rs`, web/bridge_instance and
    web-cloudflare `tunnel.rs`.
- **Spawning and fds** in `proc_run.rs`: posix_spawn*, `close_range`
  through `syscall`, raw fds.
- **Exit watching and inspection:**
  - `proc_identity/exit_watch.rs`: kqueue or pidfd, raw fds;
  - `proc_identity/inspect.rs` and `proc_identity.rs`: `proc_pidinfo` /
    `sysctl` into zeroed structs.
- **Terminal and signals:**
  - `cli/term.rs`: `tcsetattr`, `ioctl` (window size), signal handlers,
    and a saved-termios pointer for async-signal-safe restoration;
  - `main.rs`: SIGPIPE reset.
- **Login home:** web-cloudflare `secret.rs` uses `getpwuid_r` with
  `zeroed` and `CStr::from_ptr` to find the account's login home. It
  must stay a trusted passwd lookup, not `HOME`.
- **macOS exclusive rename:** `renamex_np` in `priority_dir.rs`.
- **Odd ones:** `read_volatile` in build_stamp, and the monitor's
  socket creation.
- **Tests:**
  - `signal_boundary.rs` (`kill`, `killpg`);
  - `process_boundary.rs` (`pre_exec`);
  - `proc_run_tests.rs` (`getrlimit`/`setrlimit`, `fcntl`, `getpgid`);
  - `wake_receipt_acceptance.rs` (`mkfifo`);
  - `html_integration.rs` (`std::env::set_var`, unsafe in edition 2024);
  - a few `getuid` calls.

## What changes

### 1. Safe replacements, through `rustix`

`rustix` becomes a direct dependency of `clank`, `clank-user-config` and
`clank-web-cloudflare`. It is already in `Cargo.lock` as 1.1.5.

| Call | Replacement |
|---|---|
| uid/euid/ppid | `rustix::process` |
| flock | `rustix::fs::flock` with `FlockOperation`, keeping non-blocking, contention and EINTR behaviour |
| access | `rustix::fs::access` |
| futimens | `rustix::fs::futimens` |
| ttyname | `rustix::termios::ttyname` |
| read | `rustix::io::read` on a `BorrowedFd` |
| fcntl flags | `rustix::io` / `rustix::fs` fcntl helpers, keeping CLOEXEC |
| poll | `rustix::event::poll`, keeping a no-event timeout as a timeout |
| tcsetattr / window size | `rustix::termios` |
| waitpid / getpgid | `rustix::process` |
| kill / killpg | `rustix::process::{kill_process, kill_process_group, test_kill_process}` |
| socket | `rustix::net` |
| exclusive rename | `rustix::fs::renameat_with` with `NOREPLACE`, where rustix supports it on both platforms; otherwise a residual (below) |
| mkfifo (tests) | `rustix::fs::mknodat` with a FIFO type |

**APIs keep ownership and validation in their types:**
- fd-taking functions take `BorrowedFd` / `AsFd` / `OwnedFd`, never raw
  integers. Nothing becomes a safe wrapper around `borrow_raw` /
  `from_raw_fd` over an arbitrary int.
- Signal APIs keep the existing validated PID / PGID newtypes and their
  guards: no 0, no -1, no own group. Signal-0 probes stay separate from
  real signals.

### 2. Residual unsafe: named modules and items, each justified

Only these may contain `unsafe` or `allow(unsafe_code)`. Each entry in
the scanner's allow-list names the file, the item path and a one-line
reason. The list is the contract, and it grows only by an edit that a
reviewer sees.

- `cli::proc_run::spawn`: posix_spawn attrs and file actions,
  `close_range`, and child fd setup. No safe API covers these.
- `cli::proc_identity::exit_watch::register`: only the `kevent`
  registration of `EVFILT_PROC`. rustix 1.1.5 creates the queue safely
  (`event::kqueue`, `event/kqueue.rs:395`), and Linux uses the safe
  `process::pidfd_open`, which returns an `OwnedFd`
  (`process/pidfd.rs:29`). `kevent` / `kevent_timespec` stay unsafe
  because each registered descriptor must outlive the queue. The SAFETY
  comment states how the watcher's ownership guarantees that.
- `cli::proc_identity::inspect`: `proc_pidinfo` and `sysctl`
  process-identity structs. These are FFI out-params.
- `cli::term::restore`: installing the SIGINT/SIGTERM handler, and the
  async-signal-safe saved-termios restore that it calls. The handler
  must be raw `sigaction` with a static.
- `cli::main::reset_sigpipe`: one `libc::signal(SIGPIPE, SIG_DFL)`. Rust
  sets SIGPIPE to ignore and has no safe API to undo it.
- `web_cloudflare::secret::login_home`: `getpwuid_r` for the trusted
  login home. Neither rustix nor std has a passwd API.
- `cli::priority_dir::rename_exclusive`: only if `renameat_with` cannot
  express `RENAME_EXCL` on macOS.
- `cli::build_stamp`: the `read_volatile`, if the diagnosis shows it
  needs one; otherwise it is replaced.

**Test-only items,** listed by item:
- `signal_boundary` tests;
- `process_boundary`'s `pre_exec` probe;
- `proc_run_tests`' rlimit fixture;
Not on the list: the `html_integration` environment sentinel. Today it
calls `std::env::set_var` inside the multithreaded test harness, and
nothing guarantees that no other thread or C library reads the
environment at the same time. So it is not made sound by listing it. The
sentinel test runs html generation in a **child process** given the
variable with `Command::env`, through an existing in-process entry or a
small test helper binary. It keeps its coverage, the env sentinel never
appearing in the output, with no `set_var`.

Every unsafe block in a residual item carries a non-empty `// SAFETY:`
comment directly above it, stating the invariant it relies on.

### 3. The compiler boundary

- The workspace `Cargo.toml` sets `[workspace.lints.rust]
  unsafe_code = "deny"`, and every crate opts in with `[lints] workspace
  = true`. That covers each crate's lib, bins (`clank`,
  `clank-web-cloudflare`), build scripts, examples and integration-test
  roots.
- `clank-core` and `clank-web` add `#![forbid(unsafe_code)]` at their lib
  roots.
- A residual item carries `#[allow(unsafe_code)]` on itself only, never on
  a whole file or crate.

### 4. The scanner gate, `tests/it/unsafe_boundary.rs`

Built with syn, in the style of `git_boundary` and `process_boundary`:
- **Walks every `.rs` file in every crate** and resolves each item's
  module and item path, including `#[cfg(test)] mod` scopes.
- **Finds** unsafe blocks, `unsafe fn`, `unsafe impl`, `unsafe trait`,
  `unsafe extern` blocks, and the lint opt-outs `allow(unsafe_code)`,
  `expect(unsafe_code)` and `cfg_attr(…, allow(unsafe_code))`.
  Comments and string literals do not count.
- **Requires** each finding to sit inside an allow-listed (file, item
  path). A residual item elsewhere in the same file does not exempt its
  neighbours.
- **Requires** every unsafe block to have a non-empty `SAFETY:` comment.
- **Self-tests on inert source fixtures**, parsed strings that are never
  compiled or run. Each must be rejected or accepted correctly:
  - production unsafe in a file that also holds an approved item, but
    outside it: rejected;
  - an unapproved `allow(unsafe_code)`: rejected;
  - an unapproved `cfg_attr`-wrapped allow: rejected;
  - a missing or empty SAFETY comment: rejected;
  - `unsafe` in a comment or string literal: not counted.

## Behaviour preservation

The existing seams are exercised. No real signal path is changed for a
test, and no mutation touches a kill path.

| Behaviour | Covered by |
|---|---|
| flock: blocking vs non-blocking, contention as "busy", EINTR retry | lease tests, user-config tests, door lock tests |
| poll: a timeout with no events stays a timeout | monitor and source_diagnostic tests |
| waitpid: "not exited" vs "reaped" vs "not our child" | proc_run and source_group tests |
| fd ownership and CLOEXEC across spawn | proc_run tests, incl. `concurrent_forks_never_make_a_run_incomplete` |
| signal-0 probe vs a real signal; PID/PGID guards | signal module seam tests, `signal_boundary` |
| raw-mode setup, Drop/panic restoration, async-signal-safe handler | new focused tests (below) |
| the login-home lookup still comes from passwd | secret tests |

### `cli/term.rs` verification

`term.rs` says "Untested by design" and has no tests today. This plan
adds verification that needs neither a real terminal nor a real signal:
- **A pty pair** (`rustix::pty::openpt`) stands in for the terminal.
  - Entering raw mode on it changes the expected flags.
  - Dropping the guard restores the saved termios exactly.
  - A panic inside the guard's scope restores it too, checked with
    `catch_unwind` on the pty.
- **The handler's work is factored into an async-signal-safe function**
  that takes the saved termios and an fd. The test calls that function
  directly on the pty, not through a signal, and asserts restoration. A
  source-level check in the `unsafe_boundary` gate confirms the handler
  calls only that function and `_exit` or a re-raise.
- **Installation keeps its semantics.** If the implementation moves from
  `signal` to `sigaction`, it preserves today's flags, restart behaviour
  in particular. A test reads the installed disposition back through
  `sigaction(sig, NULL, &old)` and checks it against what `signal`
  installed. No signal is delivered.

## Acceptance

- The workspace lint and the `forbid`s build:
  `cargo clippy --locked --workspace --all-targets -- -D warnings`.
- `unsafe_boundary` passes, and its fixture self-tests reject each
  forbidden case.
- The As-landed note gives before and after counts of unsafe blocks per
  crate, and the residual list with its reasons.
- The suites pass, each run once:
  - `-p clank --lib`;
  - `-p clank --test it`;
  - `-p clank-user-config`;
  - `-p clank-web-cloudflare`;
  - `-p clank-core`;
  - `-p clank-web`.
- Linux-only code (pidfd, `close_range`) is proven by the Linux CI job on
  the PR. Local macOS runs cover the macOS paths. No unavailable platform
  run is a completion gate.

## As landed

- **5bf32c03 / e1c94ce6:**
  - rustix 1.1.5 is a direct dependency of clank, user-config and
    web-cloudflare. It replaced the uid, flock, access, futimens,
    ttyname, read, fcntl, poll, termios, waitpid, kill, socket, exclusive
    rename and rlimit calls.
  - Fds and pids stay typed; `Pgid`/`Pid` still refuse ids of 1 or less.
  - `[workspace.lints.rust] unsafe_code = "deny"` covers every crate and
    target. core and web also `forbid` it.
  - The syn scanner `tests/it/unsafe_boundary.rs` holds the `RESIDUAL`
    allow-list, by item, each entry with its reason.
  - `term.rs` gained pty tests.
  - `set_var` is gone from the tests:
    - the html sentinel re-runs its own test in a child with
      `Command::env`;
    - doctor's tests use the `Ambient` seam.
  - Unsafe block counts at this stage:
    - clank: about 188 → 44, 5 of them test-only;
    - web-cloudflare: 7 → 3;
    - user-config: 2 → 0.
- **Where reality differed from the plan:**
  - `mkfifo` in the test is now a subprocess, since rustix has no Apple
    mknodat.
  - The exclusive rename needed no residual.
  - Linux exit watching is fully safe.
  - `proc_listpgrppids` and `bsd_info` moved into `inspect::platform`.
  - `reap_blocking` refuses a pid of 0 or less with an explicit
    `pid <= 0` check, added in the As-landed fix. `Pid::from_raw`
    rejects only 0 and debug-asserts against negatives, and -1 would
    wait for any child.
- **398282a7:**
  - The saved termios is published with Release and read with Acquire.
  - The handler audit follows every term.rs function it reaches through
    a per-function table, `saved()` included.
  - `signal_process` and `signal_group` dispatch through a private
    `Deliver` seam. Named signals go through rustix. Any other number,
    real-time signals included, goes through the residual `Kernel::raw`
    kill, which matches `libc::kill`.
  - The disposition test runs in a child process.
- **Final counts,** by syn, including unsafe tokens inside macros and
  excluding prose: clank has 43 unsafe expression blocks plus 1 unsafe
  extern block, 4 of the expression blocks test-only; web-cloudflare
  has 3; user-config, core and web have 0.
- **Mutation checks:** scanner checks against real files and inert
  fixtures, the term.rs restore paths, and flock contention. None
  touched a kill or signal path.
- **Not yet proven:** the Linux-only branches were type-checked in a
  scratch crate, but the PR's Linux CI is the real build.
