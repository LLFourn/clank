# a-proto-bump-cannot-retire-a-running-monitor
# A PROTO bump cannot retire a running monitor

## Why

`the-monitor-hosts-an-authors-sources` took `PROTO` 3 → 4, correctly: the hosting
capability is part of what a client agrees to, so a client needing hosted sources must not
be served by a monitor that has none.

Installing it exposed the other half. Measured on this machine immediately after
`cargo install`: five monitors running, every one started by the PREVIOUS binary — this
repo's (pid 423, up since Oct 1 19:00), plus `anxiety-attack`, `clankstack` and two
frostsnap worktrees. Counted again a few hours later: seven, now a MIX of proto 3 and
proto 4, because clients kept spawning replacements as the day went on. That mixture is
what forced the capability onto proto 5 rather than 4 — see below. Each holds its repo's `MonitorLease` and has no reason to exit: the
watchdog stands down on a stalled FOLD, and their folds are healthy. So the dedup the last
two plans bought is off in every one of those repos, indefinitely, and there is no
`clank monitor stop`.

**The draft of this plan was wrong about the mechanism, and the correction changes the
design** (codex on 4dc8637). It said the monitor declines the new client. It does not: the
client refuses the DESCRIPTOR and never connects —

```
monitor.rs:1053   if !usable(&d, repo, PROTO) { return Attached::Alone(…) }
```

— which is before `connect`, before `Hello`, before anything the holder could observe. The
holder learns nothing, and "a monitor that stands down after declining a newer client"
cannot be built on a notification that is never sent.

What each park actually costs today, from `attach_or_spawn_by`:

1. `attach_by` refuses the descriptor locally and returns `Alone`. No socket is touched.
2. `attach_or_spawn_by` spawns a monitor anyway (`monitor.rs:1200`) — a real fork/exec of
   the release binary — because `Alone` is `Alone` whatever its reason.
3. That monitor loses the lease race to the legacy holder and exits. `supervise` reaps it,
   so nothing accumulates.
4. The client then retries the same descriptor check `SHARE_ATTEMPTS - 1` = 3 more times,
   sleeping `SPAWN_SETTLE / SHARE_ATTEMPTS` = 150ms each, and falls back to a local wait.

So the behaviour is safe — nothing is lost, the fallback is the tested path — and it is
**one doomed process spawn and ~450ms per park, forever**, plus the watcher-per-waiter cost
that was the point of the last two plans.

## The design, after it was rebuilt much smaller

**The holder ends ITSELF.** That is the whole mechanism.

A monitor already declines a client whose `PROTO` it does not share. The one addition is
which side of the mismatch it is on. Declining a client that is NEWER means this process is
what an install replaced — so it answers the decline, stands down, and exits. The client
reads the decline and its ordinary connect-or-spawn starts a current one. Declining a client
that is OLDER means the client is the stale thing: the monitor keeps the repo and the client
does its own local wait, exactly as it does today.

### Why this replaces what came before

The first design had a SUCCESSOR end the holder, and everything elaborate followed from
that one choice. A stranger ending a process has to prove which process it is, so it needed
peer credentials off the socket, a birth token, and an incarnation-bound delivery
(`pidfd_send_signal`, `proc_signal_with_audittoken`) because a pid can be reused between any
check and any signal. It has to prove the process serves THIS repo, because a version
decline establishes capability and not repo. It needs an authority model, because a human
stop and an automatic replacement are not the same claim. It needs a frozen cross-version
request frame, because the ask must cross a version boundary the body cannot.

None of that exists when the holder acts on itself. It knows its own repo. There is no
identity to establish about the process it already is. No signal crosses a process boundary,
so there is nothing to bind. No permission is negotiated, because nothing else is entitled
to an opinion. Deleted with it: `StandDown` and its frozen tags, `Ask`, `Retirement`,
`STAND_DOWN_SINCE`, `Proof`, `prove_holder`, `Evicted`, `evict`, `Superseded`, `supersede`,
`clank monitor stop`, and the peer-credential reader.

### What it costs

**A newer client ends a monitor that is currently serving older ones.** They fall back to
local waits, which is the tested path, and it is the right direction: the newer binary is
the installed one and the older clients are parked waits from before the install.

**It does not retire the monitors running right now.** Proto 3 and 4 cannot learn a clause,
and nothing can add one to a running process. That is a ONE-TIME cost paid by hand — kill
them and let the next park start current ones — rather than permanent machinery. Every
`PROTO` after this one costs nothing.

### What is kept

The orderly shutdown, which self-exit needs exactly as much as supersession did: stop
admissions, cancel the greetings that produce them, claim what was admitted during the
cutoff, drain the source buffer, present once to the complete client set, flush the writers
with truthful outcomes, observe cancellation, and release the lease only on
`ReadyToRelease` — or exit with it held. Also kept: `published`, which reads the descriptor
version-first so an older monitor is a holder rather than an absence.


## Acceptance

- **A monitor that declines a NEWER client stands down and exits.** Driven, and the answer
  reaches the client before the process goes rather than arriving as an EOF to be guessed
  at.
- **A monitor that declines an OLDER client keeps the repo.** The stale party is the
  client, and it does its own local wait. Driven in the same test as the above, because
  getting the direction backwards is the only way this mechanism can be dangerous.
- **Only a VERSION mismatch ends it.** A client declined for its repo or for not being on
  the roster is not evidence about the monitor at all, and must leave it running —
  otherwise any stranger ends the repo's owner by asking wrongly. Driven.
- **The client then gets a current monitor.** Connect-or-spawn already spawns and retries
  for the lease; what this needs is for a spawn to still happen after the holder has gone,
  rather than once, immediately, against a lease it cannot yet take.
- **The stand-down is the orderly one.** A monitor leaving this way runs the same
  transition as any other: nothing it accepted is dropped silently, and the lease is
  released only when it may be.
- **No `PROTO` bump after this one needs a plan.** The rule names no version — it is a
  comparison the holder makes about itself.


## Also here: the fold stamp nobody read

Found by the user noticing `.clank/monitor.json` rewriting itself during normal work. It
was republished on every `FOLD_TICK` — every 5 seconds per repo, forever — to bump a
counter, and following the readers found there were none:

- `attach_by` is the ONLY production reader of the descriptor, and it uses `repo`, `proto`
  and `socket`.
- `Welcome::Ok`'s copy: both readers match `{ .. }` or read `proto` alone.
- `Note::Items`'s copy: the client matches `{ items, .. }`.
- `Note::Beat`'s copy: `forward_beats` does `Ok(_) => { last = now; wake() }` and times out
  on `last.elapsed()` — ARRIVAL, never the payload.
- The watchdog deliberately refuses the published copy and reads the loop's own atomic,
  because the filesystem "would add a way for this to be wrong without adding anything it
  can learn".

The stated purpose — let a client tell a WEDGED monitor from a quiet one — was already
redundant with the message existing. A beat is only sent after a fold COMPLETED (a failed
one serves nobody), so its arrival proves the advance, and "a beat arrived but the stamp
did not move" is unrepresentable. `beat_overdue`'s own doc had said as much since
`adb5453`: *"Comparing tick numbers would add nothing a client can act on."* The field was
kept anyway, "for diagnostics", and then written to disk 17k times a day.

So `Fold` is gone from `Descriptor`, `Welcome::Ok`, `Note::Items` and `Note::Beat`, which
becomes a payloadless proof of life. What survives is a private `u64` in the monitor,
compared by the watchdog inside one process — the only comparison anything ever did. The
descriptor is published ONCE, at startup, and its remaining fields never change during a
monitor's life.

Done here rather than queued because this plan already takes `PROTO` 4 → 5: removing wire
fields costs nothing extra inside a bump that is happening anyway, and would cost its own
bump later.

## Remaining implementation

Small, because the design is. The whole of it lands in ONE commit.

- The decline clause and the stand-down it triggers.
- A spawn that happens after the holder goes, not only before.
- Deleting the superseded machinery and its tests.
- The tests named in Acceptance.
- Killing the proto 3 and 4 monitors on this machine by hand after installing, which is
  the one-time bootstrap and is not code.


## Out of scope
- Changing the decline itself. Falling back to a local wait is correct and tested
  (`a_version_mismatch_declines_rather_than_fails`,
  `an_older_frame_is_declined_by_its_version`); this is about who clears the stale holder,
  not about what a declined client does.
- Loosening `admits`. Exact equality is what makes a capability change a version change.
- Migrating a holder's in-flight state to its successor. A fresh monitor refolds from disk
  and the WAL is the durable part.
- The four OTHER repos' stale monitors on this machine. They are other repos' live
  workflow; when those restart is the human's call, and a `clank monitor stop` existing is
  all this plan owes them.
