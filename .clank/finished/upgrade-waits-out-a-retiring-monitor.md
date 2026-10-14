# upgrade-waits-out-a-retiring-monitor
# `clank monitor upgrade` ends each old monitor itself, TERM then KILL on a timeout, before starting its successor

## Why

The first real run of `clank monitor upgrade`, on 2026-10-15, asked 34 old monitors to
stand down. It started a successor in only 11 of the repos:
- in 12 real repos (life, fs-war-room, clank, penlock-experiment, safepay and others)
  it reported `Unestablished { spawned: false, why: "the handshake with
  /tmp/clank-socks/<pid>.sock failed: the monitor closed mid-frame" }`;
- the master started those 12 by hand afterwards.

**The cause.** The successor's start greeted the published monitor, which was the old
one, part-way through standing down. It answered by closing mid-frame, which reads as
`Seen::Unproven`, and the start gave up without waiting.

**The fix.** A new monitor cannot take the repo's lease until the old process is gone.
So the upgrade should not infer from a half-answered handshake whether the old one is
leaving. It already verified each old monitor's process, so it ends that process
itself, with a timeout, and then starts the successor. (The first version of this
plan taught the handshake to read a mid-frame close as "leaving"; review found that
needed per-socket correlation to be safe. Ending the process directly makes it
unnecessary.)

## What to build

### 1. End each asked old monitor, bounded

For every endpoint whose old monitor answered that it is obsolete (`Asked`), the
upgrade ends that process, using the identity it verified: the repo from the
`clank monitor start --repo` command line, and the process's birth token, taken when
it was verified.
1. **Look.** Before each signal, look at the pid against that same birth.
   - Another birth there means ours exited and the pid was reused (`Gone`).
   - Nothing there also means `Gone`.
   - A look that establishes neither is `Unknown`: never counted as gone, and
     nothing is sent on it.
2. **TERM.** Only if the look found ours, and the deadline has not passed after that
   look.
3. **Wait** for the process to exit, for `TERM_GRACE` (12 s) at most, and never past
   the deadline. The grace is longer than a lease-loss drain (10 s) that TERM may
   join, so a cooperative drain is not cut short. Exited: `Exited`.
4. **KILL.** Only if the look still finds ours, and the deadline has not passed
   after that look. Exited after it: `Killed`.
5. **Give up.** Not confirmed gone by the deadline: `StillRunning`. Not lookable at
   the start: `Unverified`, with nothing sent.

**What a KILL does not prove.** A KILL, or a monitor that cannot be looked at,
proves nothing about the producer work its monitor had left. A producer group a
killed monitor leaves behind is reaped by the successor when it takes the source, as
for any dead monitor.

Every asked endpoint is ended, duplicates included. The wait is for the process to
exit, not for the lease to free: a duplicate holds a lease file already replaced.

### 2. Start the successor as an ordinary client would

With an old monitor gone (`Exited`, `Killed` or `Gone`), the successor starts through
the ordinary `attach_or_spawn_until`, under the command's deadline. It waits for the
lease there. No handshake answer is reinterpreted.

**One claim per repo, without blocking a retry.** A repo's claim records a start in
progress, whether a successor was established, and whether another endpoint of the
repo was ended while that start ran.
- A start that created nothing (`Unestablished { spawned: false }`) leaves the repo
  unestablished.
- It is tried again when another of its old monitors is ended during or after it, so
  a published duplicate still leaving does not cost the repo its successor.
- No two starts run at once for one repo.
- Unrelated repos never wait on each other.

### 3. Reported

The report gives each endpoint's `Retirement` beside its outcome, and each repo's
successor from the endpoint that last started it.

## Acceptance

Through an injected retirement (signal, liveness and lease probe as functions), so no
test sends a real signal:
- **No signal past the deadline:** none at entry, none after a look that itself ran
  past it (before TERM or before KILL).
- **An unreadable process is never counted as exited.**
- **A pid reused by another monitor of the same repo** (another birth) is not sent
  KILL, through the look that compares the captured birth.
- **A duplicate still leaving:** with a start that created nothing while the other
  old monitor of the repo was still leaving, the start is tried again once that one
  is ended, and succeeds.
- **A monitor that exits on TERM:** `Exited`, no KILL sent.
- **A monitor that ignores TERM:** KILL at `TERM_GRACE`, then `Killed`.
- **A lease that never frees:** `StillHeld`, within the deadline, and no successor
  start.
- **A process already gone:** `Gone`, with no signal sent.
- **A pid reused before KILL:** the birth check fails, so no KILL is sent.
- **Through `drive`:**
  - an asked endpoint is ended and its repo's successor started once;
  - duplicate asked endpoints of one repo are each ended, with one successor;
  - current and newer monitors are neither signalled nor restarted.
- **No reinterpretation:** `attach_or_spawn_until` has no departing exception, and an
  `Unproven` answer leaves ordinary clients alone, as before.

## As built

- **The verified identity is kept.** `repo_of_monitor` returns `Verified { repo,
  birth }`. `Effects::retire` receives that `Verified`, and `retire` looks at the pid
  with `observe_with` against the same birth: another birth is `Gone`, nothing there
  is `Gone`, unreadable is `Unknown`.
- **`retire_with`** checks the deadline after each look and immediately before TERM
  and before KILL. It never counts `Unknown` as exited. `TERM_GRACE` is 12 s.
  `Retirement` gains `Unverified`.
- **Claims:** a repo's claim is a `Claim { starting, last, retry, established }`.
  A start that created nothing is retried if another endpoint of the repo was ended
  while it ran; otherwise a later endpoint may claim and start it. The report reads
  the successor from `starting`, else `last`.
- **Removed:** 6eaac6b's `departing` exception (`attach_or_spawn_until` is as it was
  before this plan).
- **Tests:**
  - TERM then KILL on the timeout (exits on TERM; killed after the grace; already
    gone, with nothing sent; bounded when still running; no KILL past the deadline);
  - a pid reused by another monitor of the same repo is not killed, and an
    unreadable look is `Unknown`;
  - no signal past the deadline: at entry, after a slow look before TERM, and after
    a slow look before KILL;
  - an unreadable process is not exited;
  - the driver ends every asked monitor, with one successor per repo;
  - a start that created nothing is retried when a duplicate goes.
- **Mutation checks:** five checks on these paths, all caught. Every signal in the
  tests goes to a fake.
