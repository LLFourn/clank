# a-repo-is-watched-once-not-once-per-waiter
# clank — `clank monitor start`: one watcher per repo, and a registry waiters join

## Why

Measured on this machine before a reboot cleared it:

- **98 parked `clank wait` processes** for 71 distinct `(repo, author)` pairs. One agent
  (`ark-design`) had 8. A frostsnap worktree once reached 30, and when its gate finally
  moved all 30 discharged inside 548ms — the notification burst that started this.
- **~170 filesystem watchers.** Each parked wait attaches its own
  `RepoStateWatcher::attach` (`wait.rs:238`) on `.clank/` + the gitdir and **refolds the
  whole repo state** on every debounced change. So does every `clank tui`
  (`status.rs:1421`, plus a second diff watcher at 2482) — 35 of those. A single commit in
  a repo with 8 parks causes 8 identical folds.
- **36 abandoned codex hooks**, reparented to PID 1, each still holding a live
  `clank wait`. Their codex stopped reading, so their stdout reaches nobody.

The accumulation is a LIFETIME MISMATCH, not a cleanup failure
(`stash/the-waker-is-per-turn-but-the-session-is-not`): a park is armed at every turn end
and cancelled only by its own exit, so a turn begun any other way — the human types,
interrupts, a background task finishes — orphans the park that was watching and the next
turn end arms another. One per conversational exchange, unbounded. A quiet repo also mints
one per `DeadlineAction` heartbeat.

But the duplication that COSTS is the watching, not the processes. A park must exist per
session because **the hook's exit is the delivery** (below). A watcher need not.

## What `clank monitor start` is

One process per repo path — so per worktree, ~35 here rather than 98 — that owns:

1. **The fold.** One `RepoStateWatcher` and one repo-state fold, shared by every reader.
2. **The external sources — the CONFIG ones only.** `wait_events` spawn children and
   stream lines; today that happens per park, so a repo with 8 parks spawns its command
   sources 8 times. Once per repo instead, which also removes the
   SIGKILL-leaks-grandchildren hazard noted in `clank produce`'s review.

   **That hazard claim was wrong and is withdrawn.** Sharing producers does not remove the
   SIGKILL leak: `GroupKill` and `kill_on_drop` run on ordinary teardown, not when the
   owning process is killed outright (codex on ca7a4c2). A SIGKILLed monitor still orphans
   its producers — and now they are the ONLY copy, so the leak is concentrated rather than
   cured. Reaping the previous owner's orphaned producers is therefore a takeover
   requirement in its own right, before the new owner starts its own.

   But they are not uniformly shareable, and the draft glossed this.
   `resolve_wait_event_sources(&repo, &author, &args.events)` merges two kinds, tagged
   `SourceOrigin::Config` and `SourceOrigin::Cli` (`wait.rs:617,622`): the agent config's
   sources belong to an AUTHOR and may be shared by that author's waiters; a `--event`
   source belongs to ONE INVOCATION and cannot be shared at all. A monitor that pooled
   both would broadcast one author's events to another, or silently drop the CLI ones —
   the failure codex named. So config sources move to the monitor, keyed by author;
   `--event` sources stay with the client that declared them, which keeps its own channel
   for them.

   Routing stays per author and per ROLE: `drop_items_not_for` is re-derived every wake
   because a demotion mid-wait must drop what the old watch queued, so it is applied
   against the author's CURRENT role, never a role captured at registration.

   **NOT DONE, and deliberately deferred — see "What landed" below.** A wait with any
   source of its own (config, `--event`, or master's derived draft watch) keeps watching
   for itself. Sharing is refused for it rather than attempted, so nothing is lost; what
   is not yet gained is the dedup for repos that configure sources.
3. **A registry.** Waiters join with `(author, role, session, tool)` and are told what
   that author is owed, when it is owed.
4. **Fold subscriptions.** Read-only, any number: the TUI and `clank status --watch`
   consume the computed fold instead of each running two watchers of their own.

## Where the socket lives

**Not under the repo**, which the draft assumed. `sun_path` is 104 bytes on macOS and
clank nests worktrees, each adding `.clank/worktrees/<name>/`: measured on this machine,
the longest `<repo>/.clank/monitor.sock` is **97 bytes**, seven short of unusable, and a
worktree name ten characters longer than `nonce-aware-coin-selection` is all it takes.

So the arrangement is Claude Code's, which met this first:

- **A short, flat socket directory**, tried in order — `$XDG_RUNTIME_DIR/clank-socks`,
  `/tmp/clank-socks`, then the private `/tmp/clank-socks-<uid>` as the fallback. That
  mirrors its documented behaviour: *"refuses to create the socket in a directory it
  can't accept, for example one that another user owns, and uses a private per-user
  directory … instead."* `/tmp` is world-writable, so a candidate is accepted only if we
  own it and it is not group- or world-writable.
- **Discovery by DESCRIPTOR, not by a derived name.** `.clank/monitor.json` carries the
  proto, the pid, the canonical repo, the socket path and the last completed fold — the
  way `~/.claude/sessions/<pid>.json` carries `messagingSocketPath` and `status`. A plain
  file in the repo is bound by PATH_MAX, not `sun_path`, so depth costs nothing.

  An earlier draft hashed the repo path into the socket filename. A descriptor is
  strictly better: no collision to reason about, and a client can read the version and
  the fold's staleness before it connects.
- The repo is **verified on connect anyway**, because a descriptor can outlive the repo
  it describes — a moved or recreated repo would otherwise have its clients folding
  another repo's state.

**Version skew needs no compatibility path.** Old clients do not matter here; restarting
everything is acceptable. The version gate stays anyway, and small: the envelope's
`proto` is read before the body, so a skew DECLINES rather than misreads. That is not
about migration — it is because a body decoded under the wrong version can succeed with
wrong semantics, serde reading a missing `Option` as `None` and serving a bound client as
an unbound one.

## Ownership, after the intro review

The draft said **connect-or-bind**: the first waiter becomes the monitor. That is wrong,
and codex named why (on a206455) — *"give monitor ownership a lifetime independent of an
individual delivery client"*. A delivery client EXITS when it delivers, so ownership would
churn on every wake, tearing down and re-attaching the watcher and the source set each
time. The thing whose duplication this plan exists to remove would be rebuilt per
delivery.

So: **connect-or-spawn.** `clank monitor start` is a standalone process with its own
lifetime. A client that finds no monitor spawns one and then connects; it never becomes
one. That keeps self-healing without a supervisor, and ownership outlives any delivery.

**Takeover is fenced by a lease, not by a race.** An flock beside the socket, the way
`StatusLease`, `IngestLease` and `lock_stash` already do it — the kernel serializes it, so
two live owners with two watchers and two source sets are unrepresentable rather than
merely unlikely.

**But a lease does not evict a wedged holder** (codex on ca7a4c2), and unlinking the lock
path to force it would destroy the invariant: two processes holding flocks on two inodes
is two owners. So eviction is the monitor's own job. A watchdog THREAD — not the fold loop,
which is the thing that may be stuck — compares the fold stamp against its tick and exits
the process when it stalls, freeing the lease for the next client to take honestly. The
client-side detection below is then the second line: it stops a client waiting forever on
a monitor whose watchdog also failed, and it recovers by spawning once the lease frees.
Recovery must be exercised with CONCURRENT clients, not one.

**The heartbeat must prove the FOLD is progressing, not that a socket task is awake.**
Also codex: *"heartbeat freshness must demonstrate progress of the fold/projection loop,
rather than merely activity in an independent socket task."* A stamp emitted by the
accept loop proves only that the process breathes — exactly the wedged monitor it is
supposed to detect. So the stamp is written BY the fold loop on each of its ticks
(the loop already has a heartbeat tick for the poll-mode path), and carries the last
completed fold. A quiet repo produces no refolds, so absence of change is not evidence of
a stall; the tick is, which is why the stamp must come from the loop and not from the
work.

## What this plan does NOT change

Delivery. That is the scope discipline that keeps this landable, and it is forced by how
each tool is woken — established by reading the adapters, not assumed:

| tool | `LoopPolicy` | how a continuation reaches it |
| --- | --- | --- |
| Claude | `TaskCompletionWake` | **exit 2 with the wake on stderr**, hook detached by `asyncRewake` — *"the only mode that wakes on exit 2"* |
| Codex | `InHookWait` | stdout JSON **while the hook still runs**; codex is blocked reading it |
| OpenCode | `InHookWait` | stdout text, the plugin injects it |
| Grok | `Passive` | *"nothing we print can continue the agent"* |

So for Claude the park must stay alive to spend its exit; for codex and opencode the hook
is synchronous and its output IS the continuation. A monitor that tried to deliver would
have nothing to deliver THROUGH. Clients keep their own exit codes and streams, byte for
byte. `clank wait` changes only where its items come from.

This is also why "replace the park with a socket" is not a standalone move: the park does
two jobs, and a delivery channel only removes one of them. Something must still watch.

## Questions

1. **When does a monitor stop?** Ownership is settled above (connect-or-spawn, leased),
   but lifetime is not: does an idle monitor with no clients linger for the repo, or time
   out and let the next client respawn it? A lingering one keeps the fold warm and the
   watcher attached; a timing-out one costs a cold start per idle period. `clank open
   zellij` starting one up front is a third option that makes the common case warm.
2. **The protocol and its version skew.** Long-lived processes plus `cargo install`
   mid-flight means an old monitor WILL meet a new client — it happened twice in one day
   while this was being designed. Handshake a version; on mismatch the client declines and
   does its own local wait. Decide whether a superseded monitor drains and stands down.
3. **Per-agent config under a per-repo owner.** `items_for(&repo, &author, …)` derives
   from a shared fold, so the expensive half is shared — but `wait_events`, role and
   work policy are per agent, and the monitor must reload them the way a park does today
   (`wait-reloads-config-per-refold`).
4. **`--peek` stays local.** It is documented SIDE-EFFECT-FREE and one-shot — true for
   the first time now that the lifecycle ingest is gone — so it must never connect and
   never bind.
5. **Does the monitor subsume `arm-only-if-none`?** With a registry, a second park for one
   session is visible and refusable at the source, which is strictly better than scanning
   `ps`. If this lands first, that stash may be retired rather than popped.
6. **What about the 36 abandoned codex hooks?** The monitor makes each one cheap (a
   blocked socket reader instead of a watcher and a fold) but nothing kills them. They
   need their own fix — `POLLHUP` on stdout, since the existing owner sentinel takes a
   spare stdin pipe and a hook's stdin carries its payload. Separate work, named here so
   it is not mistaken for solved.

## What landed

Everything above except source hosting, which is deferred with a reason rather than
half-done.

**In:** one watcher and one fold per repo (`clank monitor start`, leased, published,
self-evicting); `clank wait` shares it; connect-or-spawn so a client that finds no monitor
starts one whose lifetime outlives any delivery; a fold watchdog that stands down when the
FOLD stops rather than when the socket does; `clank status --watch` and the TUI as
read-only fold observers, keeping only their own diff watcher; producer records so a
takeover ends a SIGKILLed owner's orphaned children before starting its own; and a sweep
of the sockets dead monitors leave in `/tmp/clank-socks`.

**Limit on the takeover reap, stated rather than papered over.** A recorded producer is
identified by its group LEADER's incarnation, which is the only part of a group that can
be written down. If the leader is still there, the group is identified and the whole group
is ended — including descendants that outlive the leader during the escalation. That last
step is allowed only on evidence that is still good: before the kill the leader is looked
at AGAIN (a different live one cancels it outright) and the gap since the identification
is MEASURED on a monotonic clock, not assumed to be the sleep. `sleep` reports a minimum
and the world supplies the maximum — a stopped or starved process resumes with its sleep
"over" and any amount of elapsed time behind it, by which point the id may name a group it
never owned — so an identification that has aged out fails closed. If the leader is
ALREADY gone when a takeover arrives and
the group still has members, those members cannot be identified at all: the kernel only
promises an id is not reused while its group has members, which says nothing about the gap
since the record was written, so they may equally be a stranger's group that was re-issued
the number and then lost its own leader. That case defers with a message naming the record,
rather than either signalling processes that may not be ours or replacing a producer that
may still be running. Closing it needs durable group-level evidence, and the obvious
carrier is not available: `ps -E` cannot read another process's environment on current
macOS, so a token in the child's environment — the one piece of state that survives exec
and is inherited by descendants — cannot be read back.

**Out: the monitor does not host config `wait_events`.** Not an omission of wiring. The
sources' durability lives in a ~4.2k-line WAL (`command_source.rs`,
`github/supervisor.rs`, `github/log.rs`) whose central invariant is "a row is
re-presented until it is acked", with the PRESENTER being the process that holds the
ingest lease. Moving hosting to the monitor makes the monitor the permanent presenter,
which re-opens a settled question — who re-presents the unacked backlog to a client that
arrives later, and against whose `presented` set — inside a subsystem with several
hard-won correctness citations. Getting that wrong produces a lost or duplicated wake,
which is the one outcome this plan may not produce.

It also buys nothing here yet: no agent in this repo configures `wait_events`, so every
wait here already qualifies to share. Master's derived DRAFT watch is the one case that does
occur, and it is refused rather than mishandled. Follow-up draft:
`.clank/drafts/the-monitor-hosts-an-authors-sources.md`.

## Acceptance

- `clank monitor start` runs one watcher and one fold per repo, and a parked
  `clank wait` attaches no watcher of its own.
- Every adapter's observable behaviour is unchanged: claude still exits 2 with the same
  stderr, codex still prints the same JSON, opencode the same text, grok still passive.
  Tested per tool against the existing expectations rather than rewritten to suit.
- **Connect-or-spawn**, and ownership outlives delivery: a client that receives its items
  and exits does not take the monitor down with it, with a test.
- **Two monitors are unrepresentable**: a second `clank monitor start` on the same repo
  fails to take the lease and exits saying who holds it.
- **A wedged monitor is detected by a stalled FOLD stamp**, not by socket silence. The
  monitor evicts ITSELF on that stall, so the lease frees. *Met structurally rather than
  by the integration test this asks for: `advance_with` is the only thing that REFRESHES
  either the descriptor or the watchdog's heartbeat — the descriptor is also written once
  at startup, which ages immediately and cannot mask a wedge — and it refreshes only after
  a fold COMPLETED, so the accept loop is given no way to prove the process breathes. The
  stand-down decision is driven as a state machine over tick sequences. A test that
  freezes the real fold loop while the socket answers was not written.*
- **Recovery is tested through both doors**, which are different paths: the monitor dying
  (client reads EOF) and the monitor wedging (silence over a socket that still answers).
  With concurrent clients recovering, not one —
  `wait.rs::shared_tests::every_client_recovers_from_a_dead_monitor_and_from_a_wedged_one`
  drives four at once through each door and requires every one of them to come back alone.
  *Named here because it lives in `wait.rs` and not in `monitor.rs`, where a reviewer
  looked for it. What it drives is the CLIENT's view of each door, which is the whole of
  the client's side: a wedged monitor and a dead one differ only in EOF versus silence.
  Freezing a real fold loop belongs to the item above, and is not written.*
- **A takeover reaps the previous owner's producers** before starting its own, so a
  SIGKILLed monitor's command sources do not accumulate — and a launch happens only after
  a CONFIRMED retirement, so failing to establish one withholds the replacement rather
  than doubling it. *See the recovery limit above: a group whose leader has already exited
  cannot be identified, and that case defers instead of reaping.*
- **Sources keep their identity**: an author's config sources reach only that author, a
  `--event` source reaches only the invocation that declared it, and no client loses one.
  *Met by the deferral rather than by routing: the monitor hosts no sources, so a wait
  with ANY source of its own — config, `--event`, or master's derived draft watch — is
  refused sharing and keeps them. Nothing is pooled, so nothing can cross. The two-author
  routing test belongs with the follow-up that moves hosting.*
- A wedged monitor — alive and accepting but not delivering — is detected and superseded,
  not waited on forever. *The same capability as the two items above and not a third,
  greener answer to them: detection is the client's `BEAT_GRACE` and the monitor's own
  stand-down, both scoped as described there.*
- `clank status --watch` and the TUI consume the shared fold; the TUI keeps its own diff
  watcher, which the monitor has no reason to compute.
- `--peek` neither connects nor binds.
- A version-mismatched client falls back to a local wait instead of failing.
- No test starts a binary (`no-binary-spawning-tests`): the protocol and the registry
  decisions are driven over an in-process pipe or a pure seam.

## Out of scope — and the findings, so they are not re-derived

**Peer-socket delivery for Claude.** Verified working during this design, and the next
step after this plan: Claude Code binds a per-session inbox socket, documented at
`code.claude.com/docs/en/cross-session-messaging` — *"when you want a script or hook to
post into a session"*. Path and token are exported to every hook and Bash command as
`CLAUDE_CODE_MESSAGING_SOCKET` / `CLAUDE_CODE_MESSAGING_TOKEN`; the wire form is
newline-delimited JSON, `{"type":"user","message":{"role":"user","content":"…"}}`, with an
optional first line `{"type":"auth","token":"…"}` (required only on native Windows).
*"When the receiving session is idle, Claude Code starts a new turn with the message."*

Tested both ways: a child of the session with the token, and **an unrelated outside
process with no token** — both delivered. The outside case was tested against a
PROMPTING-mode receiver; the docs say a `bypassPermissions` receiver HOLDS such a message
for human approval, which is the mode most clank agents run in. So this route requires
`crossSessionInbound: accept` and a `doctor` check. Requires Claude Code 2.1.224+.

Its cost is the framing: ~90 words of peer-message preamble around the payload, telling
the receiver to treat it as *a teammate's request* with permission-laundering warnings —
wrong provenance for a workflow notice, and ~120 tokens per wake.

**Channels** (`code.claude.com/docs/en/channels`) is the correct mechanism and is not
usable yet: the model receives `<channel source="…">`, exactly the lean, correctly-framed
delivery this wants, but it is research preview, a channel must be an MCP server on
Anthropic's allowlist (otherwise `--dangerously-load-development-channels`), it needs
`--channels` at launch, it is unavailable on Bedrock/GCP/Foundry, and it reintroduces a
per-session process. Revisit when custom channels get a non-dangerous path.

**A mod is NOT needed, and the research that said otherwise is wrong.**
`finished/a-mod-delivers-work-in-process.md` concludes a mod cannot be the only waker,
having tested `$.prompt.submit` from a `classic.FileChanged` dispatch where the engine
refuses it. Submitting from a `$.clock.every` TIMER — outside any dispatch — works, with
no `$.agent.spawn` and no subagent. Round 1 of that research identified exactly this
spike (*"whether the engine permits that from an idle watcher … stays the gate"*) and
never ran it. That document needs correcting separately; left alone, the subagent-spawn
chain it recommends will be proposed again.

**Still to try:** a mod intercepting an inbound peer message to strip the framing before
the agent sees it. If that works, the peer socket's only real cost disappears.
