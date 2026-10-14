# a-monitor-runs-the-installed-build
# A monitor stands down when the installed clank is a different build from the one it runs; one command moves today's monitors onto such builds

## Why

An install does not reach running monitors. A client replaces a monitor only when the
wire protocol differs (`PROTO`, 12 since 6 Oct). A change that keeps the protocol runs
the old code for as long as the old monitor lives:

- **Safepay never got nudges.** Its monitor (pid 7978) started on 12 Oct. `nudges-are-on`
  was installed on 13 Oct without a protocol change. Safepay's codex hit "model at
  capacity" that evening, and the monitor could record the stall but never nudge.
- **The lease fix reaches no one yet.** `a-monitor-keeps-its-lease` changed no protocol
  either, so every monitor running today keeps the old lease code.
- **Duplicates are stuck too.** Duplicate monitors left by replaced lease files
  (fs-war-room, clankstack, protonmailmcp, anxiety-attack, `fswt/genuine-check`) are
  reached by no client at all, since the descriptor names only one monitor per repo.

There are three distinct authorities here, and this plan keeps them apart:
- **The running image:** which build is executing. Only that image can say it.
- **The installation:** which build is at the path clank is installed at. Only reading
  that file can say it.
- **Legacy processes:** monitors from before this plan, which can do neither. They need
  a one-time migration.

## What to build

### 1. Build stamp: provenance from the running image

**The stamp is a content hash of everything a build is made from**, not a random id.
`crates/cli/build.rs` computes SHA-256, truncated to 128 bits, over:
- **every regular file** under the workspace's `crates/` tree, by relative path and
  content. That covers every workspace crate linked into clank (`cli`, `core`, `web`,
  `web-cloudflare`) and the assets they embed with `include_str!`/`include_bytes!`
  (`crates/web/assets/**`, and `web-cloudflare`'s). Any `target/` directory and hidden
  directories are excluded;
- **the workspace `Cargo.toml` and `Cargo.lock`**, which pin every non-workspace
  dependency;
- **the build configuration**: `PROFILE`, `TARGET`, `OPT_LEVEL`, `DEBUG`, the enabled
  `CARGO_FEATURE_*` set, the `CARGO_CFG_*` set Cargo provides (which carries
  `debug_assertions` and target cfgs), the `rustc -vV` output of `RUSTC`, and the
  values of `CARGO_ENCODED_RUSTFLAGS` and `RUSTFLAGS`, which are the effective compiler
  flags;
- **the profile configuration**, wherever this plan can see it:
  - the profiles in the hashed manifests;
  - the workspace's `.cargo/config.toml` (and `.cargo/config`), if present;
  - every `CARGO_PROFILE_*` variable in the build script's environment.

  `overflow-checks` in particular is not reflected in any `CARGO_CFG_*` variable. It is
  covered only through these routes.

**Outside the contract, stated plainly:**
- profile configuration supplied by a `~/.cargo/config.toml`, a `config.toml` above the
  workspace, or `cargo --config`;
- a `CARGO_PROFILE_*` variable whose name is not among those declared for rerun (below).

Builds differing only there may share a stamp, though their behaviour can differ (for
example overflow checks on or off). The release is built with `cargo install --path
crates/cli --locked` and no such overrides, which is the configuration this contract
supports.

**Unambiguous framing.** Each hashed item is written as a tag byte (file, manifest,
config), then a length-prefixed name (the relative path, or the variable's name), then a
length-prefixed value (the content). Environment items are sorted by name, and files by
relative path. No two different input sets can serialise to the same bytes.

**It reruns when any of those change.** The build script declares:
- `rerun-if-changed` on `crates/` (Cargo scans a directory recursively), the workspace
  manifest and the lockfile;
- `rerun-if-changed` on the workspace `.cargo/config.toml` and `.cargo/config`;
- `rerun-if-env-changed` on every environment value it hashes that Cargo does not
  already rerun for:
  - `RUSTC`, `RUSTFLAGS`, `CARGO_ENCODED_RUSTFLAGS`;
  - `CARGO_PROFILE_<P>_<KEY>` for `P` in `DEV`, `RELEASE`, `TEST` and `BENCH`, and for
    each documented profile key (`OPT_LEVEL`, `DEBUG`, `DEBUG_ASSERTIONS`,
    `OVERFLOW_CHECKS`, `LTO`, `PANIC`, `INCREMENTAL`, `CODEGEN_UNITS`, `RPATH`,
    `STRIP`, `SPLIT_DEBUGINFO`).

A new crate or asset under `crates/` is covered without editing a list, which is what
keeps the closure maintainable.

**What equal stamps mean.** Equal stamps mean the same workspace sources and assets,
locked dependencies, target, features, cfgs, compiler, compiler flags, and profile
configuration from the routes covered above. Different inputs give equal stamps only by a 128-bit
hash collision.

**What it does not cover** (stated, not claimed): inputs read by a dependency's build
script from outside the workspace, such as system libraries or environment variables
other than those above. A clank built against a different system library with
otherwise identical inputs counts as the same build.

**Embedding and reading:**
- The binary embeds the stamp as `#[used] static CLANK_BUILD_STAMP: [u8; N]`: a fixed
  16-byte marker, then the 16-byte hash.
- The running image's stamp is that static in memory. That is provenance from the
  executing image, not from a later `stat` of a path. It closes the case where build A
  is exec'd, the path is replaced by B before A first looks, and A records B as itself.
- The installed stamp is found by scanning the installation file (section 2) for the
  marker; exactly one well-formed occurrence is required. The scanner searches using
  that same static's marker bytes, never a second copy of them, so the scanner itself
  adds no occurrence. The release artifact keeps the static (`#[used]`, plus a read of
  it at startup), and a test checks the built test binary contains exactly one
  occurrence.

**Outcomes:**
- `Current`: the installed stamp equals the running stamp.
- `Replaced`: a well-formed installed stamp that differs. This is proof.
- `Unavailable`: the path is missing or unreadable, or has no marker, more than one, or a
  malformed one. This is never treated as replacement.

**Caching keeps the evidence apart from the displayed state.**
- **A completed scan is cached by signature, whatever it found.** A completed scan is a
  file read in full, with an equal signature before and after the read. Its result is
  cached by that signature, even when the result is `Unavailable` (no marker, more than
  one, or a malformed one). An unchanged, readable, invalid candidate is therefore read
  once, not on every wake.
- **The signature** is device, inode, length, modification time, change time and mode.
  A changed signature always invalidates the cached scan.
- **An unestablished observation is never cached.** That covers a missing or unreadable
  file, and a read that failed or saw the signature change. It is retried at the next
  existing wake point, so a read that failed and then succeeds, with an unchanged
  signature, is found.
- **The undetectable rewrite.** A rewrite that preserves the whole signature, including
  `ctime`, is not detectable from a cached result. The kernel moves `ctime` on any
  write, so that requires deliberately resetting it, which no install does.
- Diagnostics are logged when the displayed state or its reason changes, not on every
  wake.
- No timer or idle worker is added. Retries ride the wake points that already exist.

### 2. The installation path: one rendezvous

**The rendezvous is the spelling clank was invoked by, not where `current_exe()`
resolves.** On some platforms `current_exe()` resolves an invocation symlink to its
target. A client launched through `clank -> versions/A/clank` would then pass
`versions/A/clank`, and retargeting the launcher to B would never be seen.

The rendezvous is chosen, in order, by `rendezvous(argv0, cwd, PATH, current_exe)`:
1. `CLANK_INSTALL_PATH`, if set: an explicit override, used as given.
2. `argv[0]`, preserved as spelled:
   - if it contains `/`, joined to the launch's `cwd`;
   - otherwise, looked up in `PATH` as `exec` would.

   Symlinks are not resolved. The result must name an existing file.
3. Otherwise `current_exe()`. Status marks a monitor using this fallback, since it may
   be a resolved target that a retargeted launcher would not move.

**One path for everything.** The spawner passes the chosen path as `--installed-at` and
execs that same path. The monitor uses it for the startup comparison and every later
look, following symlinks with `stat` at look time. A client spawning a successor
likewise execs its own rendezvous path, so successor and comparison agree. A rendezvous
that no longer names a file is `Unavailable` (1), never a guess. A monitor started by
hand without the flag uses `rendezvous` on its own invocation.

**Retirement.** A monitor whose installation is `Replaced` retires through the lease-loss
path: fence, the producer drain on one deadline, the completion contract, then release.
It logs both stamps, and does not spawn a successor.

**Legacy clients, stated plainly.** An already-running legacy client spawns with its own
`current_exe()`, which is not promised to name the installed file:
- on macOS it names the path it was exec'd by;
- on Linux a replaced binary's `/proc/self/exe` does not resolve to the new file.

A legacy (proto 12) client meeting a proto-13 monitor classifies it as `Seen::Theirs`
and works alone, as today; that rule is unchanged. Agents' parked waits are fresh
processes on each turn (the stop hook runs the installed `clank`), so they become
new-build clients at their next turn. A long-lived legacy TUI works alone until
restarted.

### 3. One owner transition: build and lease

- **Two looks, one coordinator.** At each wake, `Tenure::may_act` makes two looks: the
  build look (1) and the lease look. They are combined in one coordinator, with the
  build look first.
- **A stale build never repairs.** A `Replaced` build retires without repairing or
  republishing the lease. A stale build never advertises itself as owner again.
- **The watchdog.** It may also `stat` the installation path. As with the lease, it only
  wakes the loop.
- **At startup**, the build look runs before the lease is taken and the descriptor is
  published. A monitor that starts already `Replaced` exits without publishing, so the
  replacement-between-exec-and-first-observation case never owns the repo.
- **Build retirement is dominant.** When the build look finds `Replaced`, the wake is
  handled as retiring. That includes a wake carrying a newer client's obsolete greeting
  (`Greeted::StandDown`): its ordinary short stand-down is not taken, and the loop
  enters the build retirement with the producer drain instead.

### 4. The legacy migration: an explicit, bounded command

Self-detection covers every later install, but it needs monitors that have it.
Migrating today's proto-12 monitors is a one-time operation. It does not happen just by
installing.

- **The trigger.** This plan bumps `PROTO` to 13. A new client contacting a repo's
  proto-12 monitor asks it to stand down through the existing obsolete path
  (`Greeting::Obsolete`). That happens at that repo's first contact by a new-build
  client: an agent's next turn, or a new TUI. A repo with no activity is not migrated by
  this alone.
- **The command: `clank monitor upgrade`,** run once by the user after installing,
  reaches every repo with a live monitor. It is bounded:
  1. **Enumerate** the monitor sockets in the vetted socket namespaces this invocation
     uses (`socket_dirs`: its `XDG_RUNTIME_DIR` candidate and the two `/tmp`
     fallbacks), and the published descriptors in the runtime directory.
  2. **Find each socket's repo.** For each socket, find the repo its monitor serves:
     - from the descriptor naming that socket;
     - otherwise, for duplicates the descriptor no longer names, from the process named
       by the socket's pid. It must be this uid's process, its command line must be
       `clank monitor start --repo <path>` (read as `proc_identity::inspect` reads
       command lines), and its birth must predate the socket file.

     A socket whose repo cannot be established is reported and skipped.
  3. **Ask.** Greet each with a proto-13 observer hello for that repo. An older monitor
     answers `Obsolete` and stands down. A newer or equal one is left alone.
  4. **Start a successor** for each repo whose reachable monitor was asked, through the
     ordinary client attach path (wait for the lease, start, attach as an observer, then
     detach). Each repo then has a new-build monitor even if nobody is active there.

  **Bounds:** at most 8 handshakes at once, one overall deadline (30 s) covering every
  handshake and start, and nothing outlasting the command. **Report:** asked, declined,
  silent, skipped (with the reason), or started, for each socket and repo.
- **It never removes** a socket, lock or descriptor because a handshake failed.
- **Scope, stated plainly.**
  - A monitor bound under a different `XDG_RUNTIME_DIR` from the one the command runs
    with is outside the namespaces it enumerates. The command says which namespaces it
    swept; run it again with that `XDG_RUNTIME_DIR` to reach those.
  - The command is not run automatically.
  - There is no idle scan, and no sweep on monitor startup.

### 5. Diagnostics: published, and shown by status

**Routing and observation are kept apart.**
- **Routing:** the shared descriptor says which monitor serves the repo. It keeps its
  owner protocol, with its documented external-change window, and carries no changing
  build state. Only the running stamp and the rendezvous path are added, both fixed for
  the process's life and written at owner publication.
- **Observation:** the build state is a separate record per monitor process, at
  `<runtime dir>/<repo hash>.build-<pid>-<birth>.json`. It holds:
  - the running stamp;
  - the rendezvous path and where it came from (override, invocation or fallback);
  - the installation state: `Current`, `Replaced`, or `Unavailable` with its reason.

  Each monitor writes only its own record, by rename, when its state changes; that
  includes `Replaced` during retirement. It never touches the routing descriptor to do
  so. An old monitor's last write therefore lands under its own identity and cannot
  change where clients connect, whatever the lease has done meanwhile.
- **Joining them.** On a status lookup, `clank status` reads the routing descriptor, then
  the observation record named by that descriptor's pid and the process's birth token.
  - A record whose birth token does not match the live process is ignored, as left by
    a dead monitor whose pid was reused.
  - Records are removed by their monitor on an orderly exit. One left by a monitor that
    died is never read, because no descriptor names it. The next new-build monitor
    removes leftovers for its own repo hash whose process is gone, once at startup.
  - No timer, watcher or recurring scan is added.

**What `clank status` prints:** the monitor's pid and build, and one of:
- `current`;
- `replaced, retiring`;
- `install path unavailable: <why>`;
- `unknown` for a legacy proto-12 descriptor without the field;

plus `(fallback path)` when the rendezvous came from `current_exe()`.

## Acceptance

Tests through seams, with fixture files standing in for binaries. No clank binary is
spawned and there is no live monitor.

- **Stamp inputs.**
  - With the same sources, changing only one of these, as Cargo really supplies them,
    gives a different stamp:
    - `CARGO_ENCODED_RUSTFLAGS`;
    - `OPT_LEVEL`;
    - `CARGO_CFG_DEBUG_ASSERTIONS` being present or absent;
    - `CARGO_PROFILE_DEV_OVERFLOW_CHECKS`;
    - a workspace `.cargo/config.toml` profile setting.
  - The framing distinguishes a moved boundary between name and content.
  - The input enumeration, shared by `build.rs` and the tests through `include!`, covers
    the real workspace's `crates/web/assets/**` and every workspace crate's `src`.
  - Over a fixture tree, changing only an embedded asset, or only a path dependency's
    source, changes the stamp.
  - Excluded `target/` and hidden directories do not.
  - The build configuration inputs are part of the hash.
- **Stamp reading.**
  - The test binary contains exactly one marker occurrence, which the scanner finds and
    which equals the static.
  - Fixture installations give `Current`, `Replaced` and `Unavailable`. The cases are:
    a missing path; unreadable; no marker; two markers; a malformed id.
  - Read counts:
    - An unchanged, readable, valid candidate is scanned once.
    - An unchanged, readable candidate with no marker, or a malformed one, is also
      scanned once.
    - A changed signature causes a new scan.
    - A failed read is retried at the next look.
  - A failed read followed by a readable file with an unchanged signature is rescanned
    and found.
  - Rename and symlink retargeting are rescanned.
  - A file replaced before the first look is `Replaced` (startup provenance).
- **Rendezvous.**
  - With a resolver whose `current_exe` returns a version-specific target, and `argv[0]`
    found through `PATH` as an alias symlink, the alias is chosen.
  - Retargeting that alias makes the monitor's look `Replaced`.
  - The override wins.
  - The fallback is marked.
  - A vanished path is `Unavailable`.
  - The successor spawn uses the same rendezvous.
- **One transition.**
  - `Replaced` retires through the drain and completion contract, and never repairs the
    lease or advertises.
  - Simultaneous build replacement and lease loss retire once and never advertise.
  - Routing is safe from a late observation. The old monitor passes its last owner
    look. Then the lease path is replaced and a successor takes it and publishes its
    descriptor. Then the old monitor writes its `Replaced` observation. The routing
    descriptor still names the successor, and the old diagnostic is stored only under
    the old process identity.
  - `Replaced` is visible in status through the observation record while the old
    monitor is still the routed one.
  - A record whose birth token mismatches is not shown.
  - An obsolete greeting in the same wake as `Replaced` leads to the drain retirement.
  - `Unavailable` keeps ownership and is shown.
  - Startup with `Replaced` neither publishes nor takes ownership.
- **Protocol.** `PROTO` is 13. Fixed proto-12 hello and answer bytes check:
  - a proto-13 observer hello to a proto-12 greeting is `Obsolete`;
  - a proto-12 client meeting a proto-13 answer is still `Seen::Theirs`.
- **Upgrade command.**
  - With several silent peers, the overall deadline holds.
  - Concurrency is capped at 8.
  - A same-repo older peer is asked, and a successor started.
  - Foreign, newer and equal peers are left alone.
  - A duplicate is resolved from its process's command line.
  - An unidentifiable socket is skipped and reported.
  - Nothing is removed on failure.
- **Diagnostics.**
  - The descriptor's fixed build fields and the observation record round-trip.
  - Status renders `current`, `replaced, retiring`, `unavailable`, legacy `unknown` and
    the fallback mark.
  - `Unavailable` notices are logged once per change of reason.
- **No new steady-state timer or watcher.**

## Release (operational, not a review gate)

- **Install at a quiet moment, then run `clank monitor upgrade`.** It retires every
  proto-12 monitor it reaches with the old short escalation (TERM, then KILL after
  200 ms). Life's Signal source must have no send in flight, and life's agent should be
  told first.
- **Check the report.** Every repo should show a started new-build monitor, and every
  duplicate should be asked. Then verify exactly one monitor per repo, on the new build,
  holding the file at its lease path.

## Out of scope

- **Agents with no recorded harness version** (launched before `nudges-are-on`). They
  still cannot be nudged until relaunched with `clank agent start`.
- **Moving the runtime directory out of `/tmp`.**

## As built

- **Stamp** (`crates/cli/build.rs`, `crates/cli/build_stamp_inputs.rs`):
  - `build_stamp_inputs.rs` is shared by `build.rs` and the tests through `include!`.
  - `stamp_of` hashes SHA-256, truncated to 128 bits, over the inputs the plan
    names, with tag, name and value framing.
  - The build script declares `rerun-if-changed` on `crates/`, the manifests and the
    workspace cargo configs, and `rerun-if-env-changed` on `RUSTC`, `RUSTFLAGS`,
    `CARGO_ENCODED_RUSTFLAGS` and every `CARGO_PROFILE_{DEV,RELEASE,TEST,BENCH}_<KEY>`.
  - It writes `STAMP` to `OUT_DIR`.
  - `sha2` is now also a build-dependency, at the version already locked.
- **`crate::build_stamp`:**
  - `CLANK_BUILD_STAMP` is a `#[used]` static: marker, then stamp, then an end tag.
  - `running()` reads it with a volatile read.
  - `find()` scans for the marker using the static's own bytes.
  - `Installation::look` caches a completed scan by signature (device, inode, length,
    mtime, ctime, mode, equal across the read) whatever it found, and never caches an
    unestablished one.
  - `rendezvous_of` picks the override, else the invoked spelling without resolving
    symlinks, else `current_exe` marked as the fallback.
- **Monitor** (`monitor.rs`):
  - `spawn_monitor` execs the rendezvous path through `monitor_command`, passing
    `--installed-at` and `--installed-from` (hidden flags on `monitor start`).
  - `start` looks at the installation before taking the lease, and returns without
    owning or publishing if it is already `Replaced`.
  - `Tenure` carries a `Built`. `settle` looks at the build first: `Replaced` gives
    `Settled::Stale` and marks the tenure retiring, before any lease repair or
    republication. `may_act` is true only for `Owner` or `Retaken`.
  - A newer client's stand-down greeting while not the owner is skipped, so the loop's
    retirement with the drain runs instead of the short stand-down.
  - `PROTO` is 13. The descriptor carries a fixed `build` (running stamp, rendezvous
    and source).
  - The changing state goes to a per-process `Observation` record,
    `<hash>.build-<pid>.json`, holding the process's birth token. It is written by
    rename on each change of state and removed on an orderly exit. Records whose
    process is gone are removed at the next monitor's startup.
  - `clank status` prints a `monitor:` line from `build_line`. That joins the routing
    descriptor to the record of the pid it names, believed only if the birth token
    still holds. The line shows current, replaced and retiring, unavailable, legacy
    unknown, or not yet observed, plus the fallback mark.
  - `Seen::Newer` now separates a newer monitor from one that refused this client on
    its merits, so neither the upgrade nor anything else reads that distinction from
    error text.
- **`clank monitor upgrade`** (`monitor_upgrade.rs`): one driver, `drive`, under one
  absolute deadline (30 s), with its world-facing effects injected through `Effects`:
  `repo_of`, `probe` and `start`.
  - **Discovery.** Only socket namespaces that `trusted_dir` vets as this uid's are
    read; any other namespace is reported with its reason. Only entries that are
    sockets owned by this uid and named `<pid>.sock` are taken.
  - **The repo comes from the process.** A socket's repo comes from its process,
    never from a descriptor: a process whose observed uid (`proc_identity::uid_of`,
    read between two birth checks) is this user's, identified by its birth token
    across the read (`proc_identity::inspect`), whose command line is exactly
    `clank monitor start ... --repo <path>`. The program must be `clank`, followed
    by the subcommand. Descriptors are not consulted at all, so a stale or reused one
    cannot authorise anything.
  - **Probing.** Each socket is greeted for its repo. Only an older monitor's exact
    obsolete answer (`obsolete_why`, the same words since proto 3) is `Asked`.
    `Seen::OlderPeer` now carries `obsolete`. Any other older refusal is `Refused`
    and leads to nothing.
  - **Starting successors.** A successor is started for each repo with an `Asked`
    monitor, through `attach_or_spawn_until`. That carries the command's own
    absolute deadline through the whole handoff: each handshake gets only what is
    left when it starts, and the deadline is checked again immediately before the
    spawn. Time spent before then, in setup or a slow lease probe, is never given
    back. `attach_or_spawn_by` is now a wrapper over it, with unchanged defaults for
    ordinary clients.
  - **What a start reports.** `spawned` is recorded only when the spawn returned a
    process, not when one was attempted. The start also reports the moment of
    creation to the driver. The outcome is one of:
    - `Spawned`;
    - `Reused`;
    - `Unestablished { spawned, why }`;
    - `InFlight { spawned: Created::Yes | Created::Unknown }`: still running at the
      cutoff, with whether a process had been created by then;
    - `TimedOut`: never begun, so nothing was started.
  - **Completion, not just admission.**
    - Discovery runs on a thread of its own. If it has not reported by the deadline,
      nothing is greeted or started (`Report::discovered` is false).
    - Each endpoint is one operation, run on at most 8 detached threads: verify its
      repo, greet it, and, if it answered obsolete, claim its repo and start the
      successor.
      - The first endpoint to hear a repo's monitor say it is obsolete claims that
        repo. A duplicate endpoint does not start it twice.
      - No operation waits for another, so a stalled endpoint holds back no healthy
        repo.
      - No step begins past the deadline.
    - At the deadline the report is read from each operation's recorded progress. A
      start that was begun is never reported as never begun. Anything an operation
      does afterwards changes nothing, and begins no further step.
    - Unfinished threads are not joined. Each ends with its own deadline-bounded
      operation, or with the command's process, so only a deliberately spawned
      monitor outlives the command.
    - The command awaits the driver over a oneshot, never the blocking pool.
    - It removes nothing.
- **`PATH` lookup.** The invocation lookup is executable-aware (`access(X_OK)`), as
  exec's would be, and keeps the unresolved path.
- **Deviations:**
  - The observation record is named by pid, with the birth token inside, not in the
    name. Status believes it only when that token still holds.
  - The upgrade does not enumerate descriptors. The verified process is the only
    source of a socket's repo; a socket whose process cannot be verified is skipped.
  - The watchdog does not look at the installation path. The loop's wake points do,
    and the fold heartbeat keeps those frequent.
- **Tests:**
  - `build_stamp`:
    - the input closure covers every linked crate and embedded asset;
    - each input alone moves the stamp (asset, dependency source, lockfile, workspace
      cargo config, encoded rustflags, `OPT_LEVEL`, `CARGO_CFG_DEBUG_ASSERTIONS`,
      `CARGO_PROFILE_DEV_OVERFLOW_CHECKS`, rustc), while output and hidden
      directories do not;
    - the framing keeps names and values apart;
    - the test binary carries its stamp once;
    - current, replaced and unavailable cases, including missing, unreadable, no
      marker, two markers and malformed;
    - read counts;
    - the invoked alias is chosen and its retargeting seen.
  - `monitor::build_tests`:
    - stale retires and never repairs or advertises;
    - an unavailable installation keeps ownership;
    - a late observation cannot take routing back from a successor;
    - status joins, and trusts only a living birth;
    - the spawn command uses the rendezvous;
    - a proto-12 client is declined.
  - `monitor_upgrade`, the whole discovery-to-successor operation through `drive`:
    - a successor only for an obsolete monitor, none for refused, current, newer,
      unproven or unreachable ones;
    - an answer that arrives after the deadline counts for nothing, and starts
      nothing;
    - a lookup still running at the deadline is left unfinished, the report comes
      at the deadline, and the late lookup greets nothing;
    - nothing is discovered or enumerated past the deadline;
    - an unvetted namespace is reported and unread, and a non-socket entry ignored;
    - at most 8 probes at once, measured as simultaneous activity, with slow
      verification ending by the deadline;
    - a full blocking pool cannot hold the command;
    - the exact legacy obsolete answer is `Asked` and a wrong-repo refusal is
      `Refused`;
    - only a clank monitor command line names a repo;
    - the real start's facts:
      - nothing past the deadline;
      - a failed spawn is not `spawned`;
      - a spawn that never answers is spawned but unestablished;
      - a compatible monitor already serving is `Reused`;
    - a start whose deadline is spent before the spawn, by a slow lease probe,
      spawns nothing;
    - a stalled endpoint does not hold back a healthy repo, whose successor starts
      once although two endpoints answered for it, while the stalled one times out
      and its late answer starts nothing;
    - a start running at the cutoff reports `InFlight`, with `Created::Yes` if it had
      created a process and `Created::Unknown` if not;
    - a process of another uid is not verified.
  - `build_stamp`: the `PATH` lookup skips a non-executable earlier candidate.
- **Not reachable from tests** (live process paths), each decision being tested:
  - `start`'s stale-at-startup return;
  - the loop's stand-down guard;
  - `spawn_monitor` choosing the rendezvous;
  - a real spawned monitor coming up as `Spawned`: the `Reused` and unestablished
    cases are tested through injected spawn and connect.
