# a-full-pass-for-quality-after-the-simplifications
# A full pass for quality, now that the simplifying is done

## Why

The last several plans have been large and mostly SUBTRACTIVE — one watcher per repo, one
host for an author's sources, one presentation boundary, one signal module, one plan at a
time. Each landed under review and each left the codebase smaller in the places it touched.
What none of them did is look at the whole thing afterwards.

That is the gap this plan is for. A sequence of local simplifications does not add up to a
coherent whole on its own: each one is judged against the plan it belongs to, and the
residue — a model that is now duplicated because two plans arrived at the same idea
independently, a responsibility that stayed where it was because moving it was out of
scope, a requirement that was taken literally and pushed a special case down into the core
— is exactly what no single plan's review can see.

This is a REVIEW plan. Its deliverable is findings plus the changes they justify, not a
feature. It is expected to delete more than it writes.

## The five questions

Asked of the whole repo, not of a diff. Each one gets a written answer naming files and
lines, and each finding is either fixed here or recorded with a reason it is not.

**1. Architecture smells.** Reconciliation passes, duplicated sources of truth, post-hoc
cleanup, special-case patches. The repo's own standard: if several local bugs share a
cause, name the missing or incorrect MODEL rather than listing the symptoms. Look for
state that is maintained in two places and then compared, for anything that "fixes up" a
value after the fact, and for correctness that rests on an ABSENCE nobody named.

**2. Duplicate models and duplicate implementations.** Two types that mean the same thing,
two functions that answer the same question with different answers, the same rule spelled
out at N call sites. There is precedent: three near-identical liveness checks with two
different readings of `EPERM`, and a producer-identity record that reinvented `ProcToken`
as an `lstart` string from `ps`. Both survived because nobody looked across modules. Look
across modules.

**3. Drift in the core rollup.** `crates/core/` is the model everything else projects from,
and it is the place a hasty requirement does the most damage. Has a UI need, a CLI
convenience or a one-off workflow pushed a field, a variant or a special case into the core
that does not belong to the DOMAIN? Anything in `repo_state` / `wait` that exists to make
one renderer's life easier is drift. Judge each public field by whether the model would
still need it with every consumer deleted.

**4. Code that can be deleted.** Dead code the compiler cannot see: a field nothing reads,
a variant nothing constructs, a function with only test callers, a configuration knob
nobody sets, a compatibility path for a version that no longer exists, a test that asserts
something the type system now guarantees. The descriptor's `fold` stamp was exactly this —
rewritten to disk every five seconds for a reader that did not exist — and only a direct
question found it. Assume there are more.

**4a. One known finding, already diagnosed — the source pipeline's extra hop.**
`EventSources::spawn` builds a private per-author channel, and `tagged_into` moves that
receiver into a FORWARDING task whose only job is to re-send each emission with the
author attached. So every hosted source's events cross two queues and one extra task.

It is also lossy on cancellation: an emission accepted into the private queue before the
forwarder next runs is destroyed when that task is aborted, and awaiting the handle proves
the task died, never that its buffer was transferred. The monitor's shutdown can therefore
report nothing outstanding while an accepted transient has gone — and a transient has no
WAL row to replay from.

The fix is to delete the hop rather than guard it: give the drivers an emission SINK
supplied by whoever spawns them, so the monitor's drivers bind the author and write
straight into its one queue. One queue and one task per author disappear. It touches
`command_source.rs`, `wait.rs`, `pr_review.rs` and both github modules, which is why it
belongs here and not inside a plan about protocol versions (codex on 1f7f7a7; attempted
there and backed out as scope creep).

**5. Centralization: is everything on the right peg?** Specifically the monitor: it owns
the repo's watcher, fold and config sources now, so what ELSE is still being done per
client that it should own, and what is it doing that belongs to a caller? The same question
for `git_io` / `git_plumbing` (the boundary is enforced, but is everything that should be
behind it behind it?), for `crate::signal`, for `agent_store`, for `stop_hook` versus
`wait`. A responsibility in the wrong place is usually visible as a caller that has to know
something it should not.

## How to do it so it is not a vibe

- **Per question, state the method before the findings** — what was grepped or read, and
  what would have been missed. "I looked at everything" is not a method and a zero count is
  not a report.
- **Rank by what the finding would cost**, not by how easy it is to fix. A duplicated model
  that lets two modules disagree outranks a dead field.
- **Lead with the model when symptoms share a cause**, per the repo's review standard.
- Prefer deleting to trimming. Prefer a type that makes the mistake unspellable to a check
  that catches it.

## Acceptance

- A written answer to each of the five questions, naming files and lines, with the method
  stated.
- Every finding is fixed, or recorded with the reason it is not and what it would take.
  "Recorded" means in this plan's text, not in a comment nobody will find.
- Deletions are driven: a thing removed because "nothing reads it" has a stated search that
  would have found a reader.
- No behaviour change that is not deliberate. This is a quality pass; anything that changes
  what clank DOES gets said out loud and justified, or split into its own plan.
- The full test suite passes, and any test removed as redundant says what now guarantees
  the property instead.

## Out of scope

- New features. If a finding wants one, it becomes a queued plan rather than growing this.
- Performance work that is not also a simplification.
- Reformatting, renaming for taste, or comment churn. The no-WHAT-comments rule applies to
  what this plan writes, and stripping existing violations is welcome, but a diff that is
  mostly comment edits is not this plan.

## Watch out

- A review plan can expand without limit. The five questions are the scope; a finding
  outside them is queued, not absorbed.
- Some duplication is right. Two crates cannot share a `pub(crate)` type, and history is
  legitimately a collection. The burden is to say which kind each one is.
- Do not undo a deliberate separation because it looks like duplication. Several of these
  were argued through review already, and the reasons are in the finished plans.

## Execution contracts and stopping condition

The five questions above define a finite audit. Inventory the production modules and
consumers first; record the revision reviewed, the searches and reads performed, the
coverage limits, and the relevant earlier decisions. Complete that inventory once rather
than restarting the audit after each fix. New evidence can revise a finding, but does not
silently enlarge the scope.

Each finding must name the model or ownership boundary, the concrete failure or cost,
the supporting producer/consumer search, its severity, and its disposition. State the
intended contract before implementing a change. A deferred finding needs a reason and a
concrete next step; a feature-sized change belongs in a separate queued plan. Preserve
deliberate separations unless evidence establishes that their original reason no longer
applies. Written findings and dispositions live in this plan, under the five questions.

For the known source-pipeline finding, promotion-time inspection confirms the extra hop
still exists in `cli/wait.rs::EventSources::tagged_into`, called by
`cli/monitor.rs::reconcile_hosting`. The implementation must establish these contracts:

- Drivers receive a caller-supplied emission sink. The monitor binds the author before
  an emission enters its existing shared queue; an invocation retains its own delivery
  scope. Author attribution and durability travel with the emission without a forwarding
  task or intermediate per-author queue.
- A successful send means acceptance by the owning queue. Stopping and joining a source
  cannot destroy an already accepted emission in a task-private buffer. The existing
  shutdown drain must still see that emission, including a transient without a WAL row.
  This does not promise delivery to an absent client or add persistence for transients.
- Durable replay and acknowledgment remain WAL-authoritative; transient retention,
  shutdown diagnostics, source leases, producer cleanup, and cancellation ownership keep
  their existing contracts. A closed receiver is handled explicitly, without a retry or
  task spin. Test acceptance immediately followed by cancellation with deterministic
  synchronization, rather than sleep-based timing.
- Resource use is an acceptance criterion for every change in this audit: no new
  periodic polling, idle worker, per-client producer, process scan or unbounded retry
  introduced to maintain consistency. Removing the forwarding hop removes one queue and
  one task per hosted author. Existing source polling is not redesigned incidentally.

## Implementation milestones

1. Complete and commit the written five-question audit, with methods, evidence, ranked
   findings and dispositions. Read the existing ownership boundaries and the relevant
   finished plans before proposing to merge models. Identify any intentional behavior
   change explicitly. This is the point for reviewers to challenge the diagnosis before
   implementation.
2. Remove the confirmed source forwarding hop through the supplied sink contract, with
   targeted source, wait, monitor and shutdown tests. Prove author isolation, durable
   replay, accepted transient retention across cancellation, and producer cleanup. Keep
   the change within the existing event semantics.
3. Apply the other agreed, justified fixes in coherent ownership/model milestones, each
   with its relevant checks. State a reader/constructor search for every deletion and
   the replacement guarantee for every removed test. Do not create cosmetic cleanup
   commits or a commit for an empty finding category.
4. Reconcile every finding's final disposition and run the full repository test suite
   required above plus relevant boundary checks. Separate actual failures from fixture
   environment restrictions, resolve what can be resolved, and report concrete limits
   rather than claiming unrun tests passed. Stop when the five answers, agreed fixes,
   dispositions and validation are complete; do not search indefinitely for more work.

## Milestone 1 audit — baseline 2802b15c81ea

This is an ownership and consumer audit, not a claim to have read every line or proved
every platform. The inventory contains 148 Rust source/test files and 166,623 lines:
core 15/10,994; CLI 119/148,178; web 6/2,713; web-cloudflare 8/4,738. Test bodies are
included. Prefixes before the first conventional test module were used only to prioritize
reads, not to classify production code reliably: several files have other test modules.
Apparent Git-layer violations in status/stash prefixes were checked and were test fixtures.

The inventory covered module declarations, public/shared types and their repository
references, owning queues/tasks, constructors and consumers, persistent schemas, source
drivers, fold/work derivation, cache/rebuild, agent/config/session storage, process/signal
boundaries, TUI workers, web bridge/feed/replay, and command/integration fixtures. Deep
reads concentrated on state transitions and acceptance/cancellation. Searches included
Rust, checked-in browser assets, tests, manifests, documentation and earlier plans; test
references were distinguished from production consumers. A low symbol count was a
candidate generator, never sufficient evidence to delete a symbol.

Limits: no external Rust-library users were surveyed; vendor JavaScript was not audited;
this milestone does not exercise real GitHub/provider/network behavior or execute Linux
code. No suite was run for this documentation-only milestone. Subsequent implementation
must recheck affected consumers, compile feature-gated paths and run the final suite.
Line references below name this baseline, before deletions move them.

### Ranked findings and dispositions

P1 denotes accepted-work loss; P2 denotes a structural correctness/resource risk or a
substantial unused implementation; P3 denotes smaller maintenance residue.

| ID | Rank | Model/failure | Disposition |
| --- | --- | --- | --- |
| F1 | P1 | Acceptance occurs in a queue the shutdown owner cannot drain; both author tagging and GitHub prompt stamping forward later | Fix in milestone 2 through one supplied sink, removing both hops |
| D1 | P2 | Unbounded ingress and absent-client transient retention have no overload policy | Record; separate capacity/policy plan after this audit, without inventing drops here |
| D2 | P2, latent | Public work-state fields can spell an actionable state without its required SHA | Record; separate typed-state change preserving wire semantics; current derivation is valid |
| F2 | P2 | An unused runtime, classifier and event/diff DTO family maintain an alternative architecture | Delete together in milestone 3; retain live fold/wrapper and preview DTOs |
| F3 | P2 | Legacy file-message writing/acknowledgment survives only through tests beside WAL delivery | Delete old writer/API; retain migration and replace obsolete fixtures |
| D3 | P3 | Two parsers describe one configuration envelope, but intentionally have different validation behavior | Retain now; any unification must preserve repair of invalid rosters |
| F4 | P3 | Unread admitted-client data, unused helpers and test-only conveniences remain in production | Delete or confine to tests as detailed below |
| D4 | P3 | A legacy warning variant has no current production constructor but is cache-encoded | Retain until an intentional schema/cache transition; do not silently reinterpret cached variants |

The core diagnosis is narrower than “everything needs a monitor.” The active design has
useful ownership boundaries. Its remaining event bug comes from an unnamed acceptance
boundary, and its largest duplication comes from obsolete implementations with no live
consumer. Repair those models once, then delete their residue. A new cancellation guard,
background reconciliation loop or retry around each forwarding task would preserve the
cause and add resource use.

### 1. Architecture smells

**Method.** Trace a source from configuration/lease acquisition through emission, owning
queue, presentation, WAL replay/ack and shutdown. Read `wait`, `monitor`, command source,
GitHub supervisor/coordinator and draft source, including their cancellation tests. Trace
other alleged sources of truth through rebuild/cache and web retained state. This checks
custody and derivation statically; it does not measure overload or scheduler frequencies.

**F1 — the acceptance owner is missing from the API.** `cli/wait.rs:1147` defines
`EventSources` with a private receiver; `spawn` at 1196 allocates that queue. `tagged_into`
at 1173 creates a forwarding task to add an author. `cli/monitor.rs:2691` calls this from
`reconcile_hosting`; its actual shared source queue is at 2403. Stopping `Hosted` aborts
its task handles. `stand_down` at 3283 closes/drains only `from_sources` at 3332–3333.
An emission already accepted by a driver's private sender can therefore disappear before
the forwarder runs, outside both the final presentation and undelivered-transient count.
Joining an aborted task proves termination, not transfer of its buffer. Logged events
retain WAL recovery; transients have no such recovery.

There is a second instance of the same cause. `cli/github/supervisor.rs:66` enters
`run_github_source`, creates `itx/irx` at 78 and joins the supervisor with
`stamp_instructions_forwarder` at 132. That future stamps the prompt and forwards to the
supplied sender. Aborting the parent drops both joined futures and that private queue.
A monitor-hosted GitHub emission currently crosses the prompt queue, the author's queue,
then the monitor queue. Fixing only `tagged_into` leaves the same custody bug in GitHub.

The implementation contract for **both** transformations:

1. A source gets a cloneable synchronous emission sink supplied by its delivery owner.
   Its `send` performs metadata transformation and enqueue in the same call. It creates
   no queue, worker or asynchronously scheduled forwarding future. The sink may wrap an
   existing sender and a synchronous mapping operation; it must return the rejected
   emission on closure rather than conceal failure.
2. The monitor binds the author into its sink before spawning drivers. A successful send
   means the attributed emission is already in the monitor's existing shared queue.
   Single-invocation waits supply their own queue/sink and keep invocation-only sources
   and draft-review scope. They must not borrow another author's monitor host.
3. GitHub prompt stamping composes synchronously with that sink. The prompt remains
   presentation metadata, absent from WAL payloads; replay still rejoins current source
   configuration. Coalescing/routing/deduplication are unchanged.
4. Backend-neutral `Emission`, `Durability` and `SourceOrigin` belong with this sink
   contract, not in command source and GitHub supervisor respectively. Currently
   `command_source.rs:582–594` defines emission/durability while all backends depend on
   GitHub's `SourceOrigin` at supervisor:56. A small neutral event-source module removes
   that dependency inversion; it must not become another registry or lifecycle manager.
5. Closed-sink behavior must remain explicit at every producer. A failed enqueue is not
   acknowledgment. Drivers exit or follow their existing finite cleanup path; no retry
   loop on a closed receiver. Logged custody still resides in the WAL.

**D1 — capacity is distinct from custody.** The shared monitor queue and pending transient
collection are unbounded; deleting intermediate queues removes amplification, not this
existing risk. A burst or an absent client can consume memory without a defined limit.
This is a static risk, not a measured leak. The client writer queue (`monitor.rs:2300`)
is already bounded, which does not bound source ingestion or retained transients.
An arbitrary `try_send` drop would violate accepted-work semantics. A separate plan must
define limits and distinguish durable notifications that can be replayed/coalesced from
transients whose only copy is in memory, including absent-client retention, producer
backpressure, cancellation and user-visible rejection. Tests must prove bounded memory
under a burst, truthful acceptance/rejection and recovery without polling. This audit
does not claim all resource use becomes bounded after F1.

### 2. Duplicate models and implementations

**Method.** Compare similarly named types across crates and search their constructors,
exports, imports and consumers. Read the callers before deciding whether they answer the
same question. Inspect finished plans for intentional distinctions. External library
consumers are outside this repository search and remain an explicit compatibility limit.

**F2 — an orphan architecture, not a second required projection.** The public modules
`cli/runtime.rs` and `cli/fs_watcher.rs` have no caller outside themselves. The runtime's
repo registration, snapshot, subscription and signal methods are not used by a CLI or
web consumer. Its classifier is imported only by that runtime. The earlier
`unify-repo-state-watcher` plan explicitly left this future runtime out of scope and
not wired to a notify producer; it was never the active monitor implementation.

`cli/repo_state.rs:19` holds the runtime's multi-repo `Clank`, basename map and live-event
ring. `opportunistic_bodies` at 23 and `seen_stale_reviews` at 25 have no reader or writer.
`RepoState::digest` at 48 and `StateDigest` at 107 are used only by the orphan runtime;
local `LiveEvent`/repo/plan wrappers at 116 onward also serve only that runtime.
Delete this family, but retain `RepoState` at 32: its root/head/fold wrapper is live in
rebuild/cache/commands and is a legitimate I/O identity around the pure fold.

`core/api.rs:210` onward supplies another live-event DTO family used only by that runtime
and `core/tests/round_trip.rs`. Structured `FileDiff`/`DiffHunk`/`DiffLine` at api:24–84
have no production producer or consumer; the real diff command invokes Git/editor and
HTML has its own presentation. `core/model.rs:19,32`'s `Feedback`/`CommitGate` have no
consumer beyond their API re-exports and module documentation. Current review readiness
uses the fold/feedback views instead. Remove these obsolete DTOs and the unused model
module, retaining live finish/rewrite/purge preview types. The checked-in
`core/tests/wire_snapshots/` schemas have no loader/reference in the repository and describe
the removed response architecture; remove them with that family after a final consumer
search, rather than preserve an apparent compatibility guarantee no test executes.

This deliberately removes exported Rust library symbols. Core is `publish = false`;
that does not prove no out-of-tree source consumer exists. No current CLI, monitor
protocol or browser wire shape is removed. If a real consumer is identified in review,
rescope that specific export rather than keep the whole unused runtime speculatively.

Useful separations to **retain**:

- `disk_snapshot::CommitEvent` is an I/O event converted into the pure fold's event;
  it is not a competing gate algorithm. Root/head identity is likewise not folded state.
- Core `WaitItem`, CLI borrowed JSON projection and stop-hook tolerant owned decoding
  have different duties. The hook must tolerate future/unknown tags during binary changes;
  replacing it with strict core deserialization breaks that boundary.
- Web repo feed retains an image for late subscribers; host replay retains frames for
  browser resumption in another process. Revision, sequence and generation have different
  scopes. Neither consumes the orphan core live-event model.
- Command producer-ID deduplication outlives the bounded WAL horizon. Its seen-index and
  WAL are not redundant authorities for acknowledgment; deleting the index changes replay.
- GitHub flat selectors and nested watch clauses have different `pr_opened` scoping
  semantics. Normalize only after preserving their deliberately different contract.
- Native `ProcToken`, safe PID/group types, monitor ownership, status lease, session
  binding and Grok ownership prove different identities/rights. Similar names are not
  sufficient to merge these models or their locks across crate/process boundaries.

**D3 — configuration parsing shares storage, not failure semantics.**
`cli/config.rs:189` and `teams_config.rs:353` parse `.clank/config.json`. Their flattened
extras preserve sections owned by the other parser. `agent_store::load_repo_config` is
strict about roster/legacy role validity; the settings command intentionally permits
editing settings while an invalid roster remains opaque. No current divergent rule or
lost field was found. Retain this separation now. A later consolidation should use one
lossless raw envelope with separate settings and roster validators, with tests proving
invalid-roster repair and unknown-section preservation. Simply sharing the stricter
typed parser would regress repair and is not a justified cleanup.

### 3. Drift in the core rollup

**Method.** Read folded facts, warning variants, work-state derivation and routing, then
trace public fields into renderers, wire builders and cache encoding. Ask whether each
fact is meaningful without one particular renderer. Constructor searches distinguish
unrepresentable states from merely documented invariants. This is not an exhaustive
formal proof of every public API input or a request to remove all presentation DTOs.

The fold's roles, plan admission, feedback provenance and handover timestamps are domain
facts. Handover dates remain meaningful without elapsed-time rendering. `ContentRef`
locates event provenance/body content; it is not a TUI layout field. Saved command reply
routes preserve the accepted backend contract across configuration edits, so they must
remain event state rather than be looked up from current UI configuration.

The clear UI-only residue is `FileDiff.always_folded` at core/api:32, but its entire DTO
is unused (F2); moving the field elsewhere would perpetuate an unnecessary model.
The active fold has no discovered row-width, glyph, selection or scroll state to remove.

**D2 — actionable work should structurally require its SHA.** `core/wait.rs:984`
exports `PlanWorkState` with independently public `sha: Option<CommitSha>` and
`waiting_on`. `WorkStatus::work_for` at 1675 expects a SHA for actionable nonblocked work.
Current `derive_status` constructs valid combinations, including blocked intro-only work
with no SHA; nevertheless callers can construct a nonblocked actionable state with
`sha = None` and trigger the expectation. This is a latent public-model hazard, not
evidence of an actual current folded repo producing invalid work.

Retain current behavior in this finite audit. The next coherent type change should
represent blocked/pre-review state separately from reviewable work with a mandatory SHA,
or make construction private through validated constructors. Keep existing serialized
wire shape through a projection and migrate consumers/fixtures together. Do not “fix”
the expectation by returning no work or inventing an empty SHA: that hides an invariant
failure and can park an agent incorrectly. Gate, waiting reason and head correction
currently derive from one decision; they are projections, not independent mutable truths.

**D4 — unused construction is not enough to ignore serialized compatibility.**
`core/repo_state.rs:208` retains `Warning::DanglingPlanRef`; searches found only fixtures
constructing it, while the current fold constructs `RejectedPlanIntro`. The finished
`adhoc-commits-and-plan-tag-validation` plan explicitly retained the old warning variant.
The warnings are wincode cache-encoded (`state_cache.rs:67`, format 15). Deleting/reordering
variants requires an intentional cache-version change and one-time re-fold, not an
unannounced interpretation of old bytes. Keep the variant in this pass; remove it at a
deliberate schema transition with cache rejection/rebuild tests. Preserve ignored-run
provenance in `RejectedPlanIntro` and live warning matching.

### 4. Code that can be deleted

**Method.** Search full repository references for functions, fields, types, configuration
consumers and schema loaders; separately inspect test callers and production entry points.
Check serialization and earlier compatibility decisions before concluding a missing
constructor means a variant is safe to delete. No deletion is justified solely by a
compiler warning, a low hit count or the word “legacy.”

F2 removes a complete unconsumed model family. Its removed round-trip/digest/classifier
tests assert behavior of an API being withdrawn; no replacement guarantee for that
withdrawn API is claimed. Keep vocabulary/preview decode tests and actual monitor/web
transport tests. If a surviving transport lacks coverage exposed by the removal, add a
test for that surviving contract rather than recreate unused DTOs as fixtures.

**F3 — migration does not require a live legacy writer.** `cli/inbox.rs:111`'s file
`send`, sequence allocation, staging publication and `scan`/`resolve`/`done` at 60/73/98
have only test callers. Production `cli/send.rs:15` dispatches through `inbox::deliver`
at 392 to the WAL. Qualified-call searches find the old writer in the send unit test
at 134 and `tests/it/inbox_wakes.rs:63`; its remaining callers are its own fixtures.
Delete the old writer, allocation/slug/publication helpers and unused old read/ack API.
This intentionally withdraws another exported Rust API, not the current `clank send`
or acknowledgment command.

Retain `migrate`/`migrate_all` at 495/590, `pending` at 646, legacy parsing at 272,
sender presentation at 51 and staging cleanup at 146, which migration itself uses.
Old on-disk inboxes are user data; there is no evidence every installation has migrated.
Replace legacy test setup with explicit raw old-format file fixtures. Convert current
delivery/wake tests to `deliver` or the command entry point and assert WAL pending/ack
behavior. Remove old allocator sequence-space/concurrency tests because that allocator
is withdrawn; surviving concurrent WAL delivery, durable acknowledgment, reply-required
rejection and migration idempotence/custody tests supply the relevant current guarantees.
Do not weaken migration tests or leave a full allocator compiled only to manufacture
fixtures for the obsolete format.

**F4 — small residue with exact consumer searches.**

- `core/repo_state.rs:862`'s `warning_plan_tag` and `core/wait.rs:1028`'s
  `AdHocWorkState::is_settled` have no callers. Delete them; retain live
  `warning_mentions_plan` and `is_open`, whose semantics are different.
- `cli/status_tui/render.rs:838`'s `page_height` has no caller. Delete it. Constants
  `MIN_EVENT_BODY_COLS`/`MIN_EVENT_STATE_COLS` at 937/946 are referenced only in layout
  tests; move/constrain them to tests, preserving those layout assertions.
- `cli/monitor.rs:1635`'s `wait_for_ownership`, `read_published` at 2161, `published`
  at 2196 and `usable` at 2243 have only test callers. Remove redundant validation
  helpers or make fixture conveniences explicitly test-only. Retain production
  `wait_for_ownership_by`, `published_in`, admission/ownership checks and their tests.
  Removing a fixture wrapper must not remove tests for protocol mismatch or live-owner
  nonretirement; those guarantees come from the live attach/admission paths.
- `monitor.rs:2264`'s admitted `Served::Agent.has_cli_sources` is copied but never read
  and needs `allow(dead_code)`. Remove it from admitted state. Retain the wire
  `Who::Agent.has_cli_sources` for wire compatibility. The current monitor does not
  refuse a hello based on this flag: `WakeSources::may_share` keeps invocation-only
  sources local before attachment. Removing unread state must not add a new refusal.
- `cli/github/timeline.rs:18` has a blanket `allow(dead_code)` justified by consumers
  that were still staged; log/status now consume the timeline. Remove that exemption
  and use compile/reference evidence to dispose of actual remaining unused members,
  without guessing which are dead from the blanket annotation alone.

Retain legacy inbox reading, old Grok fork/session discovery and serialized config
aliases where they still consume persisted user state. Keep feature-gated test-support
APIs such as web-cloudflare door backdating when integration fixtures use them. Thin
web module re-exports have consumers and no worker/state cost; deleting them for taste
is outside this plan. Missing external-user evidence is not proof that every compatibility
path is obsolete.

### 5. Centralization and the end-to-end ownership contract

**Method.** Follow boundaries from user commands and source startup to monitor, kernel
leases/process identity, fold, work projection, stop-hook decoding and browser consumers.
Search direct Git/gix and raw signaling sites, read agent/config accessors and separate
call-time work from periodic background work. Consult the watcher/source-host/protocol,
runtime-state, reply, stop-hook-role and process-inspector decisions. Static inspection
does not replace the Git/signal/ownership boundary tests at final validation.

The monitor is the correct owner of one repository watcher/fold and configured-author
source hosts. Clients identify an author/session and their invocation needs, not an
authoritative role; the roster determines current roles. The stop hook projects work
returned after waiting and does not carry a stale role through the wait. The monitor
must not absorb UI state, remote transport replay, provider-specific session lifecycle
or invocation-only sources just because it is long-lived.

These are the contracts implementation and tests must preserve end to end:

1. **Identity and ownership.** Runtime paths are out of the repo. Kernel lease and
   process-birth identity authorize the monitor owner; descriptors are discovery data.
   Attach/version mismatch cannot evict a live owner. One configured source host exists
   per author regardless of waiter count, and source ingest leases continue preventing
   duplicate producers. Session binding and roster role resolution keep their own owners.
2. **Configuration and startup.** Author hosts follow validated configuration, with
   finite reconciliation on observed changes. Caller invocation/draft inputs retain
   their admission scope. A sink is bound before producers start. Removing an author
   stops its drivers, not another author's producers or accepted events.
3. **Durable ingestion.** WAL append/identity and existing deduplication establish
   persisted event custody. The sink carries `(item, durability)` and author attribution;
   its successful send establishes owning-queue acceptance, not user acknowledgment.
   Saved reply routes/provenance remain captured at event acceptance, not overwritten
   from subsequently edited config. WAL replay stays the durable recovery source.
4. **Presentation.** Live GitHub prompts are stamped synchronously before enqueue;
   replay reads current prompt metadata without rewriting WAL bodies. A wait tracks
   already-shown identities for that wait; showing does not acknowledge. Transients
   retain existing routing and in-memory retention, with no new persistence promise.
5. **Acknowledgment and reply.** Event selectors resolve through the actual event
   store. Required replies cannot be bypassed with plain acknowledgment. Existing
   durable reply-intent/effect coordination remains in the reply backend boundary;
   sink/monitor delivery never invokes callbacks or turns send success into an ack.
6. **Cancellation and handoff.** Synchronous acceptance finishes before cancellation
   can drop the sending task. Close ingress and drain the owning queue using existing
   shutdown ordering, present/count accepted transients truthfully, then complete
   producer cleanup and lease release. Drivers retain ownership of child groups and
   cleanup; joining them proves cleanup, not client handling. Failed enqueue stays
   explicit. Do not create a new handoff journal or a background repair loop.
7. **Failure and cost.** Preserve diagnostics for undelivered transients and failure
   of cleanup. No new periodic polling, idle task, process-table scan, per-client
   producer or unbounded retry. Delete the per-author forwarding queue/task and
   GitHub prompt queue/future. Existing single-wait owning queue remains necessary.
   D1 explicitly records the remaining unbounded-capacity policy gap.

The `Beat` filesystem bridge (`wait.rs:530`) adapts a blocking synchronous receiver to
async wakeup/cancellation; it is not an unnecessary event metadata hop. Likewise existing
monitor watchdog/fallback timings protect separate ownership/filesystem races. GitHub
polling and nonholder WAL tail cadence implement existing source completeness/takeover,
not this handoff. TUI pane-presence discovery (`status_tui/zellij.rs:17,605`, five-second
period) answers pane/session questions through one leased worker; on-demand process
inspection answers a selected PID. Remote transcript tailing and browser replay have
their own enabled-client scopes. These mechanisms are retained, not multiplied to repair
F1; any replacement requires a separate evidence-based cost and notification contract.

Git reads/mutations remain behind `git_io`/`git_plumbing`; production consumer searches
found no confirmed bypass. Raw signal/process-group operations remain behind the
established signal modules, including the separate web-cloudflare tunnel boundary.
`agent_store` remains the persisted-agent/roster access point; pure work derivation does
not read environment or storage. No additional relocation is justified by this audit.

### Implementation order, proof obligations and exit

After review of this audit, implement F1 first in one coherent source-contract milestone.
Trace every command/GitHub/draft producer and both monitor/single-wait owners into the
new sink. Deterministic tests must send an accepted emission and cancel immediately,
without depending on a forwarder scheduling turn: the owning receiver must already
contain it. Cover transient shutdown accounting, synchronous GitHub prompt stamping,
author isolation, invocation/draft scoping, durable replay/ack, closed receivers and
child/lease cleanup. Search for removed forwarders/private metadata queues; preserve
existing assertions, not just successful compilation.

Then delete F2 as a complete unused architecture, F3 as an obsolete delivery API plus
fixture conversion, and F4 with the live guarantees retained. These are coherent review
milestones, not one patch per warning. Before each deletion repeat the named consumer
search at the current revision and state what changed. Run affected core/source/wait/
monitor/inbox/TUI/web tests and boundary checks according to the touched contract.

At the final milestone run the full workspace suite required by this plan, including
integration/boundary checks and relevant feature paths. Record test counts, failures,
environment restrictions and platform limits honestly. Reconcile F1–F4 to actual fixes
and D1–D4 to their reason/next step; a deferral is not a claim that its risk was fixed.
Feature-sized follow-ups can be queued after review of their contracts, without making
this pass implement their new policies. The pass ends when these agreed dispositions
and validation are complete. Do not reopen the whole inventory after each deletion or
add fallback machinery for newly exposed symptoms without identifying its owning model.

## Milestone 2 — supplied sink and accepted-event custody

F1 is implemented through `cli/event_source.rs::EmissionSink`. Its direct and author-bound
constructors enqueue synchronously; its mapping operation transforms metadata within that
send and returns closed-queue rejection with the emission. The neutral module also owns
emission/durability and source provenance. Drivers now use this contract rather than
backend-specific channel types.

`EventSources` owns only source task handles. Local waits create and retain their delivery
receiver; monitor hosting supplies an author-bound sink directly to its shared queue.
`into_tasks` transfers lifecycle ownership without manufacturing a dummy receiver or
starting a forwarder. GitHub prompt stamping composes with the sink; the private prompt
queue and joined forwarding future are gone. WAL contents, replay/acknowledgment, reply
routes, invocation/draft scopes and producer leases retain their prior semantics.

The accepted-transient shutdown test now sends through an author-bound sink in a hosted
producer task and signals acceptance only after the send succeeds. `stand_down` cancels
that producer, drains the owning queue and flushes the event to its client. No sleep or
forwarder scheduling turn establishes acceptance. Separate sink tests check two authors,
both durability tags and rejected emissions after receiver closure. The GitHub prompt
test uses immediate `try_recv` to prove stamping is complete when send returns while
serialization still omits the prompt. A real two-author hosted-command test exercises
the `reconcile_hosting` wiring, distinct author attribution, one task per source and
ingest-lease release after cancellation/join. Existing local invocation, draft, durable
replay, required-reply and process cleanup assertions remain in place.

Validation: 511 affected unit tests passed across event-source, wait, command-source,
GitHub, monitor, PR-review and events modules; the additional real hosted-source regression
passed separately (512 distinct affected tests total). All 13 integration boundary tests
passed. Production and test compilation passed; formatting and `git diff --check` passed.
The full workspace suite remains for milestone 4. This milestone adds no polling, worker,
producer process, process-table scan or retry loop. It removes one queue/task per hosted
author and one queue/forwarding future per GitHub source. Searches found no remaining
`tagged_into`, `stamp_instructions_forwarder`, backend-owned emission/durability or
GitHub-owned source-origin references. D1's existing capacity-policy gap is unchanged.

## Milestone 3a — retire the unused runtime family

F2 is fixed. Repeated constructor/import/consumer searches at `4e38bad5bb41` again found
the daemon `Runtime` and filesystem classifier only in their own family. `StateDigest`,
the multi-repo `Clank`, its unread maps/event ring, local event wrappers and `RepoRoot`
alias likewise had no surviving consumer. Core live-event/diff DTOs and model
`Feedback`/`CommitGate` had only that runtime, re-exports or round-trip tests as consumers.
No code or manifest loaded the 18 checked-in `wire_snapshots/*.schema.json` files.

Removed the runtime, classifier and model modules and their exports, the unused CLI
state/digest/event family, the orphan core event/diff types and unloaded schemas. Kept
the live CLI `RepoState` root/head/fold wrapper, core fold, vocabulary, feedback views,
finish/rewrite/purge preview DTOs, active repository watcher and web bridge/feed/replay.
The fold/cache layout is unchanged and needs no format bump. Removed documentation
references to the deleted architecture. This is the deliberate Rust-export withdrawal
described in the audit; no active command, monitor protocol or browser shape is withdrawn.

Removed 15 classifier tests because that classifier API no longer exists, and ten
live-event/diff serialization/decode tests because those DTO APIs are withdrawn. These
are not declarations that an equivalent contract is now guaranteed by a type: there is
no replacement for the retired API. The seven vocabulary wire tests remain. The active
watcher tests retain current wake-root/filter and linked-worktree behavior; monitor/web
tests retain actual presentation/transport behavior; rebuild/cache and preview/finish
tests retain their live contracts. No digest tests existed to remove. The schema files
were unloaded data, not executed snapshot assertions.

Post-deletion searches across crates found no surviving references to the removed
symbols/modules/schema directory. All workspace targets compiled with all features on
this macOS host. `clank-core --all-features` passed 259 unit tests, seven vocabulary
integration tests and one doc test (267 total). Selected CLI rebuild, cache, preview,
monitor, web and repository-watcher modules passed 215 tests. Finish integration plus
Git/signal boundaries passed 33 tests. Formatting and diff checks passed. The full suite
and platform limits remain for milestone 4; F3/F4 remain the next implementation work.

## Milestone 3b — retire legacy message writing, retain migration

F3 is fixed. Rechecking qualified callers at `34492daec052` found `inbox::send` and
`inbox::scan` only in test setup; resolve/done and the file allocator/publication helpers
were likewise confined to that obsolete API and its own tests. The production send
command already used `deliver`. Removed file send/scan/resolve/done, priority/sequence
allocation, slug/render/publication helpers and the exported file-message view. The
parser's result is now a private `LegacyMessage` owned by migration. Sender presentation,
first-line subject extraction, current WAL delivery/pending and migration remain live.

Migration keeps its existing custody order, pinned identities, lock, staging cleanup,
timestamp fallback, unreadable-file retention and removal of old bookkeeping. Literal
old-format fixture files now exercise that reader without preserving an obsolete writer
as production or test-support code. The complete historical-inbox fixture includes
`.seq`, `.seq.tmp`, `.lock` and interrupted staging data. A staging-hard-link test verifies
migration unlinks leftovers without changing the published bytes. Header/body tests now
assert the migrated WAL message, including sender, repository, timestamp and bare-body
fallback, rather than an unused legacy scan result.

Send-order tests now use `deliver` and `pending`; the late-ack test now proves an already
acked selector cannot handle a later WAL message. The existing concurrent WAL send test
also checks each complete body, alongside unique selectors and independent acknowledgment.
The cross-repo send test invokes the actual send command and asserts a WAL selector and
body, with no legacy file inbox created. The wake integration fixture uses a literal old
file; its migration/wake/ack assertions are retained.

Five old tests were removed rather than merely renamed: the hard-link publisher,
unreadable legacy sequence counter and eight-digit cap tests describe withdrawn writer
mechanisms; the writer's staging-list test is superseded by migration staging/complete
cleanup tests; the old file-writer concurrency/body test is covered by the strengthened
current WAL concurrency test. Four other tests were converted to current acknowledgment
or migration contracts. Inbox tests went from 29 to 24; there is no replacement promise
for the removed allocator/publication API. This deliberately withdraws obsolete Rust
exports and leaves current `clank send`/events behavior unchanged.

Validation: 67 unit tests passed across inbox, send, message dispatch and events/reply.
The first integration run inherited this session's `CODEX_THREAD_ID`: ten synthetic-agent
wait tests failed because the real session did not own their unbound fixture labels.
Rerunning with only that ambient variable removed passed all 17 inbox/standby/config-write
integration tests. Preserve that isolation for final validation and report the initial
failure honestly; no production identity check was weakened to accommodate the fixtures.
Compilation, formatting and diff checks passed. Searches find no remaining qualified
calls to the removed inbox APIs or the removed allocator constants. No worker, polling,
process scan or retry behavior was introduced. F4 remains next, then milestone 4.

## Milestone 3c — remove unread residue, keep the live contracts

F4 is fixed. Repeated consumer searches at `7850768e5744` found no callers of
`warning_plan_tag`, `AdHocWorkState::is_settled` or `page_height`; those helpers are
deleted. Live `warning_mentions_plan`, `is_open`, warning variants and encoded cache
layout are unchanged. The core helpers' Rust exports are deliberately withdrawn,
as scoped by the audit; no gate, routing or warning behavior changes.

The two minimum-width constants and monitor fixture conveniences `wait_for_ownership`,
`read_published` and `published` now compile only in tests. Their consumers and layout,
descriptor and ownership assertions remain. Production keeps `wait_for_ownership_by`
and `published_in`; the version-first descriptor documentation now belongs to the
live reader. Removed the unused `usable` predicate and its sole test. Its pre-greeting
version check describes a retired decision path: current attachment deliberately
greets an older descriptor's live holder to negotiate handoff. Tests of live admission,
wrong-repository refusal, version mismatch, older-holder greeting, ownership deadlines
and nonretirement remain and passed. This is withdrawal of an obsolete helper API,
not a claim that its old decision is guaranteed elsewhere.

Removed unread `Served::Agent.has_cli_sources` and its dead-code exemption. Kept
`Who::Agent.has_cli_sources`, its wire fixtures and round-trip assertions unchanged.
Corrected the audit's earlier description: current admission ignores that flag;
`WakeSources::may_share` keeps invocation-only sources local before attachment.
No new refusal is introduced, and roster/identity validation and source scope are
unchanged. Removed the timeline module's staged-consumer dead-code exemption;
compilation found no further unused timeline members to delete.

Validation: all workspace targets compiled with all features on this macOS host,
with no production warnings. There remain 20 CLI test warnings and one core test
warning in unchanged test code; this milestone does not disguise those as a clean
test-warning baseline. Core passed 259 unit, seven vocabulary and one doc test
(267 total). Selected monitor, wait, GitHub timeline, TUI rendering and TUI lease
modules passed 372 unit tests. All 13 integration boundary tests passed (652 tests
total). Integration and unit runs cleared ambient `CODEX_THREAD_ID` as recorded in
3b. Formatting and diff checks passed. Searches find no removed-helper references;
test-only conveniences retain only test callers. No polling, worker, process scan,
retry or protocol/cache-format change was introduced. F1–F4 are implemented; final
disposition reconciliation and the full workspace suite remain for milestone 4.

## Milestone 4 — final reconciliation and validation

The finite audit is complete. The baseline inventory and the five written answers
above remain the evidence; implementation did not restart the search for new work.
The final dispositions are:

| ID | Final disposition | Evidence / remaining action |
| --- | --- | --- |
| F1 | Fixed in `4e38bad5bb41` | Source acceptance goes synchronously into the delivery owner's queue, with author/durability attached and GitHub prompt mapping complete before send returns. Cancellation tests prove accepted transient custody; live two-author hosting proves attribution and cleanup. |
| F2 | Fixed in `34492daec052` | Removed the orphan runtime, classifier, multi-repo/event/diff model and unloaded schemas as a family. Live fold/root-head wrapper, vocabulary, preview DTOs and web/monitor transport remain. |
| F3 | Fixed in `7850768e5744` | WAL delivery is the sole current message writer. Migration owns reading/removing historical files; literal fixtures preserve old data, staging and custody guarantees. Removed writer-only API tests are accounted for in milestone 3b. |
| F4 | Fixed in `211defcf64fe` | Removed unread state and unused helpers, constrained fixture conveniences to tests, and removed the staged timeline exemption. Wire/admission/cache behavior is unchanged; the obsolete predicate assertion is explicitly withdrawn. |
| D1 | Deferred: capacity/overload policy | Existing unbounded ingress and absent-client transient retention need a separate decision about limits, backpressure, replay/coalescing and truthful rejection. No silent dropping or arbitrary limit is introduced. The detailed acceptance and burst/recovery proof obligations above define the next plan. |
| D2 | Deferred: valid work-state construction | Current derivation is valid, but public fields allow impossible actionable states. A coherent type/constructor migration must require a SHA for actionable work, preserve blocked/pre-review states and project the existing wire shape; returning no work on invariant failure is not a fix. |
| D3 | Retained: config parsers with different validation duties | Settings repair must still work when the roster is invalid. Consolidation is optional and requires a lossless raw envelope plus separate validators, with invalid-roster and unknown-section preservation tests. No present divergence was found. |
| D4 | Retained: encoded legacy warning | Cache format 15 still encodes this variant. Removal belongs to a deliberate cache/schema transition with rejection/rebuild tests and preserved ignored-run provenance; this audit does not alter variant interpretation. |

The five answers therefore converge on one acceptance-owner correction, retirement
of unused alternative models/APIs, and explicit retention of meaningful boundaries.
No additional centralization is justified: pure fold/work derivation, Git I/O,
validated signaling, roster/session storage, tolerant hook decoding, source drivers,
monitor hosting and browser replay retain distinct responsibilities. The full
contracts above specify acceptance, custody, presentation, acknowledgment, cancellation
and failure separately; no later repair loop is needed to reconcile those meanings.

Resource outcome: no new timer, idle worker, producer process, process-table scan or
retry loop. Hosted sources lose one forwarding task and one queue per author, and
each GitHub source loses its prompt-forwarding queue/future. The supplied sink uses
ordinary synchronous calls and captured shared ownership. Existing required source
polling and ownership/fallback timers remain scoped as documented; capacity remains
D1, not a claim of bounded total memory. Runtime paths, monitor protocol and fold-cache
format are unchanged. Historical user-data readers remain. The Rust-export withdrawals
are deliberate, with no external library-user survey or universal compatibility claim.

Final command on `211defcf64fe`:

```sh
env -u CODEX_THREAD_ID cargo test --workspace --all-features --offline
```

It exited successfully: **2,700 passed, zero failed, five explicitly ignored**,
with no filtered tests. Breakdown:

| Harness | Passed | Ignored |
| --- | ---: | ---: |
| CLI unit | 2,047 | 2 |
| CLI integration (including all boundary/config gates) | 331 | 0 |
| Core unit | 259 | 0 |
| Core vocabulary integration | 7 | 0 |
| Web unit | 11 | 0 |
| Web bridge integration | 5 | 0 |
| Custom-host example | 1 | 0 |
| Web-cloudflare unit | 38 | 3 |
| Core documentation | 1 | 0 |

The other three crates' documentation harnesses contained zero tests. All 13 source
boundary tests and all six config-write scanner tests are included in the successful
CLI integration run, not added again to the total. The expected cherry-pick conflict
diagnostic in that run came from a conflict fixture; it was not a failing test.
Formatting and `git diff --check` passed. Milestone 3c also compiled every workspace
target with all features; this milestone changes documentation only. The same 20 CLI
and one core test-code warnings remain; no production warning was emitted.

The five ignored tests are not claimed to pass: Grok's native initialization and
offline-fork smoke tests require `CLANK_GROK_TEST_BIN` in a separate credential-free
process; three cloudflare tunnel tests require live internet, with one also requiring
cloudflared. Local synthetic lifecycle, fork, command, socket, web and tunnel fixtures
did run. This macOS validation does not execute Linux-only paths, exercise real remote
GitHub/provider services or prove real Zellij server behavior. No production CPU/memory
benchmark was performed; the resource conclusion is the removed queues/tasks and absence
of added background mechanisms, not a measured idle-load or bounded-memory claim.

The `CODEX_THREAD_ID` isolation addresses the concrete fixture failures recorded in
3b: it affects only the spawned test process, not this session's binding or production
identity checks. No test was newly ignored, disabled or weakened to make this final run
pass. The earlier initial fixture failure remains recorded rather than erased.

The audit's acceptance is met within these explicit platform/provider limits: all five
questions have methods and answers; F1–F4 are fixed; D1–D4 have reasons and coherent
follow-up contracts; removed tests/APIs and preserved behavior are accounted for; the
complete nonignored workspace suite and boundaries pass. There is no further implementation
milestone in this plan. Submit this reconciliation to the review gate, then finalize
only when the gate supplies a finalize item.
