# a-monitor-takes-its-producers-with-it
# A monitor takes its producers with it

## Why

Reported by Life's master on 2026-10-05, and caused by a clank action: after an
install, the master retired Life's monitor with SIGTERM (~19:10) so a fresh one
would start on the new binary. Its command-source producers were not stopped.
They were reparented to launchd and kept running. Life's Signal producer went on
receiving into a pipe nobody read, and **a user message was lost at 19:19**.
Signal's receive pulls from the server, so an orphan does not duplicate an
event, it CONSUMES it. For any source whose receive is destructive, an orphan is
data loss.

The same evening, five more orphaned Life voice producers (`anx events --agent
life`, parent 1, 5h13m–5h42m old) were found, left by monitors replaced hours
earlier. They were stopped by hand. Their processes are gone, so their cause
can be reconstructed but not proven, and this plan does not gate on proving it.

## What the code establishes

Checked against the source, not taken from either report:

- **A monitor installs no signal handling at all.** `monitor.rs` has no
  SIGTERM/SIGINT handler, so SIGTERM takes the default disposition and exits
  without unwinding. Producers are spawned with `process_group(0)` and
  `kill_on_drop`, and `GroupKill` ends its group on drop, but no destructor runs.
  This fully explains the 19:10 orphan.
- **Crash window 1 — activation before record.** `wait.rs` calls `cmd.spawn()`
  and only then `record_producer_or_end` writes `{key}.producer`. In between, the
  real command is already running. "Read no stdout before recording" does not
  help: the CHILD can pull a remote message on its own. A SIGKILL in that window
  leaves a live consumer with no recovery evidence.
- **Crash window 2 — record removed before the group ends.** When the leader
  exits, `clear_producer` deletes the record while `_group` is still in scope.
  Its `Drop` kills the group only afterwards. A SIGKILL between the two leaves
  descendants alive with no record.
- **Start-time identity depends on the caller's environment.**
  `same_incarnation` compares `ps -o lstart` as an opaque string, and `look()`
  runs `ps` without pinning `TZ` or the locale. Measured on this machine: the same
  process reads `08:43:52` under `TZ=UTC` and `19:43:52` under
  `TZ=Australia/Sydney`. A successor whose environment differs from the
  recorder's therefore sees a LIVE producer as not-the-same. `standing` maps
  that to `Theirs`, which is documented as "ours ENDED", so the record is treated
  as spent and a second consumer starts beside the first. The conflation is the
  defect: a mismatch is taken as proof of reuse, when it can also be the same
  process rendered differently.
- **Concurrent hosting is already prevented.** `run_command_source` holds an
  `IngestLease` (an exclusive flock on the same directory and key,
  `github/log.rs`) across `run_leased_command_source`. A monitor and a
  fallen-back wait using the same source storage cannot run together. The
  earlier hypothesis that they overwrite each other's record is therefore
  WITHDRAWN, unless evidence shows a lease, key or directory mismatch between
  two hosts. The existing contention and takeover test stays as the baseline.
- **A leaderless group already blocks a rival consumer.** An occupied group with
  no leader is `Unidentified` and returns `Retired::Deferred`. That means no
  signal AND no replacement, because nothing proves those members are ours.
  That guard stays as it is.

**Reconstructed, not proven, for the five historical orphans.** They were
leaders (`pid == pgid`), and their successors DID start new producers. So they
were not classed `Unidentified`, which would have deferred. That fits a live
producer classed `Theirs` by an environment-dependent start time, or crash
window 2. Neither can be confirmed now, so both are fixed rather than one being
chosen.

## The contract

**One owner per source, from acquisition through CONFIRMED group cleanup.**

1. **Acquire** the existing `IngestLease` for the source's directory and key.
2. **Establish recoverable ownership before anything can consume** — by an
   ANCHOR, not by gating the command itself. The source's group is led by a
   clank-owned anchor process that consumes nothing. The real command is
   spawned only later, INTO the anchor's group. The order is fixed:
   a. Spawn the anchor as group leader, with an activation pipe from the host
      and an acknowledgement pipe back to it. The anchor installs its
      TERM-ignore before doing anything else.
   b. Write the record: the anchor's group id and its start identity.
   c. ARM: the host writes exactly the activation token. The anchor validates
      it and replies with a one-shot acknowledgement. The host reads that
      acknowledgement, bounded by a deadline but read once, never polled.
   d. Only after reading the EXACT acknowledgement does the host spawn the real
      command into the anchor's group.

   A successful write of the token is not arming. It proves nothing about
   whether the anchor accepted it, is alive, or has installed its TERM-ignore —
   and the anchor, not the host, decides whether the token was right. So the
   host waits for the anchor to say so. If the acknowledgement does not arrive —
   the channel closes, the anchor exits, it rejects the token, or the deadline
   passes — the attempt ends with NO producer spawned. The host then retires
   the attempt through the normal path: the group is empty or ends, and the
   record is cleared.

   The anchor is FAIL-CLOSED until armed. If the pipe reaches EOF because the
   host died, or delivers anything other than the exact token (malformed,
   incomplete, early close), the anchor exits and nothing more is started. Once
   armed it no longer reads the pipe, and stays until clank ends the group. The
   host's write end is close-on-exec and is never inherited by the real command
   or any other child, so nothing but the host can keep the channel alive. All
   of this is driven by pipe closure, not polling, and not by a parent-death
   mechanism any producer has to implement.

   So each crash window has a defined outcome:
   - **Before the record (a→b):** no record exists, and none is needed. Only the
     unarmed anchor is running, it consumes nothing, and the host's death gives
     it EOF, so it exits without help from any successor.
   - **After the record, before arming (b→c):** the record exists and the anchor
     exits on EOF. The successor finds the leader gone and the group empty
     (`Gone`), spends the record, and nothing was consumed.
   - **Arming, before the acknowledgement is read (during c):** the record
     exists and the command has not started. If the anchor is armed, the
     successor finds it `Ours` and retires it. If it rejected the token or saw
     EOF, it exited, and the successor finds it `Gone`.
   - **Armed and acknowledged, before the command (c→d):** the record exists, the
     group is just the anchor, and nothing was consumed. The successor finds it
     `Ours` and retires it.
   - **Running (after d):** the record exists, the group is identifiable through
     its anchor, and the successor retires it.

3. **Keep the group identifiable for its whole life, including cleanup.** The
   anchor is the identity, so it must outlive every other member. It ignores
   TERM, so a group-wide TERM reaches the command and its descendants while the
   anchor stays through the grace period. Only KILL ends it, and KILL ends the
   whole group in one call. The real command is spawned by the host, not by the
   anchor, so it does not inherit the anchor's ignored TERM; it runs with
   default dispositions. A command whose own leader exits while a descendant
   runs on, or a descendant that ignores TERM, therefore never leaves a
   leaderless group from a clank-started source. If the host dies DURING the
   grace period, the armed anchor is still there and the successor can still
   identify and retire the group. `Unidentified` then arises only from records
   written by earlier binaries, where the existing safe deferral stands: no
   signal, no replacement, and the reason reported.

4. **Retire as one bounded operation.** TERM the group (the anchor ignores
   it), give it a short grace, KILL the group including the anchor, and verify
   the group empty. This runs once, not as
   recurring monitoring. The record is removed ONLY after the group is confirmed
   empty, and the lease is released only after that. Requesting cleanup is not
   finishing it.
5. **Replace only on proof.** A successor starts its own consumer only after the
   previous recorded group is proved ended (`Gone`), or proved to be a different
   incarnation by an identity that cannot vary with the environment.

**Every exit uses that one retirement.** The SIGTERM and SIGINT handlers this
plan adds, the obsolete stand-down, error returns, and the watchdog's
process-exit path, as applicable. A parked WAIT hosting sources follows the same
lifecycle as a monitor; there is no second, weaker one.

**Start identity is made environment-independent, and VERSIONED.** New
records carry a version and a start identity that cannot vary with the caller —
read as a number rather than a rendered string, or rendered with `TZ` and the
locale pinned on both the recording and the checking side.

**Legacy records stay safe without pretending to normalise.** A record written by
an earlier binary is unversioned, and its `lstart` string was rendered under
whatever `TZ` and locale that process had. That is unknown and cannot be
recovered. The existing comparison and `Pgid` guard are kept exactly as they
are: the live leader's start, rendered as today, compared with the recorded
string. A mismatch is NOT proof of PID reuse, so the record defers — no signal,
no rival consumer, and an actionable message naming the record. A failure to
match never counts as evidence that ours ended.

What it must NOT do is search other candidate environments for a match. That was
proposed and is rejected, because a guessed timezone is not the recorder's
timezone, and searching widens the dangerous matches rather than recovering
lost provenance. Concretely: a legacy record written in Sydney for an 08:45 UTC
start reads `19:45`. If the pid is reused by a process starting at 19:45 UTC that
same day, rendering THAT process in UTC produces the identical string — a
stranger, matched (codex on fd20615). Only independent evidence of the original
identity may resolve an ambiguous legacy record.

## Acceptance

- A monitor stopped with SIGTERM, or SIGINT, leaves no producer group behind.
  Driven with a real child group.
- A monitor that dies by SIGKILL has its producers reaped by the next host of
  that source, before the next host starts its own. Driven with a real orphaned
  group and its real record.
- Life's shape, end to end: replace a monitor once with SIGTERM and once with
  SIGKILL, and assert no producer from the old one survives either.
- **Crash window 1, before the record:** a deterministic fixture that kills the
  host between spawning the anchor and writing its record. No record exists,
  nothing was consumed, and the anchor exits on EOF with no successor involved.
- **Crash window 1, after the record and before activation:** the host is killed
  after the record is written but before arming, and separately after arming but
  before the command starts. The record exists, nothing was consumed, and the
  successor recovers correctly in both.
- **The activation channel fails closed, through the actual host driver:** a
  rejected, malformed, incomplete or early-closed activation starts no command.
  So does an anchor that exits or never answers before the deadline. A DELAYED
  acknowledgement still starts no command until it arrives. A child spawned by
  the host after the anchor does NOT hold the activation pipe's write end, so it
  cannot keep an unarmed anchor alive. These are driven through the host's spawn
  path, not only the anchor's token parser.
- **Crash window 2:** a deterministic fixture that kills the host while the
  record is being cleaned up. The record outlives the group's last member, never
  the other way round, and the successor reaps anything left.
- **A leader that exits while a descendant runs on**, from a clank-started
  source: the group stays identifiable through its anchor and is reaped. From a
  legacy record with no clank leader: deferred, with no signal sent and no rival
  consumer started.
- **A descendant that ignores TERM, with the host dying during the grace
  period:** the anchor survives the TERM and the host's death. The successor
  identifies the group and KILLs it, and nothing is left.
- **A legacy record under a different environment:** an unversioned record whose
  `lstart` was rendered under another `TZ` defers when it does not match — no
  signal, no rival consumer — and is never classified as PID reuse.
- **The guessed-environment collision cannot authorise a signal:** a legacy
  Sydney record and a reused pid whose UTC rendering reproduces it exactly. This
  is a PURE fixture behind the signal seam, so no real process is endangered.
  Nothing is signalled and the record defers.
- **Environment-independent identity:** a NEW producer recorded under one `TZ`
  and checked under another is recognised as the same incarnation. A genuinely
  reused pid is still recognised as not ours.
- Nothing signals a group that is not provably the one clank started.
  `same_incarnation` and `Pgid` are kept, not weakened to make a test pass.

## Watch out

- Never mutate a destructive guard in a mutation run. Assert signal safety by
  construction (`Pgid`), and give tests a delivery seam so a mutant cannot reach
  a real `kill`. This repo has killed a developer's session that way.
- Tests spawn plain `sh` children in their own groups and reap them; they never
  spawn the clank binary or leak zellij sessions.
- The anchor stays blocked and lightweight while armed: it waits, it does not
  loop or poll.
- The anchor must not change what the producer sees: same command, same
  environment, same `CLANK_SOURCE_STATE_DIR`, same stdin and stdout contract, and
  default signal dispositions — the anchor's ignored TERM must not leak into it.
- No polling. Retirement is bounded and runs once; nothing watches a group on a
  timer.
- Life's Signal producer now exits on parent death (life 6bf3443) as a backstop.
  clank must not rely on producers doing that, because most will not.

## Out of scope

- Recovering the lost 19:19 message.
- Proving which reconstructed cause produced the five historical orphans. Both
  causes are fixed.
