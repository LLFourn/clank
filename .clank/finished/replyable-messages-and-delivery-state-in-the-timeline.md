# replyable-messages-and-delivery-state-in-the-timeline
# Replyable messages and delivery state in the timeline

## Why

The user requests replyable Clank agent messages, multiple replies to a message,
visible outbound replies even when their recipient is outside the current team,
and clearer event delivery icons. This plan is queued independently of the active
automatic-setup/Grok integration plan.

## Contracts

- Add `clank send --reply-to` to mark a message as requiring a reply on
  acknowledgement and record its reply destination. Resolve a concrete agent label
  and canonical repository, including cross-repository sends. Specify CLI syntax
  before implementation: a label alone refers to the sending repository, not the
  destination selected by send's --repo. Provide an explicit repository form where
  needed. Preserve ordinary messages when the option is absent and never infer
  required-reply intent merely from sender metadata.
- Use the existing required-reply event/ack contract: first acknowledgement of a
  replyable message requires reply text, and the reply must be accepted durably by
  the destination before acknowledgement succeeds. Failed delivery leaves work
  unhandled. Repeated ack of an already completed transaction is idempotent. Role
  changes cannot silently retarget a captured reply endpoint; removed/unavailable
  destinations produce an actionable error. Human-origin messages must be able to
  specify a valid destination without pretending to be a bound agent.
- Allow multiple deliberate replies to the same logical message, including after
  it has been acknowledged. Add/document a reply operation independent of ack;
  sending another reply must not reopen or repeat completion of the original.
  Define operation identities so retrying a failed/interrupted send does not
  duplicate delivery, while identical text sent deliberately twice is permitted.
  Store a reply history rather than overwriting one answer. Retain existing command
  backend reply semantics; do not accidentally repeat its side effects while
  generalizing the message backend. Required-reply enforcement applies even if
  inspection occurred outside the CLI.
- Persist outbound reply custody/provenance in the sending repository so replies
  to agents outside its roster appear as real timeline entries with body, sender,
  destination repository/agent, time, and linkage to the original message. Do not
  represent the reply solely by changing the inbound event's ack marker. Avoid
  duplicate rows when local inbound/outbound copies represent the same message.
  No external repository scan or new polling is required to display local sends.
- Expose three event delivery states consistently: `?` = not yet woken;
  single tick = woken but not acknowledged; double tick = acknowledged. Wake
  receipt proves presentation/delivery, not human reading or completion. Persist
  enough evidence for message events as well as existing event sources; opening
  list/show/TUI is not a wake. Old acknowledged events remain double-ticked; old
  unacknowledged events without delivery evidence stay `?`. Define merged-copy
  behavior and retain per-recipient detail: all handled means double tick; pending
  recipients lacking wake evidence must not be concealed by another handled copy.
- Discovery must not reveal event content without delivery evidence. `events list`
  and `events show`, in text and JSON, expose only metadata. Add `events read ID`
  for full content: verify the invocation's current agent/tool/session binding,
  reject a foreign --author, write and flush successfully, recheck binding, then
  stamp that exact copy and run any configured command-source receipt callback.
  Reading never acknowledges or supplies a required reply; broken output and stale
  bindings produce neither evidence nor callbacks. Preserve source-host cwd on new
  command rows for receipt-only backends; old rows without context skip callbacks
  with a diagnostic. No idle worker, polling or retry queue is introduced.
- Give `msg` entries a conventional mail/envelope icon, separate from their delivery
  status marker. Apply consistent icons/state to TUI, CLI timeline/log and web
  projections, with sensible terminal-width handling. Retain lossless message and
  reply bodies in structured state and detail views.

## Implementation and validation

1. Trace message custody, send routing, checked ack/reply transactions, wake
   receipts and timeline merge/projection. Specify reply endpoint syntax, repeat
   operation identities, append-only history and migration/compaction contracts.
2. Implement send/ack/reply backend support using existing WAL/lease mechanisms.
   Exercise same-team and cross-repository endpoints, required-reply enforcement,
   failures, crash/retry boundaries, races, repeated deliberate replies and old
   message compatibility. Use isolated repositories and fixture callbacks.
3. Add outbound timeline records and the mail icon plus three-state projection.
   Test actual wake vs inspection vs ack, merged copies, removed agents, restart,
   compaction, narrow terminals and web/log structured parity.
4. Document commands and icon meanings, run affected tests and boundary checks,
   and submit the complete implementation through Clank review. Do not install binaries or migrate
   live repositories merely by queueing this plan.

## Exit

Replyable messages cannot be acknowledged without an accepted reply, multiple
replies are retained and visible, external-team replies have actual timeline rows,
and the timeline distinguishes unwoken, woken and acknowledged events with a mail
badge for messages, without additional idle resource cost.

## Milestone 1: custody trace and implementation contracts

### Existing owners and the gaps

`cli/send.rs` resolves `--repo` as the destination and chooses its concrete
recipient from that roster. `sender_in` independently resolves the invocation's
repository and bound agent. `inbox.rs` appends a Message to the recipient's
`events/messages.jsonl`; its writer lock covers sequence allocation and append.
There is currently no source-side outbox. Message action keys include the
recipient, so two unrelated recipient deliveries do not accidentally merge.
Legacy file migration retains file custody until the WAL owns the message.

`cli/events.rs` and `EventLog::append_checked_ack` both enforce required replies,
including a final check under the WAL lock. However Message has no required-reply
field or route. `events/reply.rs` accepts only CommandEvent: it captures one
immutable EventReply, acquires a backend dispatch lease, records intent, calls
the saved command, and appends acceptance plus acknowledgement. Copied command
rows share that one transaction. Its callback receives a stable idempotency key;
a callback retry can repeat an invocation, and the backend must deduplicate its
logical effect. We must preserve that contract, not reinterpret this object as
a list of messages or enable arbitrary repeated command callbacks.

`stop_hook.rs` prepares command-source receipt candidates from the selected wait
snapshot and dispatches callbacks only after successful continuation output and
flush, with the binding revalidated. The separate SessionStart path has the same
presentation boundary. Inspection and ordinary wait output do not earn receipts.
`wake_receipt.rs` currently reports to configured command callbacks; it does not
save presentation evidence in the event WAL. The merged timeline in
`github/timeline.rs` reads local agent WALs, merges connected components over
stable aliases, and is unhandled while any member is unacknowledged. CLI log,
TUI and web currently project that boolean rather than three delivery states.

### CLI and routing

Use these unambiguous forms:

```
clank send --repo /destination --to worker --reply-to coordinator -m 'Please investigate'
clank send --repo /destination --to worker --reply-to coordinator --reply-repo /origin -m 'Please investigate'
clank events ack messages@7 --reply 'Investigated; here is the result'
clank events reply messages@7 -m 'A further finding'
clank events reply messages@7 --operation OP -m 'A further finding'
```

`--reply-to LABEL` is a concrete validated agent label, never a dynamic master
alias. Without `--reply-repo`, its repository is the invocation's repository,
resolved independently of send's destination `--repo`. `--reply-repo PATH`
requires `--reply-to` and resolves a Git repository through the existing approved
resolver; persist its canonical path. Validate its concrete roster member before
creating custody. This works for an unbound human sender too. Sender provenance
and the reply endpoint are separate typed fields; a human stays a human.

Add `MessageReplyRoute { repo, agent }` to Message with an explicit
`requires_reply` flag. `--reply-to` sets both. Absence preserves ordinary-message
behavior. A normal agent-origin message can use its recorded sender as the
endpoint for an explicit reply, without becoming reply-required. A human-origin
message with no endpoint cannot receive an agent reply; report that clearly.
Once an operation starts, freeze the resolved endpoint in its intent. Subsequent
role changes never move it. Revalidate membership before a NEW destination
acceptance; an existing durable acceptance remains valid if that agent is later
removed. Missing repositories or invalid recipients leave the operation pending.
No implicit delivery to a replacement master and no shell parsing of paths.

`events reply` uses the existing repository/author/selector resolution. One
selector, nonblank body, message backend only. Support `-m` paragraphs and
`--file`/stdin consistently with send. Command sources continue to use
`events ack --reply`; `events reply` rejects them without invoking a callback.
An optional first reply to an ordinary message does not acknowledge it implicitly.
For a required message, `events reply` likewise leaves acknowledgement separate;
`events ack --reply` creates the distinct acknowledgement reply transaction.
This explicit separation avoids silently completing a task with a progress reply.
Reply delivery creates an ordinary Message, with linkage to its parent, and does
not inherit the parent's required-reply flag. Replies must not create an automatic
infinite chain of reply obligations.

### Identity, locking and persistence

Keep the existing command EventReply and its records unchanged. Add typed
message operation records in the replying author's messages WAL, rather than a
new background service or file per message. An operation records its random ID,
original copy key `(author, source, seq, ident)`, original message handle, concrete
endpoint, exact body, sender provenance, time, and purpose (`ack` or `reply`).
Accepted operations are the source-side outbound timeline records. They are
separate from presentable inbox work: the sender must never wake to acknowledge
its own outgoing reply. Pending intents are recoverable state, not a false Sent
claim. Expose them in event detail with their retry operation ID and diagnostic.
The locked plain-ack guard must also reject an existing unaccepted ack-purpose
message operation, including on an otherwise ordinary message. A progress/reply-
purpose operation alone does not block plain acknowledgement.

The acknowledgement operation has a deterministic identity derived from the
original full copy key and a versioned domain. It is immutable: same ack/text
retries it; a different text is refused and directs the caller to a deliberate
reply. A completed acknowledgement retry is a no-op, never another message.
Each new `events reply` invocation without `--operation` mints a new operation
ID. Equal bodies intentionally sent twice therefore deliver twice. Print the
operation ID once intent is durable, before delivery. `--operation OP` MUST
refer to a retained existing intent under this author and original copy; it
never creates an unknown operation. Verify body and route against that intent.
Missing/pruned IDs fail without sending, with an explanation to start a new
reply only if the caller intends another message. No body-based deduplication.

Acquire one nonblocking message-dispatch lease per replying author, releasing it
on process exit. Re-read the selected copy and operation after acquiring it.
Use the existing messages writer lock for allocation and short WAL writes only.
Never hold an append/data lock while writing another repository. Destination
acceptance takes only its recipient's short writer/data lock. Distinct dispatch
and append locks avoid deadlock when two agents reply to each other. A contended
dispatch reports retryable contention; it never waits in an idle loop.

Use the EventLog durable append discipline (repair torn final lines, sync the
file, sync the parent directory), not the ordinary send path's weaker append
assumption, at these reply transaction boundaries:

1. Source intent is durable before any destination write.
2. Destination atomically stores the received Message and a pinned acceptance
   identity for this operation. A versioned delivery ID incorporates canonical
   source repository, source author and operation ID. A duplicate under that ID
   returns the original acceptance after syncing any visible-but-unsynced record;
   a different immutable payload is an error. Never allocate a second inbox row.
3. Source records durable acceptance. For an ack-purpose operation, acceptance
   and retirement of the exact original copy are one source WAL record. A reply-
   purpose acceptance adds history/outbound custody without changing original
   acknowledgement. Readers fold both states from these records.
4. Only after source acceptance is durable, release the destination's dedup pin.
   This is best-effort storage cleanup, not permission to roll back acceptance.
   A later explicit retry of the known operation can finish cleanup without
   sending. Never inspect remote repositories just to render the local timeline.

Destination dedup must not rely on its bounded normal observation horizon: a
recipient can acknowledge and compact its reply while the sender is still in
the acceptance crash window. Pin the minimal operation acceptance independently
of inbox-body retention until step 4. Destination repeats of a still-pinned
operation synchronize and return acceptance even if its original body has aged
out. After release, the source acceptance suppresses every supported retry;
unknown source IDs cannot initiate delivery. A cleanup interruption can retain
a small pin indefinitely until an explicit retry; no idle janitor is introduced.
This costs storage per unresolved handoff, not per wake or idle tick. Purging the
source transaction store ends its retry contract; it does not authorize replay
of a caller-supplied old operation ID.

Crash cases follow these boundaries: before intent, no send; after intent, retry
same operation; after destination acceptance, destination deduplicates; after
source acceptance, return the recorded result and never redeliver. No atomic
cross-repository filesystem transaction is claimed. A partial/erroring durable
append is uncertain until the next retry repairs/re-reads/synchronizes it. Do
not mark handled merely from an in-memory read or a successful write without
its required sync. Fixtures must fault each boundary, including directory sync.

### History, compaction and version compatibility

Message replies are append-only immutable operations, not replacements for the
existing `EventRow.reply`. Detail shows retained operations and their states.
Retain every unaccepted intent and every unresolved destination dedup pin.
Accepted bodies/history use the existing recent-display retention policy; this
feature does not promise infinite transcript storage or add a new body-size cap.
When a retained original references aged-out replies, indicate that older reply
history was pruned rather than claiming it never existed. Retained operation
acceptances remain replayable; once pruned, explicit operation retry fails closed.
Compaction preserves allocator reservations, exact copy identities, required-
reply policy and wake evidence attached to retained events. Accepted ack-purpose
operations compact to the same handled semantics as ordinary acknowledgements.
Keep their immutable intent/acceptance while the original is selectable, even if
the separate outbound display row ages out, so retry never starts another ack
reply. Ordinary display pruning cannot silently remove an original's completion.
Unknown records remain byte-preserved using the existing foreign-record rules.

Do not encode a reply-required message solely as optional fields on a v1 inbox:
older readers would drop those fields and allow a plain ack. Use a semantic v2
inbox envelope for reply-required Messages, and extend the current reader to
understand that specific record/version. Ordinary messages keep their existing
format. Old readers preserve a v2 inbox as foreign and cannot select it for ack.
New operation/pin records use distinct typed record tags; no reinterpretation of
existing command ReplyIntent/Ack meanings. Preserve v1 command transactions,
legacy migration IDs and pending work. Mixed old/new message-operation writers
are unsupported; deploy supporting binaries on all custodians before enabling
reply-required sends, and test old-reader foreign preservation explicitly.

### Wake evidence and timeline projection

Add a discriminated presentation record `(seq, ident, at)` to the existing event
WAL, independent of backend callbacks, reply intent and acknowledgement. Record
the first successful presentation once per exact copy; repeated wakes are no-op
writes. The hook carries authoritative selected copy keys alongside its existing
receipt capture; no rendered-text parsing or second whole-inbox lookup to invent
which rows were shown. Under the short write lock, refuse a missing/mismatched
copy rather than attaching evidence to a reused selector. All presentable logged
sources, including messages, timers and GitHub, use this evidence. Callback
configuration/limits/failure do not decide whether an event was presented.

Use the existing stop and SessionStart successful-continuation/flush boundary,
current binding checks, and adapter-specific Grok/OpenCode model-delivery boundary.
Background task creation, waiter registration, `wait --peek`, ordinary CLI waits,
list/show/TUI opening and failed/stale hook output are NOT wakes. Do not claim
proof of model understanding or human reading. A crash after actual output but
before the WAL stamp can conservatively leave `?`; never stamp before output to
hide that uncertainty. Recording failure does not prevent already emitted work
or acknowledge it; report a bounded diagnostic and try again on a later actual
wake. Existing command receipt callbacks retain their current separate semantics.

Per inbound copy: Acked => double tick; otherwise presentation evidence => single
tick; otherwise `?`. For merged entries: all local recipient members acked =>
double tick; any pending recipient without presentation evidence => `?`; otherwise
single tick. Keep every member's actual state in structured detail. Baseline/
observational entries remain distinguishable from pending work. Old acked events
project double tick; old pending records without evidence project `?`.

Source outbound entries carry body, endpoint, original linkage and operation ID,
with a Sent/custody badge when accepted. Destination acceptance does NOT mean
the remote agent woke or acknowledged, so never manufacture double ticks for an
external outbound-only entry. In the same repo, shared typed delivery identity
merges outbound provenance with its real inbound copy into one entry; only
inbound members decide work/delivery state and are acknowledgement targets. Do
not merge different replies by original message handle or body. External replies
show locally without fetching the destination or installing receipt polling.

Define one shared delivery enum/projection and preserve the existing `unhandled`
boolean for consumers that need pending-work logic. Use that projection in CLI
log/JSON, TUI/detail and web serialization. Messages use a conventional envelope
badge separate from `?`/single/double tick; align terminal display widths and
retain an ASCII/accessibility label fallback. Status colour remains restricted
to deliberate indicators, consistent with the completed stable-colour plan.

### Reviewable implementation sequence

1. Typed endpoint/message-format compatibility and send routing; required-reply
   refusal at both CLI and locked WAL boundary. Preserve all ordinary-message
   and command callback fixtures.
2. Source operation records, destination dedup pins, durable acceptance and
   explicit ack/reply operations. Test deliberate equal-body replies, retries,
   cross-repo role changes/removal, reciprocal concurrency and every crash/sync
   window, including destination compaction before source acceptance.
3. Local outbound history and shared identity merge; compaction/restart/pruned
   operation behavior. Verify external rows need no external scan and outgoing
   records never become sender work.
4. Presentation stamps at real adapter boundaries and shared delivery projection.
   Verify inspection vs wake, failed flush, binding changes, callback failure,
   legacy states, mixed recipient states, and retained stamp compaction.
5. Envelope/status rendering across CLI, TUI and web; narrow/accessible layouts,
   structured body/provenance parity, documentation and reviewed installation.

This milestone defines implementation contracts from the current code. It does
not change message delivery, installed binaries, event formats or live state.
Validation is source tracing and whitespace checks; backend and UI acceptance
belong to the implementation milestones above.

## Milestone 2: typed reply destinations and required-message custody

`send --reply-to LABEL [--reply-repo PATH]` now validates a concrete roster agent
and captures a typed MessageReplyRoute. The default reply repository comes from
the invocation, independently of send's destination `--repo`; a nested working
directory resolves to its Git root. Explicit reply repositories likewise resolve
to canonical Git roots. Unbound human senders can name endpoints without changing
sender provenance. Invalid labels, missing/non-Git repositories and reply-repo
without reply-to fail before message custody is created. Role changes do not
mutate the saved label. Ordinary send, migration and interruption paths retain
their existing behavior through the no-route wrapper.

Message carries `requires_reply` and its captured route through the event view
and wait JSON. Hook hints explicitly request `ack ... --reply <text>` for these
messages. Existing event policy now rejects their plain acknowledgement both
at CLI resolution and under the final WAL write lock; inspection cannot bypass
that policy. This milestone does not implement message reply dispatch yet:
`events ack --reply` still supports only the existing command backend, and such
message work remains owed until the following transaction milestone. No binary
or hook installation has been performed.

The new obligation uses a specific version-2 Message inbox envelope. Ordinary
messages remain v1 with omitted new fields, and old rows deserialize without a
reply requirement. The reader admits only the defined v2 live Message with a
required reply and concrete route; unrelated v2 records and later versions stay
foreign. The writer refuses required messages lacking a route or using a
non-message/baseline transport. Compaction preserves the exact pending input
and its v2 envelope. The prior reader's v1 version gate cannot expose this new
record as ordinary ackable work. This is semantic version refusal, not a claim
that old binaries can process the new feature: supporting custodians must be
deployed before use, as the previous contract states.

Validation: send tests pass (9), event WAL tests pass (56), inbox/migration tests
pass (25), and existing command reply/callback tests pass (9). The new required-
message hook hint and existing stable wait serialization tests pass. Git-layer
boundary checks pass (2); formatting and whitespace checks pass. Fixtures cover
cross-repository delivery, canonical origin routing, unbound senders, frozen
labels through master changes, invalid endpoints, both plain-ack boundaries,
legacy messages, specific version admission and pending compaction/restart.
No polling, helper process or per-message runtime resource was introduced.
Remaining: deliberate reply operations, durable cross-repo handoff and history,
outbound timeline projection, presentation stamps and mail/delivery rendering.

## Milestone 3: durable message operation and receiver-custody primitives

The existing messages WAL now understands distinct typed message-operation and
message-receipt records. The command EventReply/ReplyIntent/Ack contract remains
unchanged. An immutable operation captures the source repo/author, exact original
copy, endpoint, body, time, purpose and operation ID. Repeating that ID with
changed input is refused. New operation IDs permit deliberate equal-text replies.
Outgoing operation records reserve their own sequence space but are not inbox
work: they never cause the sender to wake for its own outgoing message.

Destination delivery appends one durably synced ordinary Message under a delivery
ID derived from source repo/author/operation. That inbox record IS destination
acceptance, avoiding a crash window between receipt and message writes. Its
reserved action-key prefix lets the reader recover acceptance immediately from
the inbox. Repeating delivery verifies the immutable payload digest, syncs any
visible-but-uncertain acceptance, and returns the original copy receipt. Replies
do not inherit the original required-reply flag.

Before compaction discards an accepted destination inbox body, it synthesizes a
small typed receipt pin with delivery ID, payload digest and exact copy identity.
The pin is independent of the ordinary identity/display horizons and survives
acknowledgement, body pruning and restart. It cannot be removed until the source
has durably recorded acceptance. The release primitive is explicitly for the
future trusted dispatcher to invoke after that step; release failure is cleanup,
not rollback. Released metadata is retained only while its inbox is selectable,
so the inbox cannot regenerate an active pin on the next compaction. No permanent
file per operation, idle cleanup loop or new helper process is added.

Source acceptance is a single operation record that embeds its receipt. For an
ack-purpose operation, folding that record retires only the original `(seq,
ident)` copy. A deliberate reply acceptance leaves original acknowledgement
unchanged. Pending ack operations also make an otherwise ordinary message
reply-required at the final locked plain-ack check and in its wait projection.
Compaction retains every pending operation and its original copy, including an
already-handled original of a still-pending follow-up reply. Accepted history
uses the existing display retention; ack decisions remain while their original
is selectable. Conflicting operation/receipt records fail explicit mutations
rather than inventing acceptance. Torn transaction tails are repaired before
recovery appends. File and parent-directory sync are checked at each boundary.

Validation: all 64 event-WAL tests pass, including eight new storage/custody tests;
25 inbox/migration and nine existing command callback tests pass. The tests cover
source acknowledgement only after destination acceptance, equal-body deliberate
operations, duplicate delivery, pending ordinary ack policy versus progress,
body pruning before source acceptance, receipt release, sequence collisions,
compaction/restart, torn tails, and injected file/directory sync failures at
intent, destination acceptance, source acceptance and release. Git-layer boundary
checks pass (2); formatting and whitespace checks pass.

These are storage primitives, not yet a user-facing dispatcher. Their allocating
calls require the existing local message writer lock; the source also requires
its author dispatch lease. The next milestone supplies those owners, CLI
`events reply`, deterministic ack IDs versus random deliberate IDs, strict
existing-ID retry policy, endpoint/binding validation, and acceptance-before-
release orchestration. It must reconcile/sync a visible stored acceptance before
suppressing another delivery, and check for existing destination acceptance before
refusing a subsequently removed recipient. Outbound detail/timeline projection
and wake-state rendering still follow. Nothing is installed or run against live
message state in this milestone.

## Milestone: explicit CLI dispatch and retry ownership

`events ack --reply` now dispatches message replies using a deterministic
operation ID tied to the replying repo/author and original sequence/identity.
`events reply` allocates a fresh deliberate operation, even for equal bodies;
`--operation` retries only a retained existing operation with identical immutable
input. Unknown/pruned IDs and changed bodies fail before delivery. Deliberate
replies leave the original acknowledgement unchanged. Existing command-backend
callback behavior stays in its separate dispatcher.

The synchronous dispatcher owns one nonblocking per-author runtime lease for
the entire transaction. Each WAL phase acquires only that repository's existing
message writer lock and releases it before touching the next repository. There
is no background retry worker, polling, or cross-repository lock nesting. It
persists source intent, confirms destination acceptance, persists source
acceptance (including an ack-purpose decision), then attempts receiver-pin
release. Failed cleanup does not undo acceptance. Failed delivery preserves the
intent and prints its operation ID and explicit retry instructions.

Explicit reply routes stay frozen. Ordinary messages may use their saved
sender's label/repository as a concrete endpoint; legacy nested sender paths
are resolved to their canonical Git root before creating the intent. Human
messages without an endpoint fail clearly. Retrying a retained operation reuses
its saved endpoint/time/body rather than deriving them again. New destination
acceptance checks the exact saved Git root and current concrete recipient;
existing destination acceptance is reconciled before roster validation, so
removing a recipient after a crash does not erase the accepted delivery.
Accepted source retries re-sync before suppressing delivery. Missing/deleted
destinations are not recreated by transactions or pin cleanup.

`events show` exposes retained reply operation IDs, exact bodies, purposes,
endpoints and pending/accepted status in text and JSON. Read projection groups
operation history once by original sequence/identity and folds accepted ack
decisions from those same records. README documents sending reply-required
messages, ack replies, multiple deliberate replies, retry IDs and mixed-writer
deployment constraints.

Validation: 36 event CLI tests, 64 event-WAL tests, 25 inbox tests, nine send
tests and two Git-boundary checks pass. Eight dispatcher tests cover actual CLI
parsing, immutable acknowledgement retries, repeated equal-body deliberate
replies, unavailable/restored recipients, accepted-delivery recovery after
recipient removal, legacy sender fallback, lease contention, deleted destination
cleanup and concurrent reciprocal replies without deadlock. Formatting and
whitespace checks pass. Nothing is installed or dispatched against live message
state by this milestone.

Still required: outbound timeline projection (including external-team replies),
durable presentation evidence, the pending/woken/acked icons and mail glyph,
and end-to-end adapter/rendering validation. This milestone does not claim any
of those presentation changes are complete.

## Milestone: outgoing custody in the shared timeline

Accepted message operations now project as message timeline entries even when
the parent inbox body has aged out. They are read from the same parsed local WAL,
not by scanning a destination repository or reading it again for each operation.
Pending intents remain event-detail recovery state, without a false Sent row.
Each entry retains exact body/time and typed outgoing provenance: source author
and repository, concrete destination, operation/delivery IDs, original copy key
and handle, and ack-versus-deliberate purpose. `clank log --json` exposes these
fields along with real recipient members.

Delivery aliases are scoped by the canonical destination repository, concrete
recipient and immutable delivery ID. In this repo, source custody and its actual
recipient inbox copy merge once. Different deliberate operations with equal
bodies remain different rows. Only inbound members decide `unhandled`, baseline,
seen-by and acknowledgement targets. An external outgoing-only row has no local
recipient members and carries Sent/custody, never a manufactured handled tick.
Removing/offlining the destination does not hide or recreate it while rendering.

The TUI, CLI log and remote ledger use the conventional `✉` mail badge. Outgoing
rows name their destination and show `sent →`; the TUI uses an arrow in its state
gutter. Enter opens their full, scrollable body and wrapped provenance in the
event detail page. Page navigation now has a typed `EventKey::Outgoing` separate
from `EventKey::Inbox`, preserving refresh identity without fake selectors or
ack targets. Outgoing-only pages offer Back, with no ack or remote body fetch.
Same-repo pages retain their actual inbox actions. Remote ledger text/glyphs stay
escaped; this milestone adds no remote body-fetch or reply editor.

Validation: 254 GitHub/event-WAL/timeline tests, 23 CLI-log tests, 478 TUI tests,
36 event CLI tests, two Git-boundary checks and two remote-timeline boundary
checks pass. Seven new custody/projection/
page tests cover unavailable external repositories, local merge and ack state,
pending intents, equal-body distinct replies, accepted records after parent-body
pruning, JSON provenance, refresh/navigation, mail/Sent markers and narrow-width
detail wrapping. The focused real Chromium `messages.js` check passes all four
assertions using synthetic frames; no authenticated session was changed.
The older general `check.js` was also attempted and fails its existing navigation
expectations (including a three-tab count and old plan-link behavior); it is not
reported as a passing browser suite. Formatting and whitespace checks pass.

Still required: durable presentation stamps at the successful adapter output
boundary, pending/woken/acked state projection and single/double ticks across
CLI/TUI/remote detail, merged-copy wake behavior, and complete end-to-end adapter
validation. Nothing is installed or migrated in live repositories here.


## Completion and requested discussion boundary

The user directed that all remaining implementation be completed in one commit.
The earlier milestone sections above are historical implementation reports, not
permission to split this remainder. The queued
`master-and-reviewer-enforce-single-commit-plans` updates both roles with the
user's single-commit default and reviewer enforcement requirements.

After this plan clears review and is finalized, block further plan work for the
user's requested Grok integration discussion. The reported issue is that native
PreToolUse tags only bare `clank wait`, so a same-repository `--repo` is left
untagged and refused in automatic mode. Discuss why session wait slots and attempt
generations exist, why command rewriting was chosen, whether it is technically
necessary, and cleaner alternatives. Do not silently expand this message plan
into a Grok handoff redesign or begin the next queued plan before that discussion.


Additional user steering folded into this remainder: metadata-only discovery and
explicit receipt-aware content reads belong here, rather than another queued plan.
Grok's Anx report was acknowledged (`messages@8`). Its absent log reviews are
caused by command-reply lock/owner sidecars directly under `.clank/agents`: the
feedback scanner attempted `<file>/feedback`, returned ENOTDIR, and the log's
best-effort review collector discarded the entire scan. This remainder filters
non-directory entries, with a regression fixture containing both real feedback
and reply sidecars. It fits reply/timeline custody; no Grok handoff change is made.

## Final implementation and verification

The remaining work is submitted together, in one implementation commit:

- Each accepted selection carries an exact `(source, seq, ident)` presentation
  key through backlog replay, the shared monitor and the actual delivery adapter.
  This is transient selection metadata, not stored event content. WAL ingestion
  refuses an already-stamped payload rather than changing keyless identities.
- Successful output and flush, with a still-current binding, append the first
  presentation timestamp to the existing source WAL. Codex, Claude and OpenCode
  Stop wakes, Claude SessionStart context and registered native Grok handoffs use
  the same local evidence writer. Grok's registration/selection alone, silent
  control responses, ordinary waits, peeks and metadata inspection earn nothing.
  Binding is checked before output, after flush and under the WAL write lock.
  Callback absence/failure never fabricates or erases local evidence. A crash or
  fsync failure after real output can conservatively leave an unwoken marker.
- Evidence is discriminated by exact identity, survives restart and ordinary or
  recurring compaction while its parent is retained, and never acknowledges work.
  Repeated delivery keeps the first timestamp; the source batch holds one short
  lock and performs one read and durable write/sync sequence. There is no idle
  worker, polling, detached receipt process or automatic retry queue.
- One delivery projection drives CLI log/JSON, TUI and remote ledger: pending
  copies without evidence keep the merged row unwoken; otherwise pending copies
  are woken; all acknowledged copies are acknowledged. Detail keeps each copy's
  own timestamp. Outbound-only custody is Sent, never a fabricated remote wake or
  acknowledgement. Double ticks have their own terminal width; envelope and
  state survive narrow layouts and escaped web rendering.
- `events list` and `events show` now use an allowlist in both text and JSON.
  Message subjects/bodies, command/timer content, watch instructions, callback
  argv and reply text are excluded. `events read ID [--json]` is the content
  boundary for a currently bound agent session; a foreign author override or stale
  binding is refused. Successful read output earns local evidence and the existing
  configured command-source receipt callback, without changing reply obligations.
  New command rows preserve the trusted source-host cwd even for receipt-only
  backends. Older rows can use saved reply-route cwd; missing historical context
  skips the callback with a diagnostic. Raw filesystem/timeline views remain
  outside receipt instrumentation; receipts prove delivery, not model cognition.
- Shipped master/reviewer message guidance, the GitHub inbox skill and event nudge
  text direct agents to `events read`. The reviewer skill remains within its
  existing size budget; the single-commit policy changes remain in their own
  queued plan. No installed skills or live agent setup were modified here.
- Accepted reply-body pruning now leaves a small retained-parent flag so a full
  read reports incomplete history rather than implying it is complete. It uses
  the existing retention horizon, preserves pending operations and drops with its
  parent. `events reply -m -` now uses the shared stdin substitution path.
- Reply lock/owner files no longer enter the feedback-directory scan. The new
  regression includes real short-SHA feedback alongside both sidecar forms. The
  built CLI was also run read-only against Anx's original Git range: it now renders
  Grok's CONTINUE review on e2fcdb2 and FINISHED review on f19cada; the installed
  binary had omitted both. No Anx feedback/history or sidecars were edited.

Validation completed:

- The CLI library sweep verified 2,248 tests with three ignored and identified one
  skill-size failure. After shortening the guidance without raising its budget,
  all 70 setup tests passed, including that sole failure. Together all 2,249
  enabled CLI library tests are verified. Core: 262 passed; web: 24 passed;
  Cloudflare web: 38 passed, three ignored. Initial sandbox-only pipe/socket
  failures were rerun with the fixture permissions they require.
- Seven real producer/WAL/wait/hook/read/callback integration tests pass, including
  receipt-only command sources, inspection without content, failed required ack,
  successor routing, shared monitor context and recurring native deadlines.
- Two Git-boundary checks pass. Five focused real-Chromium timeline assertions
  pass for all delivery markers, mail badges, external reply provenance and
  escaping. This is the focused messages fixture, not a claim that the older
  general browser navigation suite passes.
- The explicit-read tests cover lossless JSON content, no content in discovery,
  first-delivery persistence, no acknowledgement, repeat reads, failed writes and
  flushes, foreign-author refusal, and rebinding before/during output. WAL tests
  exercise exact-copy collisions, restart, compaction and uncertain sync recovery.
  Formatting and whitespace checks pass.

No binary, hook or live repository migration is installed at this submission.
Review and finalization are still required. After finalization, stop further plan
work for the requested Grok handoff discussion before promoting the next item.
