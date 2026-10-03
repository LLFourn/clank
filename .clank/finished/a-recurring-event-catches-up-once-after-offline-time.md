# a-recurring-event-catches-up-once-after-offline-time
# A recurring event catches up once after offline time

## Why

The user asks for recurring events alongside one-shot scheduled events. Missing a
deadline while Clank is offline must not lose the work, for either kind. Recurring
events must debounce rather than replay every missed tick. Preserve the user's
resource requirement: native deadlines, no new periodic polling or permanent service.

Build on the timer integration after its active plan clears review. Queueing this
plan does not change or promote that active plan.

## Public contract

- V1 adds a fixed elapsed-time recurrence to a timer definition, proposed as
  `{"kind":"timer","name":"check-deploy","due_at":"2026-10-04T09:00:00+11:00",
  "repeat_every":"24h","content":"Check deployment"}`. `due_at` is the absolute
  first scheduled instant/anchor, never reset to restart or acknowledgement time.
  Omitting `repeat_every` keeps the existing one-shot behavior. Validate positive,
  whole-second intervals and checked arithmetic with the established duration parser;
  reject unsupported duration syntax and values that cannot schedule safely.
- This is fixed interval recurrence, not cron, a timezone calendar, or a promise
  that 24 hours means the same local time across DST. Document that distinction.
  Implement no additional recurrence syntax in this bounded plan.
- One-shot events missed while offline are accepted exactly once on next arm,
  regardless of how far overdue they are. Already accepted unhandled work replays;
  acknowledged work cannot refire on rearm, restart or compaction.
- A recurring definition has due slots `anchor + n * interval`, `n >= 0`. On arm
  after offline time, compute the latest elapsed slot directly and accept at most
  one catch-up event representing the missed range. Do not enumerate missed ticks.
  Preserve its latest scheduled due instant separately from actual ingestion time.
- At most one unhandled event exists per recurring definition/generation. While it
  is unhandled, later ticks coalesce into that pending obligation without new rows,
  selectors or repeated scheduled wakes. Normal replay of an unhandled event is
  still allowed; receiving it, showing it or a wake receipt is not completion.
- Acknowledgement covers all slots elapsed through the durable acknowledgement
  boundary, including ticks that occurred while the event remained pending. The
  next due instant is the first anchored slot strictly after that boundary. Thus a
  long-running task or long offline period does not cause an immediate ack/replay
  storm. Slots after the acknowledgement boundary remain owed and coalesce into
  one catch-up event if Clank was offline again. Exact-boundary ack covers that slot.
  Do not mutate a delivered payload/ID in place to update its missed count.
- Define event payload metadata that tells consumers this is recurring catch-up,
  its schedule generation, latest represented slot and missed range/count. The
  delivered payload is an immutable acceptance snapshot; document that additional
  ticks during its pending lifetime are covered by the acknowledgement contract.
- Configured schedules remain author-owned across roster role changes. An
  invocation recurrence is local to that invocation; a new CLI invocation is a
  new scope, without an offline replay promise for a discarded invocation. With
  no monitor or armed wait, Clank cannot launch a provider at the deadline; durable
  configured schedules catch up on the next arm. Preserve side-effect-free peek.

## Definition identity and changes

Specify a normalized generation identity from source/scope, anchor and interval.
Equivalent timestamp/interval spellings must agree. Content edits do not mint new
occurrences or rewrite pending work. Anchor/interval changes make a new generation;
removing/editing future definitions cancels their future schedule at the existing
observation boundary but preserves any accepted obligations from the old generation.
An old-generation ack cannot silence a new generation. Renaming is a new source.
Reject duplicate names and incompatible mixed definitions before replacing a host;
invalid edits retain the entire last accepted host and delivery specification.

Explicitly exercise changing between one-shot and recurring definitions, removing
and restoring an identical definition, and multiple authors/CLI scopes. Do not
infer occurrence identity from a human-readable source slug or role claim.

## Durable custody and bounded state

The event WAL and acknowledgement remain the acceptance/completion authority.
Determine the exact durable recurrence-progress format before implementation:
stable slot IDs, pending eligibility, acknowledged coverage and the next anchored
deadline must reconstruct after every crash boundary. Existing Ack records include
their write timestamp; assess whether verified retained records plus a checkpoint
can reconstruct the required coverage. Never substitute current restart time for
the recorded ack boundary. Backward wall-clock movement cannot move proven coverage
backwards; define how recorded slot indices and ack time interact.

Do not append one permanently retained `.seen` ID per recurring tick: the one-shot
timer's indefinite ID-index policy would make recurring schedules grow forever.
Use bounded per-live-generation progress derived from authoritative accepted/ack
records and a crash-safe checkpoint/compaction contract. A checkpoint is derived
custody/progress, not a second mutable completion authority. Compaction must not
erase the last evidence needed to suppress covered slots or discover pending work.
Specify retention/cleanup for removed and historical generations; refuse ambiguous
or corrupt evidence rather than inventing an empty completed schedule.

Use existing source leases/write locks to serialize ingestion, ack and progress
checkpointing. Resolve ack-vs-deadline races, append/index/checkpoint failures,
lease contention and monitor takeover explicitly. Publishing must follow durable
acceptance. A rejected/failed checkpoint cannot enable a hot retry loop or silently
retire an accepted event. Discovery remains independent of current definitions.

## Resource and clock contract

Extend the existing ordered native deadline set, with no timer task/process per
recurrence, timer-specific watcher, clock/config polling, detached callback, daemon
or tight retry loop. Compute latest missed slot and next future slot in constant
time; cost must not scale with offline duration. Existing watcher/owner wakes apply
configuration and ack changes. Pending recurrences have no repeatedly expiring
deadline: suppress their scheduled sleep and rearm on observed durable ack through
existing invalidation channels. Empty/pending-only recurrence sets have no recurring
timer wake or source-log scan on every tick/heartbeat.

Keep the documented monotonic/wall-clock behavior: recheck before acceptance,
backward movement cannot fire early, and idle forward jumps are observed on the
armed deadline or existing owner/config wake. Do not strengthen that guarantee by
introducing polling. Preserve the existing monitor's heartbeat/watcher costs.

## Implementation milestones

1. Trace the current timer, WAL, ack timestamps, identity index, backlog discovery
   and watcher seams. Record the exact generation/slot/coalescing and durable
   checkpoint contracts, including schema/protocol/version/old-reader behavior,
   before coding. Identify all CLI/hook/TUI/web projections and installation order.
2. Implement additive recurrence parsing/model and passive scheduling with bounded
   durable progress, pending suppression and ack-driven rearm through the existing
   timer ownership/custody paths. Preserve one-shot behavior and rejected-config
   host retention. Avoid a second scheduler/inbox/ack implementation.
3. Drive local/shared-monitor behavior, crash recovery and presentation; document
   recurrence JSON, catch-up/debounce semantics, edits, clock limits and upgrade
   order. Report source-tree validation separately from installed binary support.

## Validation and exit

- Deterministic/injected-time tests: no early wake; anchored live recurrence; late
  first arm; one catch-up after days/years offline; no per-slot enumeration;
  pending ticks coalesce; ack at/before/after exact slot boundaries; future rearm
  preserves phase; offline again after ack yields one new catch-up.
- One-shot regressions explicitly prove offline catch-up with no prior arm,
  restart/replay after acceptance, acknowledgement suppression beyond retention,
  and removal after acceptance. Offline deadlines may not simply be skipped.
- Crash/failure tests at append, ack, checkpoint and compaction boundaries; index
  repair, corruption refusal, identity retention, concurrent waiters and ack races.
  Exercise large tick counts and verify bounded durable progress, not an unbounded
  ID set or WAL growth from coalesced pending ticks.
- Config edits/removal/restoration, old-generation ack, roster role changes,
  invocation isolation, lease contention, native watcher invalidation and monitor
  takeover with a pending recurrence and with a future anchored deadline.
- Drive real local wait and shared monitor/socket paths. Verify actual event log,
  list/show/ack, hook hints and log/TUI/web projections, with safe single-line
  summaries and lossless bodies. Read/receipt never acknowledges recurrence work.
- Assert no extra processes/tasks/watchers/polls, stable deadlines under unrelated
  traffic, no scheduled wake while pending, and bounded retries after failure.
  Run affected tests and feature/boundary checks; state platform/clock limitations.

Finish only when both scheduled and recurring offline catch-up are demonstrated,
recurring backlog is coalesced durably, and the implementation satisfies the resource
contract. No live configuration migration or binary installation is authorized by
this queued implementation plan alone.

## Milestone 1: implementation contract and traced seams

This section fixes the design for review before recurrence code. It chooses a
compacted progress record IN the existing event WAL rather than another progress
sidecar, scheduler, completion store, or per-occurrence `.seen` file.

### Current code and the changes required

- `core::agent_config::TimerSource` currently holds name, absolute due_at and
  optional content. `cli::timer_source::parse` validates whole-second RFC3339;
  `TimerSet` is an ordered `(Instant, key)` map, plus recovery entries retried on
  existing owner wakes. A successful one-shot acceptance removes its deadline.
- `timer_source::accept` takes an `IngestLease`, strictly inspects retained WAL
  IDs, repairs `source_ids::Seen`, appends an inbox, stamps a selector and signals
  logged custody. The one-shot ID is `timer/{due}`. Preserve these identities and
  historical `.seen` suppression for one-shot schedules; do not run recurrence
  through Seen. Existing inbox appends and plain acknowledgements are not synced
  like reply transactions. Recurrence needs a synced acceptance and synced ack.
- `github::log::Record::Ack` has `at`, `seq`, and an optional identity discriminator.
  `append_checked_ack` already revalidates under the exclusive sidecar write lock.
  Repeated ack is inert. `maybe_compact` currently drops old handled payloads and
  their acks; identity-only observations cannot reconstruct an ack's time.
- `EventLog::load` can quarantine corrupt logs. Timers already inspect strictly
  before calling it. Recurrence must also refuse foreign, ambiguous or corrupt
  custody without quarantine/reset; ordinary inspector commands remain read-only.
- `timer_source::Backlog` discovers configured timer logs independently of current
  definitions, plus this invocation's keys. It retains pending keys and invalidates
  on event-directory changes. Accepted work must remain discoverable after removal.
- `wait::items_with_policy` and the shared monitor select native deadlines within
  their existing lifecycle loops. `RepoStateWatcher::attach_with_timer_invalidation`
  marks an AtomicBool on event-directory changes/rescan notices. Currently that
  flag invalidates backlog only; it must also refresh recurrence scheduling state.
  `monitor::reconcile_hosting` retains accepted hosts on invalid config and leaves
  unchanged specs running. Extend that path without rebuilding timers on every beat.
- The existing filesystem watcher is actually `notify::PollWatcher`, including
  normal mode, to avoid measured macOS native-watch attachment costs. Recurrence
  adds no watcher or poll cadence; native refers to timer sleeps, not a claim that
  the existing repo watcher uses OS push notifications. Preserve its bounded roots.

### Identity, slots and payload

Parse `repeat_every: Option<String>` using `wait::parse_duration_str`, require a
positive integral number of seconds, and normalize to `interval: u64`. Keep the
existing timestamp rules and use checked arithmetic throughout. Validate a complete
replacement specification before changing any running source or delivery route.

A generation descriptor is `{name, scope, anchor, interval}`: scope is configured
or the exact invocation token; author/repo ownership is supplied by the enclosing
events directory. Store and validate the full descriptor, not merely its hash.
The generation ID is the full BLAKE3 hex digest of the JSON-encoded tuple
`("clank-timer-recurrence-v1", name, scope, anchor, interval)` in that order, with
scope encoded as null for configured and a string for invocation. Do not include
content, role, interval spelling or timestamp spelling. Hash/key disagreement is
an error, never grounds to merge generations.

Give each recurrence generation its own WAL key under the existing discovery
prefix: `timer-config-recurring-{generation}` or
`timer-invocation-recurring-{generation}`. This separates recurring state from
one-shot Seen and separates old/new generations, avoiding a multi-generation
checkpoint map in one WAL. A source-name collision with another definition is
still refused during whole-spec validation. One-shot keys remain unchanged.

For wall second W, latest(W) is absent if W < anchor, otherwise
`(W - anchor) / interval`. With covered slot C, first owed is C+1; with no coverage
it is 0. If latest(W) >= first owed, accept ONE immutable snapshot with
`first_slot`, `last_slot = latest(W)`, and `count = last_slot - first_slot + 1`.
Validate count and due arithmetic before append; no missed-slot loop. Producer ID
is `timer-recurring/{generation}/{last_slot}`. `due_at` is the anchored instant of
last_slot; inbox `at` is actual acceptance time, and `event_at` remains due_at.

Add optional `recurrence` metadata to `WaitItem::TimerEvent`, omitted for old
one-shot rows. Its typed value contains the full generation descriptor, generation
ID, first_slot, last_slot and count. The next event starts after proven coverage,
not after the previous snapshot's last slot if more ticks elapsed while pending.
Never change a pending row's ID, content or metadata. Content-only edits affect
the next acceptance. No automatic receipt, list/show or wake may acknowledge it.

### The one authoritative transition

Expose recurrence operations THROUGH EventLog, keeping parsing, canonical grouping,
ack matching, allocation and sidecar locking inside `github::log`. Do not expose
raw unlocked log writers or independently match seqs in timer_source.

At initial admission, event invalidation or a due deadline, fold a strictly
validated generation WAL into `covered_through: Option<u64>`, pending row (zero or
one logical group), and allocator high-water. All physical copies of a logical
group use the existing `(seq, logical_ident)` grouping. More than one distinct
unhandled occurrence in one generation, conflicting metadata, mismatched feed ID,
impossible slot arithmetic, reply records or foreign records fail closed.

To accept, take the existing nonblocking IngestLease, THEN acquire the exclusive
sidecar data lock. Re-read/fold under that lock; this is essential because ack does
not take the ingest lease. Repair a crash-torn tail before an append. If pending,
return Pending with no append; if nothing is owed, return the anchored next due
instant; otherwise allocate and append one complete inbox with the snapshot.
Sync the file and containing directory before publishing. Update the caller's
state to Pending after acceptance; there is no recurring deadline while pending.
Release data lock and ingest lease before sending the logged emission signal.
The signal is advisory; a closed sink cannot erase durable accepted work. Initial
admission/recovery must sync complete existing recurrence evidence under the lock
before treating it as durable and releasing it through backlog publication.

For a recurring row, `append_checked_ack` uses the SAME exclusive data lock and
strict generation fold. Capture the recorded wall second A inside this critical
section, then append/sync its existing discriminated Ack record. Coverage is
`max(previous coverage, row.last_slot, latest(A) if present)`.
The snapshot's last slot is the minimum coverage even if the clock went backwards.
The boundary means the wall timestamp recorded in this durable acknowledgement,
not an unknowable instant after fsync or CLI exit. A retry of an already acked row
syncs existing evidence if necessary and never advances coverage to retry time.
Use the first covering ack for a row; conflicting duplicate ack evidence must not
silently enlarge coverage. Old-generation ack affects only its own generation WAL.

Ack-versus-deadline order follows the sidecar lock. If a row is still pending,
deadline inspection cannot append; if ack won, acceptance can cover only slots
after its proven boundary. Accepted slots are never regenerated, including after
backward time movement. Read failure, lock contention or a failed sync disables
the scheduled deadline and retries on an existing owner wake, with a diagnostic
only when the failure changes. No zero-delay retry sleep.

### Compaction, crashes and retention

Add `Record::TimerProgress {v, seq, generation, covered_through}`. Here generation
is the full validated descriptor plus ID, covered_through is an optional index,
and seq is the reserved allocator high-water, not a new inbox selector. This is
derived compacted WAL evidence, much like an identity observation. It cannot be
changed by a receipt or used to retire a pending row. Only verified inbox+ack
evidence or a previously valid progress record may advance it.

Under the exclusive sidecar lock, compaction first strictly folds recurrence
evidence, then writes exactly one merged progress record, every unhandled group,
and ordinary bounded recent handled presentation history with its matching acks.
Recurring identities need no per-slot identity horizon or Seen directory: coverage
plus pending custody replaces it. Include the high-water of all original records
to prevent selector reuse after history disappears. Sync temp file, atomically
rename, sync directory. Compaction failure leaves either old or new complete WAL;
never remove original evidence in an earlier separate checkpoint write.

Before inbox sync, no event is published. After a complete append but failed sync,
a retry strictly folds and syncs the existing row before using or publishing it;
it cannot append a duplicate. After durable append but before emission, discovery
replays it. Ack failure retains pending custody until existing evidence is synced.
Before compaction rename, old inbox+ack evidence suffices; after rename, progress
and retained pending rows suffice. A torn final append is repaired without allowing
later appends to hide it inside the WAL. Interior corruption/foreign semantics
remain untouched and require repair. Pending payload may not be reconstructed
from config, so unavailable WAL evidence cannot justify suppressing or completing it.

Storage grows with retained history and distinct definitions, NOT elapsed ticks.
For one stable generation: one progress record, at most one pending logical row,
bounded recent display history, and append overhead bounded by the existing
compaction threshold. No tick-specific files or permanently pinned tick IDs.
Keep removed generations' small coverage tombstones and any accepted obligations;
restoring the identical descriptor must resume after its old coverage. There is an
unavoidable lower bound of one tombstone per historical generation if restoration
must preserve this guarantee indefinitely. Do not falsely promise storage bounded
only by currently live definitions. There is no automatic tombstone GC in V1;
deleting a source's custody explicitly resets its history, outside this plan.
Changing one-shot to recurring creates separate custody; changing back restores
the original one-shot identity rules. Config removal cancels future scheduling,
not an already accepted obligation. Invocation history has no cross-invocation
catch-up promise; leave existing invocation custody lifetime unchanged.

### Owner integration and no lost wakeups

TimerSet states are Future(deadline), Pending(no deadline), and Recovery(no deadline).
On initial arm or takeover, fold recurrence evidence once. Future entries enter
the existing ordered map; Pending entries remain owned without repeatedly waking.
Each event-directory invalidation refreshes relevant recurrence entries and backlog
once through the existing owner loop. The current dirty flag is author-coarse;
reuse it and refresh live recurrences only on that flag, not every heartbeat or
unrelated repo commit. An unchanged host spec must preserve future deadlines.
Future-to-Pending happens in the acceptance transaction; Pending-to-Future happens
on observing durable ack. If already overdue after ack, transact one catch-up.

Attach the existing watcher before the final admission fold, or perform a final
refresh after attachment, so an ack between initial inspection and watcher setup
cannot leave a Pending entry asleep forever. Clear dirty before folding, allowing
any concurrent write to set it again and enqueue the next existing wake. A dirty
event caused by one's own append may cause one harmless reinspection, never a
write/checkpoint cycle. After watcher disconnection, follow the existing explicit
error/fallback policy; do not introduce silent recurrence polling. Explicit
`--poll` uses the already requested poll cadence for invalidation; native mode
adds no heartbeat log scans for healthy Future/Pending entries.

At due wake recheck wall time and custody. A backward jump reschedules the same
anchored target, never fires early. A forward jump yields the latest elapsed slot
when the armed deadline or existing owner wake observes it. No periodic clock
watcher is introduced. Initial arm catches overdue one-shots and recurrence even
after arbitrarily long offline time; no armed process means no provider launch.
Peek only inspects existing accepted custody and must not create a progress record,
mint an invocation, accept a due event, or start a producer.

### Format, presentation and rollout

Keep JSON WAL v1: new TimerProgress is a new `t`, not reinterpretation of an old
record, and recurrence metadata is an optional TimerEvent field. New readers
understand progress; one-shot v1 bytes retain their meaning. Old readers treat
TimerProgress as foreign and may omit metadata. Consequently old writers are
NOT safe recurrence custodians: their foreign retention is bounded, and old config
parsers can ignore repeat_every and turn it into a one-shot. Do not claim mixed
old/new operation or downgrade safety; a marker cannot make an already shipped
binary obey a rule it does not implement.

Advance monitor PROTO from 8 to 9 for recurrence hosting and enriched delivery.
Use the existing version negotiation/retirement and lease handoff; do not kill
owners by guessed PID. Deploy current CLI and paired monitor/web host binaries,
retire/restart old monitors and waiters through their supported lifecycle, verify
the active host supports recurrence, THEN enable recurrence config. Updating code
does not update an already running monitor. This plan alone does not authorize
installation/live config edits. Downgrade requires disabling recurrence and
preserving its custody for a supporting version, not running old writers on it.

Update `core::wait::TimerEvent`, `EventView` recurrence provenance, the explicit
`wait::WaitJsonItem` mapping and human/hook formatting. Events list/show/ack keep
the existing selector and completion interface; show/JSON carry the full immutable
metadata/body. TUI/log/web event projections derive recurrence summaries from the
shared event view; they must show first/latest due/count without leaking control
bytes, while the original content remains lossless. Keep ordinary timer kind and
one-shot presentation stable. Wake receipts remain unrelated to ack/coverage.
Web bridge framing/wincode schema need no new transfer kind: inspect the actual
event projection mapping and carry the enriched projection through the current
publication path. Browser rendering must not discard metadata or duplicate rows.

Implementation order after this contract clears review: core parsing and pure slot
arithmetic; strict WAL fold/durable acceptance+ack+compaction; TimerSet state and
watcher rearm; all projections/docs; real local/shared acceptance and failure tests.
Test exact boundaries and backward clocks with injected time, not timing sleeps.
Include thousands of completed occurrences and decades of skipped slots to prove
that storage/CPU do not grow with skipped ticks, and race tests for watcher
attachment, concurrent ack, compaction and takeover. Until these checks pass and
reviewers finish the plan, this section is a design contract, not shipped support.

### Implementation progress: recurrence model and arithmetic

Added optional repeat_every and the typed normalized generation descriptor, plus
pure generation hashing, occurrence IDs, anchored slot/catch-up/ack calculations
in timer_source::recurrence. Validation rejects zero, unsupported syntax and
overflow; legacy one-shot JSON omits the new field. Arithmetic covers decades of
offline ticks in one fixed-size snapshot and preserves coverage under backward
wall time. No task, clock read or I/O was added to these calculations.

This is a staged milestone: TimerSet explicitly refuses recurrence while durable
WAL transitions are not implemented, rather than silently treating it as one-shot.
Remove that temporary refusal when strict WAL acceptance/ack/compaction and owner
integration are ready. The pure helpers are currently exercised by tests and will
become production callers in that integration; no recurrence has been enabled or
installed by this commit. Remaining milestone 2 work starts with WAL custody.

### Implementation progress: durable WAL transitions

EventLog now owns a strict recurrence fold and an acceptance transaction under
the existing ingest lease and exclusive sidecar lock. It syncs existing recovery
evidence and new inbox custody before returning an Accepted transition; Pending
does not append another occurrence. The ordinary checked-ack entry point routes
recurring rows through synced discriminated acknowledgement, with retries retaining
the original recorded boundary. Immutable recurrence metadata reaches the typed
TimerEvent and explicit wait JSON projection; one-shot metadata remains omitted.

Recurring compaction retains one TimerProgress record, pending custody and bounded
recent display history, using the same atomic WAL rewrite helper as ordinary
compaction. The progress record reserves allocator high-water and requires an
explicit coverage field (null is valid, missing is not), preventing a damaged
checkpoint from resetting completion. No recurrence Seen files are written.
Strict reads refuse foreign/corrupt semantics, conflicting copies/ack timestamps,
unproven range gaps and generation/key disagreement without quarantine or reset.

Tests exercise large offline catch-up, pending coalescing, retry ack boundaries,
backwards clock, compaction retention/allocator recovery, complete and torn tail
recovery, failed rewrite, ingest contention, ack/deadline races, concurrent writers,
malformed ranges and missing checkpoint coverage. This remains a staged custody
milestone: TimerSet still refuses recurrence, and no binary/config was installed.
Next integrate Future/Pending/Recovery into the existing owner loops, including
watcher admission races, PROTO 9, no heartbeat reinspection, removed-definition
backlog recovery, presentation and real local/shared-monitor validation. Explicit
sync-failure injection and long-run automatic-threshold compaction remain part of
the final failure/resource verification; the current tests force retention rewrites.

### Implementation progress: owner and watcher integration

TimerSet now owns Future deadlines, Pending entries without a sleep, and Recovery
entries retried on existing owner wakes. Its constructor remains passive; arm
folds recurrence custody after ownership is accepted. Existing deadline selection
and emission channels publish the locally stamped selector without persisting it.
Acceptance invokes the existing compaction threshold. No recurrence tasks,
producers, watchers, or poll cadence were added. Future deadlines survive unrelated
traffic and pending schedules do no heartbeat WAL I/O.

The local wait performs a final refresh after watcher attachment to close the
ack/admission gap. On event invalidation it refreshes recurrences once and retries
one-shots separately; unchanged healthy recurrences do not join heartbeat retries.
The monitor reconciles config before recurrence refresh, avoiding acceptance from
removed definitions at that observation boundary. It retains dirty across a failed
repo fold, and avoids retrying a failed recurrence twice on the same beat. Monitor
PROTO is now 9; current source support still requires the rollout order above.

Backlog recovery validates and syncs recurrence custody even after definition
removal. Peek uses the read-only path. Healthy pending recurrence rows are cached
until existing invalidation, and already shown bodies are not cloned on heartbeats.
Monitor admission uses its existing fresh inventory, so joining clients do not
inherit a stale cached ack state. A refused generation emits a stable diagnostic
and cannot block inspection/discovery of healthy logs in the same directory.

Owner tests prove pending sleep suppression and no heartbeat reads, backward-clock
rechecks, ack rearm/phase, takeover, cached backlog/peek boundaries, and automatic
threshold compaction across a seeded history of thousands of completed slots.
Real local wait tests exercise decades-offline catch-up, replay, ack after removal,
restore without refire, and a parked native deadline. The affected monitor suite
also exercises ack refresh/removal and protocol negotiation. Integration tests
must run without the enclosing agent's session IDs; otherwise fixture ownership
is rejected, as it should be. No binary or live configuration has been installed.

Remaining before finish: enriched human/hook/TUI/web presentation and docs; a real
shared-monitor recurrence acceptance test including watcher-driven rearm; explicit
sync-failure injection; and the final platform/resource and upgrade report. Source
recurrence scheduling is now connected, but this plan has not cleared final review.

### Implementation progress: presentation and rollout documentation

CLI event descriptions, human wait output, stop-hook hints, log descriptions and
TUI detail now share a recurrence summary with the immutable accepted count, due
range and interval. The TUI wraps the schedule detail so a narrow pane can scroll
through the full range. Timer names and content in human wait hints are sanitized
to one line; durable content and structured output remain lossless. Merged timeline
events, web event state and structured timer log rows retain full recurrence
metadata; legacy one-shot rows omit the optional field.

README documents fixed elapsed-time recurrence syntax, offline catch-up, pending
coalescing, durable acknowledgement coverage, frozen acceptance metadata,
generation edits/restoration and retained tombstones. It explicitly states the
PROTO 9 deployment order, old-writer/downgrade incompatibility, and the unchanged
repository PollWatcher cadence. This milestone does not install a binary or edit
live timer configuration.

Validation: 46 timer-filtered unit tests (including real-WAL projections into
human wait, hook, log/web state and TUI); 17 timeline and 28 event tests; 10 command
tests including README contracts; and both git-boundary integration tests passed.
Remaining: real shared-monitor recurrence acceptance and watcher-driven ack
rearm, explicit sync-failure injection, and final platform/resource reporting.

### Implementation progress: real monitor and durability acceptance

The subprocess acceptance fixture starts the current source-built monitor and
admits a real stop-hook waiter before adding a recurring configuration. The
existing watcher observes the edit, delivers one offline catch-up range, and
pending replay leaves the WAL byte-identical. A real CLI acknowledgement then
rearms the monitor; a second admitted hook receives the next anchored occurrence
through its native deadline. Selectors advance and timer delivery has no command
receipt side effect. The fixture retires its monitor with the normal newer-peer
handoff and waits for exit; subprocess cancellation is kill-on-drop.

Per-handle, test-only file/directory sync fault injection now exercises failed
acceptance, recovery, acknowledgement and compaction at the actual common sync
boundaries. Complete visible appends cannot be delivered through recovery until
sync succeeds. Failed acknowledgement retries keep the first recorded boundary,
even if retried much later. File-sync failure before compaction rename and
directory-sync failure after rename both preserve pending custody and proven
coverage on restart. The injection has no state or branch in production builds;
the existing durability helpers retain the same file-sync/directory-sync order.

Validation on this macOS host: 48 WAL tests, 48 timer-filtered unit tests, 9 reply
transaction tests and both git-boundary tests passed. The real shared-monitor
recurrence acceptance test passed (10.42 seconds). Earlier milestone results
cover the 136 monitor tests, real local one-shot/recurring waits, config/identity
contracts, presentation and README. Formatting and whitespace checks pass.

### Final resource, clock and rollout report

- Recurrence adds no process, task, producer, watcher, periodic poll or callback.
  The owner selects its existing earliest native deadline. Healthy pending
  generations have no deadline and no heartbeat WAL reads; poison-WAL and cached
  backlog tests establish this independently of timing. Unrelated traffic retains
  the same future deadline. Failure moves an entry out of the sleep set and retries
  on existing owner wakes, with stable diagnostics rather than a hot loop.
- Missed-slot arithmetic is constant time, regardless of offline duration. WAL
  folding costs retained custody/history, and deadline-map updates cost the number
  of active definitions, rather than elapsed ticks. Config/event invalidations can
  refresh the author's active recurrences; they do not introduce another cadence.
  The repository already uses PollWatcher and the monitor already has a five-second
  fold heartbeat; recurrence does not change either cost.
- Automatic compaction retains per-generation coverage, all pending custody and
  bounded recent display history. It does not create recurrence Seen files or
  enumerate missed ticks. Distinct historical definitions retain tombstones, so
  storage scales with generation churn, not just currently configured schedules.
  This is intentional: removing that evidence would permit completed work to
  replay when an identical definition is restored.
- Whole-second wall time defines slots; monotonic sleeps govern waiting. Due
  acceptance rechecks wall time, so a backward jump cannot cause early delivery
  or reopen proven coverage. An idle forward jump is observed at the existing
  deadline or another owner wake; no periodic clock monitor was added. Fixed
  elapsed intervals have no timezone/DST calendar semantics.
- End-to-end sockets, watcher and file/directory sync were validated on macOS.
  No Linux run or physical power-loss test was performed. Fault injection verifies
  software refusal/retry paths; filesystem durability still depends on the OS and
  storage honoring sync. The monitor/locking paths retain their existing Unix
  platform requirements; this plan does not claim new Windows support.
- This is source-tree support only. No installed binary, live configuration or
  provider session was changed. Deploy the current binary everywhere and retire
  old monitor/waiter processes before enabling recurrence (PROTO 9). Old binaries
  can ignore the new interval and cannot safely write compacted progress; mixed
  writers and downgrade are unsupported. A configured timer cannot launch an
  absent provider while all Clank owners are offline; it catches up on next arm.

All implementation milestones are now submitted for the final review gate. Only
the gate's finalize instruction authorizes finishing this plan.
