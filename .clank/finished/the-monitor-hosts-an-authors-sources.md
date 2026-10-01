# the-monitor-hosts-an-authors-sources
# The monitor hosts an author's config sources

## Why

`a-repo-is-watched-once-not-once-per-waiter` put one watcher and one fold in one process
per repo, and `clank wait` now shares it. One thing it did not move: an agent's configured
`wait_events`. A wait that has any source of its own — a config `wait_events` entry, a
`--event` source, or master's derived draft watch for an open PR review — refuses to share
and keeps watching for itself.

That refusal is safe (nothing is lost) but it is a hole in the dedup: a repo whose agents
configure sources gets none of the benefit. The measured case that started that plan was
98 parked waits for 71 agents with ~170 watchers.

**The draft said "every park spawns its own producers". That is wrong and is withdrawn**
(codex on 277d949): `IngestLease` already serialises spawning — the holder runs the
producer and every other park TAILS its log. What is actually still duplicated, and what
this plan is for:

- **The watcher.** A wait with any source refuses to share, so it attaches its own
  `RepoStateWatcher` and refolds the whole repo on every change — the exact cost the
  previous plan removed for everyone else.
- **Producer ownership is tied to a DELIVERY CLIENT**, which is the same mistake
  connect-or-bind would have been for watching. The lease holder is a parked wait; when it
  delivers it returns and drops the lease, a tailer acquires it, and the user's command is
  spawned afresh. So the producer churns on every wake rather than running once. A monitor
  holds the lease for its own lifetime, which is the point of it being a process of its
  own.
- **A tail poll per non-holder**, every `TAIL_INTERVAL`, for each of the N-1 parks that did
  not get the lease.

## What makes this more than wiring

`EventSources::spawn` is easy to call from the monitor. The hard part is who PRESENTS.

The sources' durability is a ~4.2k-line WAL — `cli/command_source.rs`,
`cli/github/supervisor.rs`, `cli/github/log.rs` — whose central invariant is that a row is
re-presented until it is acked. Today the presenter is the process holding the source's
`IngestLease`: it spawns the producer, appends rows, and streams them to its own agent,
carrying a per-process `presented: HashSet<(seq, ident)>` so a takeover does not re-present
what it already showed. A non-holder TAILS the holder's log and takes over when the holder
leaves.

Make the monitor the holder and that model needs re-deciding:

- The monitor outlives every client, so its `presented` set is no longer "what this agent
  has been shown" — a client that connects later has been shown nothing, and the monitor
  must replay that author's unacked rows to it at admit time rather than only forwarding
  what arrives next. Forwarding only new items silently drops the backlog; replaying
  against the monitor's own `presented` set silently drops it too.
- Acking stays the agent's (`clank events ack`, by selector), so the unacked set is
  durable and per-author — which is what makes a correct answer possible, but it has to be
  read from the log per client rather than tracked in the monitor.
- `--event` sources must stay with the invocation that declared them (`Hello.has_cli_sources`
  already tells the monitor they exist, and `WakeSources::is_empty` already refuses sharing
  for them). Master's derived draft watch is per-review, not per-config, and would need the
  same per-author treatment or the same refusal.

### What was decided

The channel from a producer carries a SIGNAL; the LOG decides. `owed_from_sources` reads
that author's unacked rows out of the WAL at admit time and on every wake, against the
`shown` set on the `Client` — so there is one delivery path instead of a backlog path and
a live path with different rules, which is what would have sent a pre-connection row
twice and an acked row anyway. `shown` lives on the connection, so a successor client is
owed what its predecessor was shown and never acked.

Hosting follows the ROSTER, not who is connected: `reconcile_hosting` compares each
author's resolved spec against what is running and touches only the author whose spec
changed. A producer therefore lives as long as the monitor rather than as long as a
delivery client, which is the churn this plan set out to remove.

Durability is now carried rather than inferred: a source emits `(WaitItem, Durability)`,
because `is_logged`-by-equality was wrong for every durable command event (the selector is
stamped after the append) and because github warns once and keeps delivering after a failed
append — a reader told that row was durable would present from a log that has no row for it.

The derived draft watch and `--event` sources are refused sharing rather than hosted:
`may_share` destructures `WakeSources` so a new field cannot default into being shareable.

## Acceptance

- An author's config sources run ONCE per repo, in the monitor, and reach only that author.
  *`hosting_follows_the_roster_and_reconciles_on_a_config_change` drives the once-per-repo
  half from the roster with no client attached; `two_authors_sources_do_not_cross` writes a
  row into each of two authors' event dirs and serves BOTH through one `serve_to`, so
  cross-talk is possible there and excluded.*
- A client that connects after a row was logged still receives it if it is unacked. Exactly
  once PER CLIENT ADMISSION, not once per row for all time: an unacked row stays eligible
  for a later client and for a reconnect, because being shown to a client that then died
  is not being handled. Only an ack retires a row.
  *`a_logged_row_is_presented_once_and_never_after_an_ack`: a row logged before anyone
  connects, delivered once however many arrival signals follow, owed again to a SUCCESSOR
  client, and never after an ack. `shown` is a field of `Client`, so it cannot outlive the
  connection it describes.*
- A `--event` source still reaches only the invocation that declared it, with that
  invocation keeping its own channel.
  *Two independent refusals, both tested. `hostable` resolves no CLI items at all, so such
  a source cannot arrive at a pooled host
  (`nothing_the_monitor_hosts_belongs_to_an_invocation`,
  `a_pooled_host_will_not_run_an_invocations_source`). And `may_share` refuses the wait
  itself: `a_wait_shares_when_the_monitor_covers_all_of_its_sources` drives all three
  outcomes — no sources, config-only, and an invocation's — each against what `share`
  then does. `a_configured_and_an_invocation_source_of_one_name_do_not_share` is the
  SEPARATE property that a configured and an invocation's source of one NAME keep their
  own log, lease and state directory; it calls no `may_share` and is not evidence about
  sharing (codex on 4423f27).*
- Two authors in one repo, one of them with a CLI source, and neither loses an event.
  *`a_cli_author_keeps_its_own_channel_while_another_shares`.*
- A config change restarts only the affected author's sources.
  *`a_config_change_restarts_only_the_edited_author`: two authors each hosting a producer,
  one edited, and the other's task ids unchanged. Written because the single-author test
  caught a repo-wide rebuild only through a source-less author taking the restart arm every
  pass — hoisting the `spec.is_empty()` skip removes that coverage, and I ran both edits
  together to check which test still fails.*
- The existing WAL tests pass unchanged rather than being rewritten to suit.
  *No WAL test asserts anything different, and the inventory below is counted from
  `git diff 277d949..`, because the hand-written version of it was short by the biggest
  file (ruthless on 4423f27). Two mechanical changes run through it:*

  *(a) a source channel carries `Emission` = `(WaitItem, Durability)` instead of a bare
  `WaitItem`, so a holder's destructuring gained a `, _` — `github/coordinator.rs` (the
  largest share, and the one the first inventory missed), `github/supervisor.rs` in three
  tests, and `pr_review.rs`, where only the `next_draft` test HELPER changed and no test
  body did. (b) `backend_state_dir` takes the resolved storage key instead of the source:
  one line in a `command_source.rs` test.*

  *Five assertion lines were removed in the whole range. Three are coordinator's
  `item`/`woke` → `.0`, each keeping its message, including `"byte-faithful record"`. The
  other two are in `wait.rs`, belong to the `may_share` rewrite rather than to a WAL test,
  and are two DIFFERENT changes rather than one (codex on 4fac4ac). `assert!(nothing
  .is_empty())` became `assert!(nothing.may_share())` — same subject, and the property
  that replaced `is_empty`. The second, `!configured.is_empty(), "the fixture declares a
  source"`, guarded a fixture CALLED `configured` that was built from a `--event` string:
  it is now `invoked` and drives the refusal, while the name `configured` went to a real
  config-file fixture carrying its own guard at `wait.rs:2714`, immediately above
  `assert!(configured.may_share())`. That guard matters because `may_share` is
  `drafts.is_empty() && events.iter().all(…)`, and `all()` on an empty iterator is TRUE —
  without it the assertion is satisfied by a fixture declaring no source at all. One test
  was ADDED, `a_write_that_did_not_land_is_reported_as_transient`.*

## Out of scope — and what the reap ACTUALLY does, since the draft's first
## description of it is no longer true

The takeover reap landed, but not in the shape this draft first described. Written down
here because moving hosting to the monitor changes who a producer's owner is, so the next
plan touches exactly this:

- A producer records its child's **pgid and start time** beside the source log. The start
  time is the identity: `argv[0]` was tried and is wrong in both directions — a recycled
  pid running another copy of the same command matches, and `sh -c '<cmd>'` EXECS, so the
  live process reports the exec'd name and the producer stops recognising its own child.
- `ppid == 1` is NOT required and was deliberately removed. It is what an orphan looks
  like on macOS but not on a Linux session with a subreaper, where orphans reparent to the
  session manager — requiring it would defer such a group forever. The entitlement to
  signal is the LEASE, which already proves the previous owner's process is gone.
- The **group** is what must be empty, not the leader: a `sh -c` that backgrounds
  something exits while that descendant keeps running in the same group.
- A launch happens only after a CONFIRMED retirement (`Retired::Clear`). Evidence that
  cannot be established withholds the replacement rather than doubling it.
- **The open limit, and the one this plan should think about:** a group whose leader has
  ALREADY exited cannot be identified at all — the kernel only promises an id is not
  reused while its group has members, which says nothing about the gap since the record
  was written. That case defers. A token in the child's environment would close it, but
  `ps -E` cannot read another process's environment on current macOS. If the monitor
  becomes the producer host, it is alive while its producers are, which may make a
  continuously-maintained membership record possible where a cold one is not.
  **It is the host now, and the gap is still open:** nothing here maintains such a record,
  so a successor still reads a COLD sidecar and a group whose leader has already exited
  still defers instead of being reaped. The opportunity is real and unspent.
- A process group is a validated `Pgid`, never an integer: `kill(-1, …)` is every process
  the user may signal. `signal_boundary` is a gate that fails the build if production code
  aims a signal at a group through a bare number.
