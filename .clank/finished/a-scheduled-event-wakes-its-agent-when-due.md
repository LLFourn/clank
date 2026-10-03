# a-scheduled-event-wakes-its-agent-when-due
# A scheduled event wakes its agent when due

## Why

The user, via codex in /Users/llfourn/src/anxiety-attack, requests a native timer
event for scheduled tasks. A command source can implement a blocking timer today,
but launching a process per schedule is unnecessary overhead. The ordinary event
`at` field is provenance, not a request to delay delivery; do not repurpose it.

Add an explicit one-shot scheduled source to the existing event lifecycle. The
deadline wakes an armed wait; ingestion, replay, showing and acknowledgment keep
their separate meanings. Use the monitor's existing configured-source host and
delivery-owner sink rather than a second scheduler daemon or polling loop.

## Scope and public contract

- Add an opt-in `wait_events` source with a distinct `kind: "timer"`, a required
  nonempty `name`, an absolute RFC3339 `due_at` with explicit offset, and optional
  `content`. For example:
  `{"kind":"timer","name":"check-deploy","due_at":"2026-10-04T09:00:00+11:00","content":"Check the deployment"}`.
  This is the proposed new interface, not an already-supported config example.
- One definition represents one occurrence. No recurrence, cron, relative-time
  restart semantics or timezone calendar engine in this plan. Editing `due_at`
  creates another occurrence. Normalize equivalent timestamps to one instant so
  timezone spellings cannot create duplicates. Reject invalid/ambiguous timestamps
  and empty names with a useful diagnostic before starting a source.
- Scope configured sources to their author, as other configured sources are.
  Invocation-only `--event` timers retain invocation-local scope and recovery
  identity. A roster role change must not make a configured author timer disappear
  or silently transfer its ownership. Repo-wide master-target schedules, if needed,
  require a separately specified routing contract; do not claim author timers solve
  Anx's existing backend-owned master handover of unfinished tasks.
- The event becomes eligible at its due instant. If armed late or restarted after
  the deadline, ingest it immediately once. Its ID is stable from source identity
  plus normalized due instant, independent of arm time, process, session and content.
  Preserve the due time as event provenance; use actual ingestion time separately.
- Publish through the existing WAL and synchronous sink. Re-arm, monitor takeover,
  crash recovery or duplicate waiters cannot mint a second logical occurrence.
  Once ingested, an unacked occurrence remains owed across restart/config edits;
  showing or a future receipt callback does not complete it. An acked occurrence
  cannot refire merely because its definition is still configured, including after
  bounded WAL identity retention. Use the established identity-retention contract,
  not a second mutable completion authority.
- Removing/editing a definition before ingestion cancels/replaces its future
  occurrence when the source owner observes the change. Removing it after ingestion
  does not delete accepted work. Use a distinct timer key namespace, lossless name
  hashing and existing origin/invocation scoping. Reject duplicate timer names within
  one origin/scope rather than let conflicting deadlines contend for one identity.
  Reusing the same name and normalized due instant refers to the same occurrence;
  renaming creates a new source, as for command sources.
- A timer wakes existing Clank wait/lifecycle machinery. With no running monitor or
  armed client, there is no promise to launch a provider at the wall-clock deadline:
  persist the definition and catch up on the next arm. State this clearly.

## Clock and resource contract

Use one owned deadline scheduler per hosting scope, with an ordered set of its
pending timers and one Tokio sleep to the nearest deadline. Do not spawn a process
or forwarding task per timer, scan the process table, poll files/clock periodically,
or create a new permanent service. Empty schedules have no armed timer. Config
changes use existing wake/reconciliation channels to replace the pending set and
nearest deadline. The monitor observes configured-source edits during its existing
refold/reconciliation. Local fallback and invocation sources retain their current
arm-time configuration contract: changes apply on the next arm, not through a newly
added timer/config polling loop. Cancellation drops native deadline state, joins
ordinary source tasks and releases existing leases. Definition removal becomes
effective at that observation boundary,
not atomically with an external editor's filesystem write.

Compute the monotonic wait from wall time when arming; recheck wall time before
ingestion so a backward clock adjustment cannot fire early. Re-arm only for an
actual remaining deadline or an existing change notification. Document that a
forward wall-clock jump during an otherwise idle monotonic wait is observed at
its armed deadline or next definition re-arm/config change, not through clock polling.
If a stronger clock-adjustment guarantee is requested, specify an OS notification
contract first. Suspend/resume and past-due behavior must be exercised where the
platform permits and their limits stated accurately.

No new overload/drop policy: preserve durable custody and bounded callback/client
resources. The quality audit's existing unbounded-ingress finding remains separate.

## Implementation milestones

1. Trace source config/validation and stable keys, core event projection, WAL
   identity retention, hosted/local startup, source leases, config-change handling,
   wait/hook/TUI/web consumers. Confirm the interface and write the exact identity,
   clock, removal, restart and invocation contracts before implementation. Determine
   any required cache/protocol transition from actual serialized consumers.
2. Add the config/model and native scheduler as one coherent source integration.
   Reuse EventSources ownership, neutral emission sink and current WAL/retention
   seams; any extracted shared helper must have actual command and timer consumers.
   Avoid a timer-only parallel event inbox/dedup/completion model. Admission and
   current command/GitHub source behavior remain unchanged.
3. Drive the end-to-end timer wake, durable replay/ack, config edits, takeover and
   cleanup. Document the supported config and installation order: existing configs
   need no migration; new timer configs require a binary that supports the variant.
   Report supported installed features separately from source-tree implementation.

## Validation and exit

- Deterministic injected/paused-time tests: nothing before due; one occurrence at
  due; late arm; equal instants with different offsets; invalid config; several
  deadlines; cancellation; clock recheck; empty schedule; config removal/replacement.
- Real driven wait/monitor tests: a parked agent wakes; two waiters share one host;
  authors and invocation scopes stay isolated; restart/takeover catches up without
  duplication; an unacked event survives source removal and an acked definition
  does not refire after dedup retention boundaries; failure keeps custody truthful.
- Test source serialization/projection, hook and TUI event display, CLI list/show/
  ack, and relevant Git/identity/runtime/process-cost boundary checks. Preserve all
  existing command/GitHub/reply behavior. No assertions based only on helper success
  or arbitrary sleeps when the actual wake can be synchronized.
- Report test counts and platform/real-clock/provider limits. Finish after these
  contracts and checks; do not absorb recurrence, read receipts, Anx completion or
  repo-wide master routing into this timer feature.

## Related receipt request

The same message asks about producer notification when an utterance is read.
`a-source-is-told-when-its-event-woke-someone` is currently an unpromoted draft
with unresolved observable-boundary/dispatcher/failure contracts. Required-reply
support is already implemented and is different. Timer delivery must not invent
a receipt callback or call `anx ack`/`anx say --done`. Receipt-by-utterance-ID should
remain idempotent and must leave backend-owned work owed across master handover.

## Promotion assessment — 2026-10-03

Assessed against HEAD `a83d1b335d82` after the quality audit finalized. The current
`core/agent_config.rs::WaitEventSource` is a tagged Github/Command sum, with the
command source key owned by `CommandSource::source_key`. Adding a timer is an explicit
new kind rather than overloading command provenance. `wait::resolve_wait_event_sources`
already validates at arm time; `WakeSources::may_share` uses origin to keep invocation
sources local. The timer must extend those exhaustive matches without changing scope.

The monitor's existing `reconcile_hosting` runs on watched config/refold changes and
uses an author-bound sink; its source receive branch derives durable work from logs.
`EventSources` owns cancellation/join, and `EmissionSink` now performs acceptance and
metadata mapping synchronously. These provide the host and custody seams without a
new daemon. Existing `EventLog` append/replay/ack, `IngestLease` and durable producer-ID
retention provide concrete integration points; implementation must resolve timer
retention in milestone 1 without retaining a second completion ledger.

The plan is ready for introduction review as a bounded, additive one-shot feature.
The source-change and clock-jump limits above are intentional. No receipt feature,
provider installation or existing configuration migration is included. Milestone 1
must record exact projection/transport/retention choices before implementation; no
automatic promotion of this proposal into a supported installed interface is implied.

## Milestone 1 — occurrence, custody and ownership contract

Assessed at `2c655a7c8a2a`. The introduction review identified two real gaps: the
ordinary WAL identity horizon cannot suppress a still-configured occurrence forever,
and `monitor::owed_from_sources` discovers only sources still declared in config.
This milestone specifies their solutions before a scheduler is implemented.

### Evidence and boundaries

- `core/agent_config.rs::WaitEventSource` and `CommandSource::source_key` own pure
  config/schema/identity; parsing clocks/files belongs in the CLI. The CLI already
  depends on `time` with RFC3339 parsing/formatting support.
- `wait::resolve_wait_event_sources` validates before arming. `WakeSources::may_share`
  keeps invocation sources local, while `monitor::hostable` and `reconcile_hosting`
  own author-configured source startup and replacement. `EventSources` currently
  owns task abort/join; native timer state must be owned alongside those tasks.
- `command_source::Seen` remembers all ingested producer IDs beyond the WAL horizon.
  `apply` appends before indexing before presenting. Its index is currently
  best-effort; `Seen::recover` is fed the capped `LoadedLog.feed_ids`. Reusing that
  capped recovery and permitting timer compaction after index failure would still
  allow an old acked timer to refire. Timer checkpointing must be stricter.
- `EventLog::load` restores sequence allocation and owns quarantine. `read_rows`
  is a read-only inspector, not sufficient preparation for append. WAL payload/ack
  records stay authoritative. `maybe_compact` retains unacked rows but ages ordinary
  identities. Migration-only pinned keys deliberately describe a finite historical
  inbox; timers must not borrow that special case.
- `owed_from_sources` uses live config keys; `events::read_all` already discovers
  logs independently for inspection. `wait::items_for` currently reads messages
  for the initial/peek projection. Timer backlog discovery must reach this projection
  and both shared/local waits without requiring its source definition to survive.
- `WaitItem` is a serde event/wake shape, not a member of the wincode fold cache.
  `WaitKind` has one macro vocabulary and exhaustive classification; `event_view`
  feeds common CLI/TUI/web event presentation. Monitor `Note::Items` carries typed
  core items, whereas the hook tolerates unknown tags in its subprocess wire.

These are static reads/reference traces, not new executed test evidence. No native
timer, schema change, protocol bump, config write or installation exists yet.

### Public config, timestamps and identity

The config remains the proposed `{kind:"timer", name, due_at, content?}`. A `TimerSource`
contains strings/optional content only; a CLI `ParsedTimer` supplies the validated
absolute instant and scope. Reject whitespace-only names, invalid/missing offsets,
times before the Unix epoch, leap seconds and nonzero fractional seconds. V1 uses
whole-second deadlines to match the existing event timestamp domain; `.000` denotes
the same instant as no fraction. Do not round a nonzero fraction and silently fire
early. Reject deadlines that cannot form a checked monotonic deadline on this host.
Document these explicit restrictions. Existing command/GitHub config stays unchanged.

Normalize accepted `due_at` to Unix seconds, independent of offset spelling. Identity
is the tuple `(repo, author, resolved storage key, occurrence ID)`. Within a timer
source the occurrence ID is `timer/<unix-seconds>`; it need not be globally unique,
just as command IDs are unique within their source. Content and arm/session time are
not identity. Configured storage keys use a distinct `timer-config-<slug>-<full-name-hash>`
namespace; invocation keys use `timer-invocation-<slug>-<full-name-hash>-<invocation>`.
Hash the entire name, not only its lossy slug, and preserve the original name in the
payload. Validate duplicate keys/names within one scope, including detected collisions.
Config and invocation definitions with equal names remain distinct. Namespace helpers
own this spelling; discovery must not infer origin by searching arbitrary name text
for `-inv-`. Do not change existing command/GitHub storage keys.

One `--event` wait call mints its invocation identity once, before any timer acceptance;
retain it through that wait's sharing decision/fallback and cleanup. A separate call
is a new invocation and may fire an equal timer again. Invocation isolation is not a
promise to replay an earlier call's timers automatically. Old invocation rows remain
inspectable/ackable by qualified selector, but cannot wake another invocation.
Configured timer restart/catch-up reuses its author key and occurrence ID.

### Event and wire shape

Add `WaitItem::TimerEvent { name, id, due_at: u64, content: String, selector: Option<String> }`,
`WaitKind::TimerEvent` with wake tag `timer_event`, and `Transport::Timer`. Missing
content becomes empty; the WAL retains supplied content rather than imposing new
lossy truncation. Shared renderers retain their existing escaping/control handling.
The common event view exposes kind `timer`, source name, body/content, due timestamp,
and the presented selector. Timers have no reply route or required-answer flag in v1;
ordinary event ack handles them, and `--reply` must refuse unsupported timer replies.

Append the complete event through `EventLog` with its occurrence ID as `feed_id`,
`event_at = due_at`, normal actual ingest time in the envelope, and Timer transport.
Stamp `<source>@<seq>` only after append/replay, as for command selectors. Provide
typed wait JSON, hook hint/ack recipe, event list/show, TUI state/body/time and web
timeline support through the common view. A deadline is not workflow review work;
existing block presentation treats it as independently owed event work.

WAL record semantics/version stay v1: new payload/transport tags are additive, and
old readers preserve uninterpretable records as foreign with their sequence envelope.
Timer logs occupy a new source namespace, so old source drivers cannot compact them.
The fold cache stays format 15: no folded data layout changes. Bump monitor PROTO
7 to 8 when timer hosting/wake support lands. A new client must not trust a proto-7
host that cannot parse/host timers; use the existing explicit handshake/lease handoff,
never retire an owner merely on observing a descriptor version. Old binaries still
cannot use a timer config: install a supporting binary before enabling new definitions.

### Durable acceptance and persistent deduplication

Extract `Seen`/`Known` to a neutral source-ID module with actual command and timer
consumers; preserve command index filenames and behavior. It records ingestion, not
handling. Do not add a timer completion store. Keep one source WAL plus the established
ID index; use the existing per-source `IngestLease` for the acceptance critical section.
Native timers need that lease only during due ingestion/checkpointing, not an open
lease descriptor or producer per future definition.

For each due occurrence:

1. Take the nonblocking source ingest lease. Busy means another owner can ingest;
   retain eligibility and read the authoritative log on existing owner wakes.
2. Load/restores the WAL allocator before append. Read all identities still retained
   in its primary/observation records through the WAL's existing parser, with an
   uncapped inspector/checkpoint helper. Do not use capped `LoadedLog.feed_ids` as
   the sole repair evidence or introduce a second JSONL parser.
3. Reconcile that evidence into the source-ID index. If the index or WAL already
   accounts for this ID, do not append it again; pending payload/ack state comes
   only from its actual WAL row. Its original content/time remain immutable.
4. For a genuinely new ID, append first, then persist the ID index, then send the
   logged emission synchronously to the owning sink. Never index an unappended
   event, and never emit an append failure as if it were accepted durable work.
   A closed sink leaves the appended event owed in the WAL for the next reader.
5. Index failure after append does not erase that event or turn it into a transient.
   Keep WAL evidence and mark checkpoint repair pending; do not append a duplicate
   while the retained WAL still proves ingestion. The index's in-memory tier alone
   is not a successful durable checkpoint.
6. Timer compaction may drop historical ID evidence only after every retained
   source ID is successfully persisted. Gate the timer-owned compaction path on
   that strict checkpoint. No other production actor compacts these timer logs.
   If checkpoint fails, skip compaction and report it; preserve recoverability
   until the next existing owner wake/rearm can repair it. Command/GitHub policies
   are not silently strengthened or changed by this feature.

This covers the append/index crash gap and successful compaction beyond 1,000 IDs.
Timer corruption or foreign semantic records are fail-closed for automatic ingestion:
preserve/quarantine evidence and diagnose rather than clearing the index and refiring
a possibly completed timer.
A damaged index entry with no remaining WAL proof is ambiguous, not evidence of a
new occurrence. Require repair instead of silent replay. The normal no-refire promise
does not cover manually deleting/corrupting all custody evidence or new power-loss
guarantees beyond the existing WAL's durability. Tests must exercise these distinctions.
Indefinite timer ID retention has the same disk-growth tradeoff as command IDs; this
plan does not claim unlimited schedule history has bounded total storage.

### Backlog discovery independent of a live definition

Introduce one timer backlog reader shared by monitor presentation and the local/initial
wait projection. It discovers only the configured timer namespace for that author,
plus the exact current invocation timer keys when applicable; validate parsed rows as
Timer events with Timer transport. Do not glob all invocation logs into author work.
Inspect without acquiring ingest leases, quarantining or indexing. Qualified row/ID
keys retain per-client shown suppression and grouping; acknowledged rows are excluded.

The shared monitor must invoke this reader even when `hostable` is empty, including
after restart with no timer definitions. The normal wait's initial projection and
`--peek` include accepted configured timer backlog alongside messages/workflow work.
Peek only reads existing accepted rows: it never fires a past-due definition, starts
a source or mutates a log/index. Source removal cannot delete or hide accepted work.

Normal wait arming must catch up declared due timers before its initial return, even
if workflow work is already ready. Build the native timer scope and owning emission
queue once after identity/config validation; process due definitions without launching
ordinary command/GitHub drivers early. Merge the accepted timer rows through the same
presentation path and binding guard, then retain future timer state if the wait parks.
Do not postpone native catch-up forever behind continuously actionable plan work.
Initial/stateless projection scans once; a long-lived owner then retains a derived
in-memory inventory of timer logs with pending work. Timer append and existing watcher
notifications invalidate affected entries. Reuse the existing repository watcher and
carry dirty timer-WAL keys/cause information where necessary; do not add a second
watcher. Overflow/unknown relevant changes require an explicit rescan. No new full
directory/WAL scan is run on every healthy native-mode heartbeat. Existing explicit
poll-mode fallback may refresh on its existing cadence, with that cost documented.
At admission, validate pending candidates against their WAL rather than trusting a
possibly stale cached ack state. This inventory is a disposable read cache, not a
completion authority or another persistent ledger. Physical/logical grouping must prevent
the same accepted row reaching a returned batch through both the sink and backlog scan.

### Native scheduling, edits, failure and cost

Use passive `TimerSet` state owned by the existing monitor/local wait source lifecycle,
not a task per timer or a separate forwarding service. The monitor keeps one ordered
deadline selection across its hosted authors; a local wait keeps one across its own
configured and invocation scopes. A persistent absolute monotonic sleep participates
in the owner's existing select loop. Traffic must not restart a relative countdown.
There is no extra timer task, thread, command process or per-definition idle lease.

At a deadline, recheck wall time and ingest ready entries in stable deadline/key order.
Accepted/known IDs leave the pending set. Zero pending entries means no timer branch.
Config replacement rebuilds the affected author's pending definitions; unchanged
specs do not restart healthy sources. Canceling/dropping native state cancels its
future deadlines without changing any accepted WAL row. Configured monitor edits
take effect when observed; local/CLI scopes retain arm-time definitions. That distinction
is intentional and testable, not a claim of cancellation atomic with external file writes.

Compute a checked monotonic deadline when a definition is armed. Backward wall-clock
movement at expiry re-arms the actual remaining delay instead of firing early. Forward
clock changes are observed at the originally armed deadline or definition re-arm; no
new periodic clock check is introduced. Suspend/resume behavior follows the host's
monotonic timer and wall-time recheck, with platform limitations recorded explicitly.

Busy leases and failed append/checkpoint attempts stay pending, with bounded diagnostics
on a failure-state transition. Retry only on an already-existing owner repository wake/
heartbeat, rearm or config reconciliation, at most once per pending key per such wake.
Never leave a zero-duration due sleep enabled for failed entries. Healthy future timers
perform no scheduler I/O/clock checks on heartbeats; the owner tests an empty recovery set
and continues. This adds no timer-specific polling cadence or self-scheduled retry loop.
Kernel contention is not permission to let two appenders allocate independently.

No running monitor/armed wait still means no provider-launch promise. Persisted
definitions catch up at the next normal arm. Roster removal stops future hosting;
stored work remains in that author's inbox and is not reassigned merely because the
master role changes. Event receipt/completion semantics and Anx routing remain separate.

### Implementation proof obligations and milestone disposition

Milestone 1 is complete as a contract/evidence revision. Implementation is milestone 2,
not an untested assertion that this scheduler exists. In addition to the original tests,
drive strict index-repair failure with more than the WAL identity cap before recovery,
compaction/rearm without refire, restart after definition removal, zero live timer
definitions with pending rows, normal initial catch-up versus side-effect-free peek,
current/foreign invocation isolation, source lease contention/owner death, mixed old/new
monitor handshake, config cancellation at its actual observation boundary, and deadline
stability under unrelated client traffic. Test that errors cannot hot-loop and healthy
future timers add no heartbeat I/O or extra task/producer count. Count outcomes through
real source/wait/monitor/ack paths, not only through helpers.

No tests were run for this documentation-only milestone. Consumer/schema/custody reads
and `git diff --check` are the validation here. Core/CLI and relevant feature/boundary
checks belong to the implementation milestones; Linux, live provider and real-clock
behavior are not claimed from these static traces. Commit this contract for review
before editing the timer implementation.

## Milestone 2 — native source integration

Implemented the additive `TimerSource`, `TimerEvent`, `timer_event` wake vocabulary,
common event view and Timer WAL transport. The typed wait JSON and hook use the
actual timer fields and qualified ack selector. Hook hints collapse controls in
operator-supplied text; stored content stays lossless. Normal CLI acknowledgement
works; an answer to a timer is refused because it has no reply backend. Monitor
PROTO is now 8; WAL v1 and fold-cache format 15 are unchanged. No binary was
installed and no agent's live configuration was changed.

`timer_source::TimerSet` owns passive deadline and failure-recovery state. Local
wait and monitor select loops wait on the nearest absolute monotonic deadline;
unrelated traffic does not reset it. A monitor selects across its author hosts.
Timer hosting starts no task/process and holds no future timer's ingest lease.
Deadline expiry rechecks wall time; backwards movement rearms instead of firing
early. Failed due entries leave the deadline set, retrying once on an existing
owner wake rather than enabling a zero-duration sleep. Reconciliation cancels old
definitions before retrying unchanged ones, and newly built sets do not receive
an immediate second attempt. Timestamp parsing itself reads no clock: unchanged
healthy hosts do not gain timer clock checks during heartbeat reconciliation.

The command index implementation moved to `source_ids` with its existing paths
and best-effort recovery semantics intact. Timers additionally use strict durable
checkpointing. `EventLog::retained_timer_ids` uses the existing WAL parser without
the cursor identity cap and refuses corrupt/foreign semantics before `load` can
reset custody. Acceptance takes the existing nonblocking ingest lease, restores
allocation, checkpoints retained IDs, appends, indexes, then synchronously signals
the owning sink. An index failure after append still publishes logged work and
keeps repair eligible. Timer compaction runs only after the retained ID checkpoint
and the new ID write succeed. No migration pin or completion ledger was added.

`timer_source::Backlog` discovers configured timer logs independently of current
definitions, plus only the calling invocation's exact keys. Its derived inventory
keeps pending keys and validates their ack state against the WAL at presentation.
It does not cache failed directory reads as an empty inbox. Initial/stateless
presentation discovers once; long-lived monitor presentation reuses the inventory.
The existing repository watcher carries a timer-WAL/directory-change dirty bit;
timer signals also invalidate inventory. Invalidations conservatively rescan known
author inventories rather than adding another watcher. Healthy empty inventories
do not scan on native-mode heartbeats. Explicit local poll mode retains its existing
refresh cadence and may rediscover on those wakes. The repository watcher's existing
500 ms PollWatcher baseline and permanent source-ID disk-growth tradeoff remain;
this milestone does not claim to eliminate those existing costs.

Normal waits mint invocation identity and build native state before the initial
workflow return. Due catch-up therefore occurs even with ready workflow work,
without launching ordinary command/GitHub drivers early. `--peek` only discovers
accepted configured backlog and does not arm, append or index timers. Local return
merges native signals through authoritative replay so one row cannot arrive through
both the sink and discovery. Removing a definition leaves accepted work owed.

### Executed validation

- Ten new native source tests cover timestamp restrictions/equivalent offsets,
  name/scope isolation, duplicate definitions, wall-clock recheck, unchanged
  deadlines, ordered late catch-up, immutable accepted payloads, ack/rearm,
  contention and recovery, failed index writes after append, strict checkpoint
  repair of 2,100 IDs beyond WAL retention, corruption/foreign/ambiguous evidence,
  definition-independent replay, invocation isolation, cancellation and closed-sink
  custody.
- Two new monitor lifecycle/presentation tests prove passive hosting, stable
  deadlines on unchanged reconciliation, future-definition cancellation, accepted
  work after definition removal, independent delivery to two admissions and durable
  ack validation through cached inventory. These drive hosting/presentation helpers,
  not a real monitor process with a future deadline.
- A new hook test consumes actual typed wait JSON and verifies due provenance,
  sanitized hints, lossless wire content and the ack recipe.
- Three new integration tests drive a real parked local wait, initial due catch-up
  alongside ready queue work, side-effect-free timer peek, removal/replay, unsupported
  reply refusal, CLI ack/no-refire, and distinct invocation custody.
- Final affected CLI unit run: **854 passed, 0 failed**, eight test threads. A first
  default-concurrency run had one existing command quarantine fixture fail because
  it could not verify a previous process group; its isolated rerun and the complete
  final affected run passed. No command process-ownership policy was changed.
- Affected integration/config-reload/Git/path/config-write boundary run:
  **29 passed, 0 failed**. Core config/wait checks: **107 passed, 0 failed**.
  All-target/all-feature CLI compilation and `git diff --check` passed. The compile
  retains the existing 20 CLI test warnings; no production warning was introduced.

This is source-tree implementation on macOS. Milestone 3 still owes supported
configuration/installation documentation and further driven shared-monitor deadline,
takeover/config replacement, watcher invalidation and timer-specific TUI/web scenarios.
Existing mixed-protocol/non-retirement tests passed in the affected monitor suite,
but that does not substitute for a new real timer-host process scenario. Linux,
actual wall-clock adjustment/suspend/resume, live providers and resource benchmarks
have not been exercised. Commit this integration for review before that final pass.

### Review revision — rejected timer edits preserve accepted sources

The final review found that `reconcile_hosting` stopped the current host before
constructing a replacement timer set. A malformed deadline or duplicate timer name
therefore left the author's command and GitHub sources stopped until another config
edit. Invalid source configuration is now an error state distinct from an intentional
empty configuration: reconciliation logs the rejected edit and retains the complete
previous host, including its timer deadline and the source specification under which
its already-accepted WAL rows must be presented. It retries the retained timer set
only on an existing owner wake. A valid empty configuration still removes the host.
For valid nonempty replacements, all timer validation/construction now succeeds
before any old task is stopped, so replacement remains prepare-then-swap.

The regression test exercises both an invalid deadline and duplicate timer names
while a prior host has command and GitHub drivers, due-timer state, and accepted rows
in both source logs. It verifies task identities and guards survive, the old accepted
source names/instructions still govern presentation, and a subsequent valid empty
configuration cleanly stops and joins both drivers. This tests lifecycle custody
without depending on launching external source processes.

Validation for this revision: the focused regression passed. The full monitor unit
suite ran 136 tests: 104 passed and 32 failed because this sandbox denies local
socket operations (`PermissionDenied: Operation not permitted`) used by unrelated
monitor integration tests. A separate command-process variant was attempted and
removed because this environment cannot provide the OS process-start evidence that
the command source deliberately requires; it is not used as evidence for this fix.
`cargo fmt` and `git diff --check` are run before committing this revision.

## Milestone 3 — supported configuration and driven delivery

README now documents the exact timer JSON, timestamp restrictions, occurrence
identity, definition edits/removal, ownership across role changes, invocation scope,
peek/ack semantics, offline catch-up, monotonic/wall-clock limits and resource model.
It provides the source-install command and requires upgrading every client/monitor
host before adding timer definitions. No binary was installed or live agent config
changed by this milestone; the previously installed release binary must not be
represented as supporting this feature merely because this checkout does.

Driving timer presentation found a genuine missing integration: the merged timeline
classified every non-GitHub/non-message row as Command. Timer WAL transport now maps
to a distinct Timer backend and namespace. Log JSON emits `timer_event` with the
due timestamp, ordinary timeline timestamp, source, content and handled state. The
shared timeline badge and TUI detail page name its origin `timer`; web snapshot
serialization carries the same backend and member transport. This is an additive
presentation tag, without changing fold-cache encoding or timer custody.

Two permanent regressions start from real Timer WAL rows. They drive timeline
projection, shared oneline rendering, log JSON, serialized web event state and TUI
detail rendering before and after ack. They verify due-time ordering, lossless
stored multiline content, terminal sanitization, Timer provenance and handled-state
refresh. The existing browser terminal renders the same TUI output; an independent
live browser session was not exercised.

### Executed validation

- A debug binary was built and run in a fresh temporary Git repository through real
  `clank monitor start` processes and protocol-8 Unix-socket clients. Two simultaneous
  agent clients received the same future-deadline occurrence/selector, after its due
  instant. Restart presented that selector again with exactly one WAL event row.
  CLI acknowledgement retired it. Adding an external timer WAL after an admitted
  client's inventory became empty invalidated discovery and delivered the new row.
  Editing a live future definition replaced its deadline and delivered the new
  occurrence. An explicit higher-version handshake cleanly retired the holder; a
  successor process accepted its still-pending future timer at the original deadline.
  This uses a synthetic protocol-9 client, not a claim that a released proto-9 binary
  was tested. The temporary driver is `/private/tmp/clank-timer-monitor-smoke.py`.
- With local sockets/process inspection permitted, the affected monitor, timeline,
  log, TUI rendering and repo-watcher suites passed: **337 passed, 0 failed**. This
  resolves the sandbox-denied real-socket checks recorded in the preceding revision.
  After adding the ordinary `ts` field to the new timer log DTO, its focused WAL/log
  projection regression was rerun.
- All-target/all-feature CLI compilation passed; formatting and diff checks passed.
  Existing test warnings remain; there is no new production warning.

The three milestones are implemented. Resource behavior is established by owned
deadline state, zero timer tasks/producers, unchanged-deadline tests and the shared
monitor path; this is not a measured CPU/RSS benchmark. Linux, actual wall-clock
adjustment, suspend/resume and live provider sessions were not exercised on this
macOS run. The documented observation limits remain part of the public contract.
Submit this final milestone to the gate; only a FINISHED verdict permits finalization.

### Review revision — terminal rows project a safe single-line summary

The final presentation review caught that `gh_describe` inserted the timer's raw
multiline content into the shared terminal row. The detail page was sanitized, but
the human log, oneline log and TUI timeline could still receive a newline or terminal
escape through that row. Every text field in the shared description now uses the
existing `text::one_line` projection before interpolation, including source, actor,
kind, detail and content. It strips escape sequences and controls and takes the first
line; the quoted summary therefore stays closed and cannot occupy another terminal
row. No WAL payload, event view, JSON content or full detail body is rewritten.

The real-WAL regression now includes CSI escapes, tabs, carriage returns and a
multiline timer body. It checks the exact single-line summary, absence of controls
and escape fragments, all other hostile text fields, handled-state refresh, and
lossless JSON content. The existing full-detail WAL test still verifies that later
body lines remain available and safely rendered. The affected log/TUI rendering
suites passed: **178 passed, 0 failed**. Formatting and diff checks passed.

### Review revision — event inspection shares the safe summary contract

The subsequent review identified the same raw-content assumption in `events::describe`,
the list and show summary. That description now applies the existing single-line
projection to all event text fields. `events show` decides whether to render the
full body by comparing its sanitized text with the displayed content summary,
rather than comparing the two raw stored fields. A timer whose body and content
are identical therefore still exposes its later lines under the full body; a plain
single-line body equal to the summary is not duplicated. WAL and JSON remain lossless.

A real-WAL inspection regression verifies safe source/content summaries, terminal
control filtering, later body lines, unchanged JSON and the plain-summary case.
The affected event-inspection/log suites passed: **48 passed, 0 failed**.
Formatting and diff checks passed.
