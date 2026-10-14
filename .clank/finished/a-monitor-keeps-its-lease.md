# a-monitor-keeps-its-lease
# A monitor whose lease file is gone or replaced re-takes it or stands down, so two monitors of one repo do not persist

## Why

`~/src/life` had two monitors (pids 59940 from 6 Oct and 12850 from 11 Oct), and so did
this repo (74420 from 6 Oct and 49716 from 12 Oct). On 14 Oct, life's `assistent` signal
source moved from one to the other (the old producer stopped, a new one started), cutting
off two in-flight Signal sends.

The cause is visible on disk. Life's lease file
`/private/tmp/clank-run-501/211ce3226eac9be825cc01efb46fa6d6.lock` was replaced on 11 Oct
08:01, its inode going from 250896861 to 259241798:
- 59940 still holds its `flock` on the deleted inode;
- 12850 opened the new file and locked it.

Each holds a lease, so each believes it is the repo's only monitor, and they contend for
the repo's source ingest locks.

The lease is the monitor's whole claim to the repo (`acquire_lease_in`: "the lease IS the
liveness"). But a lease on an unlinked file proves nothing: nothing reachable by path is
locked, so the next client finds the path free and starts a second monitor. Nothing in
clank's production code was found removing it; whatever did (an outside cleanup, a manual
removal) is not under clank's control. So the monitor must not assume its file is still
there.

## What to build

### The guarantee, stated honestly

A lease's file can be removed or replaced from outside at any moment. So a successor can
lock a new inode before the old monitor next looks, and an overlap cannot be ruled out by
any check. This plan gives detection and convergence:
- a monitor that sees its lease is no longer the reachable one stops acting as owner at
  once;
- it then either re-establishes verified ownership or retires.

The remaining window, between an outside change and the next check, is documented, not
claimed away. Per-source ownership (each source's ingest/group lock) stays in force
throughout, so even inside that window two monitors cannot both run a consumer for one
source. The second to arrive is refused by the source lock, as today.

### 1. Verified acquisition

A lease is accepted only when the locked fd is the file at the canonical path. After
`flock` succeeds, the monitor compares the fd's device and inode (`fstat`) with the
path's (`stat`): at first acquisition, at re-acquisition, and before publishing a
descriptor after a repair. The outcomes are distinct:
- **held and verified:** owner;
- **busy:** another holder has the path; this monitor is not the owner;
- **the path replaced between open and lock** (identities differ): retried once, then
  treated as unverifiable;
- **an I/O or namespace error:** unverifiable.

Unverifiable never authorises continued ownership or a fallback lease path: it is
handled as loss (3).

### 2. One coordinator

The fold loop is the only authority over the lease: only it validates the lease
authoritatively, re-takes it, publishes a descriptor or gives it up.
- **Checks at existing wake points.** It runs the cheap identity check (`fstat` and
  `stat`) at each fold and each loop wake. There is no new timer and no per-waiter
  watcher.
- **The watchdog only observes.** On its tick it may `stat` the path against a shared
  snapshot of the held identity, read-only. If they differ it sets a shared flag. It
  never opens, locks, re-takes or publishes; the loop validates and acts at its next
  wake.
- **Owner actions are fenced once loss is known.** From the moment the loop sees loss
  (or the flag), it suspends:
  - admitting new clients;
  - dispatching recovery attempts;
  - starting producers.

  It keeps serving what is already in flight, until ownership is re-established or
  retirement begins.
- **If the loop is stalled,** the watchdog's existing forced exit
  (`source_group::exit`) remains the fallback for a stalled loop. It is a different,
  documented outcome: producers are cut off, not drained. It is logged as a forced
  retirement, never as a graceful one.

### 3. Loss: re-take or retire

- **The path is missing.** The loop creates and locks it, then verifies as in (1). If
  verified, the monitor is the owner again: it lifts the fence, republishes its
  descriptor (only after verification), and logs the repair.
- **Another verified holder, or the path still unverifiable after a retry.** The loop
  retires through the async `stand_down` path that owns admissions, writers and task
  settlement, with the producer drain below.

### 4. One bounded retirement timeline

A lease-loss retirement is one lifecycle operation with one deadline, not a series of
waits:
1. **Fence.** Stop admitting clients and handing producers new work.
2. **Fan out.** Send every producer group TERM at once.
3. **Drain, on one shared deadline** (the drain grace, 10 s, from the moment TERM is
   sent). Each source keeps its ingest/group ownership until its producer is confirmed
   ended, so no successor's consumer can start on top of it.
4. **Escalate** any group still running at the deadline to KILL, and confirm it ended.
5. **Settle** writers and tasks within the existing settle budget, then release the
   lease and exit.

The existing `stand_down` budgets (`RETIRE_BUDGET` 3 s, `SETTLE_BUDGET` 500 ms) are
extended for this kind of retirement to cover the drain deadline, so a normal drain is
never cut short by the 3-second budget.

**The watchdog knows about it.** Retirement records its deadline where the watchdog can
read it. While a bounded retirement is under way, the watchdog judges that deadline, plus
a margin, instead of the fold's age. Intentionally stopped folding is not a stall, and a
monitor whose last fold was already old when retirement began is not forced out during
a normal drain. A loop that is genuinely unresponsive (not retiring, or retiring past
its deadline and margin) still gets the documented forced fallback.

**Each source's outcome is recorded accurately:**
- `ended within grace`: it exited after TERM, before the deadline. A producer that
  ignores TERM but happens to exit is not claimed to have drained; only producers that
  drain on TERM (as life's does) finish their accepted work this way;
- `killed at grace`: KILL at the deadline;
- `forced`: the stalled-loop fallback.

**Limitation:** a producer still running at the deadline is cut off. Ordinary reconcile
restarts keep today's policy.

### 5. Diagnostics

Every detection logs:
- the path;
- the held identity (device and inode);
- the path's identity, or that it is absent or the error;
- the time;
- what was done: re-taken, retired, or forced.

### 6. Releasing it (operational, not a review gate)

Every monitor running now is an old binary without these checks: the detached
duplicates and the current holders alike. After `cargo install`, for this repo and for
life:
1. **Identify each running monitor**, and which one holds the reachable lease, by
   comparing the lease path's inode with each pid's open lease fd. Confirm each pid is
   the recorded process (its birth token) before signalling it.
2. **Retire them all.**
   - The detached duplicates first, then the current holder: the holder is also the old
     build, so retiring only the duplicates would leave no monitor on the new one.
   - Use each build's supported retirement. For these builds that is SIGTERM, which
     goes through `source_group::retire_on_termination`: groups are retired with the
     old short escalation (TERM, then KILL after 200 ms), and the process exits as the
     signal's default.
   - This is not the new coordinator's drain. In-flight producer work and socket writes
     may be cut off. So retire life's monitors at a quiet moment, with no Signal send in
     flight, and say so to life's agent.
3. **Start one monitor on the installed build.** Let the next client, such as a parked
   wait or the TUI, start it as today. Then verify that exactly one monitor runs for
   the repo, on the new binary, and that its open lease fd is the inode at the lease
   path.

## Acceptance

Tests of the production code through the lease and loop seams, with no live monitor:

- **Acquisition.**
  - Verified when unchanged.
  - Busy when another holder has the path.
  - The path replaced between open and lock: retried, then unverifiable.
  - An unreadable namespace: unverifiable, never owned.
- **Loss.**
  - The path removed while held: re-taken, verified, then republished. A second lease
    attempt then fails.
  - Replaced and held by another: retirement.
  - Unchanged: only the identity check runs.
- **Coordination.**
  - The watchdog observing loss only sets the flag, and the loop acts on it.
  - Concurrent loop and watchdog observations produce one action.
  - New admissions and dispatches are fenced while loss is pending.
  - Publication happens only after verified ownership.
- **Retirement timeline.**
  - Several fixture producers are sent TERM together and share one deadline, not
    serial waits.
  - One that keeps an accepted request running past 200 ms (and past the old 3 s
    budget) and ends within the grace is recorded `ended within grace`.
  - One that ignores TERM is KILLed at the deadline and recorded `killed at grace`.
  - Each source's ingest/group lock is held until its producer has ended.
- **Watchdog.**
  - A retirement that begins when the last fold is already old completes its drain
    without being forced out: the watchdog judges the retirement deadline.
  - A loop that stops answering while not retiring, or past its deadline and margin,
    is forced and recorded `forced`.
- **Diagnostics** carry both identities (or absent/error), the path and the time.
- **No new timer or watcher.**

## Out of scope

Moving the runtime directory out of `/tmp`, unless investigation shows the files are
being removed by a system tmp cleaner; then it is a follow-up.

## As built

- **Verified acquisition** (`monitor.rs`, `acquire_lease_with`): after `flock`, the fd's
  device and inode must equal the path's. Otherwise the result is `LeaseError::Busy`
  (another holder) or `LeaseError::Unverifiable` (an I/O or namespace error, or the path
  replaced between open and lock twice running). A test hook (`between`) runs between
  open and lock. `MonitorLease::check()` reports `Found::{Held, Absent, Other(id),
  Unreadable}`.
- **Receive, look, act** (`start`):
  - The loop's `select!` only receives. Each wake becomes a `Wake` value carrying its
    payload.
  - Then `Tenure::may_act` looks at the lease. It re-takes and verifies a lost lease,
    then republishes, or decides to retire.
  - Only then is the wake acted on. Every owner effect is gated on that look:
    - recovery dispatch and authorisation;
    - accepting and admitting clients;
    - source restarts;
    - interrupt notices;
    - timer production;
    - the fold's reconcile, source publication, recovery and identification.
  - The fold is looked at again after its await.
  - While retiring, a payload stays in custody: a client is kept for the retirement's
    presentation, an emission is kept and served, and an authorisation is dropped, which
    the asker reads as a veto. At the top of the loop, a retiring tenure enters
    retirement.
  - The watchdog only `stat`s the path against the held identity and wakes the loop
    (`Notify`). It never decides; the wake's own look does.
- **Retirement, one timeline** (`stand_down` with a `Drain`, budget `LEASE_RETIREMENT` =
  `DRAIN_GRACE` 10 s + `RETIRE_BUDGET`):
  - At entry, before greetings settle, `Owners::drain` closes producer admission, sends
    every group TERM and fixes one absolute cutoff.
  - From then on, every retirement entry point waits for the group's work to end or for
    the cutoff, whichever comes first, and keeps the group and its ingest lease until
    it is confirmed ended. The entry points are the drain's own `retire_all`,
    `Owned::retire`/drop, and deferred retries.
  - Work has ended when the group has no members but its anchor
    (`signal::group_members`). A root exiting is not enough.
  - The drain then escalates straight to KILL (`retire_after` with zero, since TERM was
    sent at entry).
  - The drain's retirement runs on a thread of its own, not the blocking pool.
  - `Draining::finish(by)` waits on a notification rung by that retirement finishing
    and by each `Reading` guard dropping. The wait is bounded by the stand-down's
    absolute deadline and starts nothing after it.
  - It returns a `DrainEnd`: `Complete`, `StillRetiring`, `Unconfirmed(n)` or
    `StillReading(n)`. Only `Complete` permits `ReadyToRelease`. Anything else is
    `RequiresProcessExit`, whatever greetings and hosted tasks did, so the documented
    held-lease exit (which cuts the drain off and retires what remains) is the release
    path.
- **Outcomes:** each source is recorded `ended within grace`, `killed at grace`, or `not
  confirmed ended`, from what its retirement saw. `exit()` brings a drain's cutoff
  forward (`cut_off`), so a forced exit cuts producers off at once.
- **The real reader** (`wait.rs`): the host takes the registry (`run_command_source_with`
  takes `owners`) and holds a `Reading` guard while it reads.
  - When its root exits during a drain, it keeps reading until the cutoff rather than
    100 ms, so a descendant's later output is logged and sent.
  - Retirement runs off the runtime (`spawn_blocking`), since during a drain it blocks
    until the cutoff.
- **Watchdog:**
  - The loop records its retirement deadline (`begin_retiring`: the full budget plus
    settle).
  - Within that deadline plus a margin (`WATCHDOG_TICK * WATCHDOG_MISSES`), the fold's
    age is not judged.
  - Past it, or on a stall while not retiring, the monitor is forced out and logged
    `forced`.
- **Diagnostics:** `Detection` logs the path, the held identity, what is at the path, an
  RFC 3339 time, and what was done.
- **Tests:**
  - `monitor::lease_tests`:
    - acquisition: verified, busy, replaced every time, replaced once;
    - an unchanged lease only looks;
    - removed, then re-taken before republication;
    - held by another, so retired and fenced;
    - the watchdog only wakes, and the loop acts once;
    - a wake after an unseen replacement is fenced;
    - retirement judged by its deadline;
    - one whole deadline: an unending greeting plus a TERM-ignoring producer finish
      within budget and settle, and the producer is dead by then;
    - a lease retirement through the real source host, with a root that exits on TERM
      and a descendant working 3 s, whose output reaches the durable log before the
      host is cancelled.
    - each incomplete case forbids release within the deadline: a drain unfinished at
      the deadline, an unconfirmed retirement, a reader outstanding at the deadline,
      and a full blocking pool.
  - `source_group`:
    - one shared grace, with work past 3 s and a stubborn producer killed at the
      cutoff;
    - a descendant the root left is waited for;
    - work still running at the cutoff is reported killed;
    - a drain waits for its readers.
  - `wait`: a drain reads what the descendant produces through the real host (sent and
    logged).
  - `signal`: group members are listed.
- **Not reachable from tests:** `start`'s `select!` needs a live monitor. Each decision
  it calls (`may_act`, `stand_down`, the drain) is tested.
