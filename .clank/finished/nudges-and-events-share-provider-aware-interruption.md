# nudges-and-events-share-provider-aware-interruption
# Nudges and events share provider-aware interruption

## Problem and outcome

A nudge must reach an agent that is busy, including interrupting its current turn. Grok uses Ctrl+C; other harnesses may use Escape. Events should optionally request the same interruption. Share targeting and interruption in a reusable module, while retaining nudge's independent recovery path.

## Contracts

- Resolve the actual live session, pane and provider, including fork/provider overrides. Preserve rejection of ambiguous, exited or mismatched targets. Never interrupt an unrelated pane.
- Research current supported control channels for each supported harness, alongside Zellij keystrokes. Document which can interrupt without Zellij and what their completion guarantees mean. Do not infer success from a process signal or accepted keystroke alone.
- Make message submission and interruption an explicit provider-specific sequence. Usually submit the nudge message first and then send the interrupt key. Verify that this cancels the running turn without discarding the submitted message; where a provider requires different ordering, implement and document its tested sequence.
- Nudge uses this sequence to interrupt ongoing work. Keep its recovery operation usable when ordinary event waiting or hooks are broken.
- Add an optional interrupt setting to event creation and source configuration, carried through persistence and delivery. Default is off and existing events remain compatible. Define its behavior for messages, command sources and scheduled/recurring events rather than silently dropping the option.
- Event persistence and normal delivery do not depend on an interrupt transport. Without Zellij or another supported control channel, deliver the event normally. Transport failures must not erase the event, acknowledge it or fabricate a wake receipt.
- Initiate optional interruption when a newly eligible event is durably available to its recipient, including while no wait is armed. Do not put the only interrupt trigger behind waiter delivery: a busy agent is precisely the case this feature must reach. Future scheduled events must not interrupt before they are due. Define catch-up and recipient changes against the same logical-delivery identity.
- An interrupt request is separate from event delivery, wake receipt, acknowledgement and required reply. Report attempted, unavailable and failed interruption accurately. Define once-per-delivery identity and retry behavior so replay, reconnect and repeated waits do not repeatedly interrupt the agent.
- Use the same reusable targeting/provider/transport code for nudge and event interruption, without making nudge depend on a healthy inbox. No continuously running polling, repeated pane scans, per-event idle tasks or speculative background helpers. Work happens on an explicit nudge or eligible event delivery.

## Implementation and validation

First map each harness's documented cancellation and input semantics using current primary sources and concrete terminal experiments. Extract the shared operation, add the event option end to end, then wire both callers to it. Test ordering, provider-specific keys, fork overrides, ambiguous targets, busy agents, no Zellij, failed transport, reconnect and duplicate delivery. Include a live busy-Grok acceptance test proving the submitted instruction survives cancellation; clearly distinguish fixture coverage from authenticated live results. Document supported transports and graceful fallback.

## Initial code trace

`cli::nudge::run` locates a live pane independently of inbox/session-binding state,
then calls `open_zellij::say_to_pane_for_tool`; it currently does not call the
existing `stop_pane` operation. That operation already selects Ctrl+C for Grok
and Escape for other tools. Extract and validate this existing knowledge rather
than introducing a second key table. Preserve Codex's provider-specific submit
sequence. Trace `monitor`, `inbox`, `command_source` and `timer_source` for durable
eligibility and recovery before choosing the shared interrupt initiation boundary.
Provider detection for recovery nudges must remain usable with broken bindings;
stored configuration alone must not masquerade as proof of the live pane's tool.

## Research milestone: control channels and implementation contracts

Primary references checked on 2026-10-04:

- Claude interactive mode: https://code.claude.com/docs/en/interactive-mode
  Enter queues a message while busy; Escape interrupts and sends queued messages
  next. Escape is context-sensitive (dialogs/footer selection), so accepted bytes
  alone cannot prove cancellation. Use a single Escape, not double Escape, which
  can clear a draft or open rewind. This supports submit-then-interrupt for the
  ordinary busy prompt; live contextual acceptance remains required.
- Grok shortcuts: https://docs.x.ai/build/keyboard-shortcuts
  Ctrl+C is documented to cancel a turn. Current documentation also lists Escape;
  preserve Ctrl+C for the installed version and reported behavior rather than
  treating all Grok versions as identical. Queue survival after cancellation must
  still be tested; the shortcut table does not prove that sequence.
- Grok machine interfaces: https://docs.x.ai/build/cli/headless-scripting
  ACP operates over the running agent's stdin/stdout. ACP cancellation is specified
  at https://agentclientprotocol.com/protocol/v1/prompt-turn. Starting a separate
  ACP process or loading a persisted session into it does not control a live TUI.
  No existing attachable TUI control channel was established by this research.
- Codex: https://learn.chatgpt.com/docs/app-server
  turn/interrupt addresses threadId and turnId; cancellation completion is a
  turn/completed notification. turn/steer adds input but is not cancellation.
  Existing codex_chat already connects to a local control socket and checks the
  thread is loaded before mutation. Its caller-home socket selection and release
  polling are not appropriate to copy into this feature. Use the target profile
  and preserve completion notifications instead of adding a polling loop.
- OpenCode: https://opencode.ai/docs/server/
  POST /session/:id/abort addresses a session on its owning server. An API route
  does not establish discovery of the correct running server. The installed plugin
  has a live SDK client, so investigate reusing that client for control rather than
  spawning another server or guessing a port. Native transport remains contingent
  on proving ownership and exposing an authenticated local control path.

### Shared operation and target identity

Extract terminal submit and stop primitives from open_zellij into one reusable
module, leaving compatibility callers as thin wrappers. Keep one provider key
table and Codex's submission encoding. Separate operations for submit-and-interrupt
(nudge) and interrupt-only (an already persisted event); do not insert an event's
body into chat and then deliver the same body again through the inbox.

The shared API takes an explicitly resolved live target, not a label plus ambient
provider environment. Target evidence must identify repo, label, live pane/session,
actual provider, and native endpoint/session where applicable. Recovery nudge's
pane lookup retains exact launched-command matching and ambiguity rejection.
Zellij's terminal_command identifies the clank launcher, not the tool it later
executes. resolve_tool is roster-only and cannot verify the live provider. Before
wiring nudge, establish a corroborated live-provider source that survives broken
bindings, including swapped/forked providers; do not silently fall back to a stale
roster. This is an explicit implementation prerequisite, not resolved evidence.

For native transports, validate that the exact thread/session is currently owned
by that endpoint; never resume or create a thread to make cancellation possible.
Native request acknowledgement is Accepted, not ConfirmedStopped. Terminal
transport reports Submitted or Failed, never ConfirmedStopped without evidence.
Return partial outcomes when nudge submission succeeds but interruption fails;
the user must not be told to resend a message whose submission already succeeded.
Do not send an interrupt after submission fails. Never fallback to another
transport after a mutation has an ambiguous outcome, risking duplicate submission
or cancellation of a replacement turn. Bound explicit I/O and preserve errors;
no retry loops over all sessions or fixed-delay attempts to guess completion.

### Event policy, custody and identity

Add a backward-compatible optional interrupt policy, false when absent, to message
creation and supported source configurations. Serialize it with durable event
custody, separate from WaitItem's display payload and acknowledgement. Source
policy must be captured for an occurrence: a later config change must not silently
reinterpret an old pending event. Define corresponding CLI --interrupt controls
for send and schedule and the typed source field for command/GitHub/timer sources.
Scheduled future entries become eligible only at due time; recurring catch-up
uses its existing debounced occurrence identity.

Use the existing WAL identity (source plus stable entry/action identity and
recipient), not timestamp, subject, sequence alone or waiter ID. Persist an
interrupt claim before issuing a side effect, under a short writer lock, and
perform network/terminal work outside the lock. Record outcome separately. Normal
replay never reclaims an already attempted delivery; changing the target session
does not turn one pending event into permission to cancel every replacement.

Exactly-once external cancellation cannot be guaranteed across a crash between
claim and side effect. Choose at-most-one automatic attempt per logical recipient
delivery: an interrupted claim is indeterminate and not automatically repeated.
An explicit later nudge remains available. Unavailable transport is a recorded
no-op for this attempt, not a timer that keeps probing until a pane appears. The
durable event remains pending in all cases. Explicit receipt and ack paths retain
their existing semantics; interruption never marks an event woken or handled.
Test compaction retains claim/outcome for retained pending entries and malformed
or old entries default safely. Keep the marker within existing runtime storage.

### Trigger ownership and interruption-to-wake handoff

inbox::deliver appends messages under its writer lock and returns; command_source
and timer_source append before emitting a Logged item. monitor receives source
emissions regardless of connected clients, and watches message log changes. Use
these existing custody notifications and recovery scans. Do not put interruption
behind serve_to_cached/admit_into, which require an armed waiter. Define one
owner for each delivery's claim so send, monitor and restart cannot race separate
attempts. A sender-side direct attempt must coordinate through the same claim;
it must not require a continuously running monitor to make an explicit send useful.

For a running monitor, process newly eligible durable entries and unresolved
unclaimed pending interrupt entries on existing log-change/startup boundaries.
Batch target discovery once for an eligible batch; no scan for non-interrupting
events and no idle interrupt scan. Execute bounded work without blocking the
monitor event loop or retaining filesystem locks. Reuse existing task ownership
and shutdown handling, with no detached immortal control worker.

An interrupted agent still needs a route to inspect pending work. Trace each
provider's cancellation lifecycle before assuming its normal Stop hook runs.
OpenCode's plugin currently intentionally suppresses rearming after
MessageAbortedError to respect a human stop. An event-requested cancellation must
be distinguished through an owned pending-work intent, allowing that specific
delivery's continuation without making human cancellations restart the agent.
Likewise test Grok StopCancelled and its background handoff, and Codex's held
Stop hook. Do not cancel Clank's delivery waiter as if it were arbitrary tool
work. If no supported continuation is possible, report the limitation and keep
the event pending; do not fabricate wake receipts or inject speculative turns.

### Implementation milestones and proof

1. Establish live-provider evidence and extract the reusable transport operation;
   wire nudge with precise partial outcomes and deterministic ordering tests.
2. Add event policy, durable claim/outcome and source/message/timer propagation;
   test legacy decoding, compaction, concurrent claims and crash boundaries.
3. Wire eligibility notifications and provider cancellation-to-wake handoff;
   cover busy/no-wait cases, no Zellij/native transport, catch-up and replay.
4. Run isolated terminal experiments plus live busy-Grok acceptance, verifying
   submitted text survives and pending work is actually surfaced. Do not interrupt
   a reviewer that is writing feedback for this plan. Document observed versions,
   unsupported contexts and evidence limits; refresh installed binary as needed.

This milestone is research and contract refinement only. No interrupt side effects
were issued and no production cancellation behavior has changed yet. Live-provider
corroboration, transport integration and authenticated queue-survival evidence
remain implementation work for subsequent reviewed milestones.

## Milestone: shared terminal input operation

Extracted provider-aware submission encoding, cancel keys and submission sequencing
into cli::pane_input. Nudge and remote controls call this shared module; raw Zellij
subprocess execution stays in open_zellij::write_pane_action, preserving the single
spawner boundary. There is no new scanner, timer, task or idle work. Codex's existing
single paste-window delay and kitty Enter encoding remain unchanged.

Added submit_and_interrupt for an explicitly resolved target. It submits before
requesting cancellation, rejects empty input without side effects, and does not
interrupt after a failed or uncertain submission. Submission failure warns that
some input may already have been typed; interruption failure explicitly says the
message was submitted and must not be automatically resent. Terminal acceptance
still does not prove that the harness stopped or consumed the message.

Validation passed: four ordering/partial-outcome unit tests, existing cancel-key
and submission-encoding tests, nine nudge tests, the remote target-provider test,
three Zellij ownership boundary tests, the Zellij cost boundary and both Git
boundary tests. Formatting and whitespace checks passed. No live interrupt was
issued and this source milestone is not installed.

Nudge behavior deliberately remains submit-only in this extraction milestone.
The new combined operation is not wired to nudge until live-provider corroboration
and queue-survival evidence are established. Existing raw Zellij I/O remains
synchronous and lacks an explicit deadline; bounded event-loop-safe execution is
still required before monitor integration. Durable event policy/claims, eligibility
triggers, native channels and cancellation-to-wake handoff remain subsequent work.

## Milestone: launch provenance and native process corroboration

agent start now atomically publishes the actual composed launch tool, configured
executable, Zellij session/pane and its kernel process-birth token immediately
before exec, after preview and setup checks. The private uid runtime namespace
holds one fixed slot per repo/label/session/pane identity. Different panes cannot
overwrite each other's evidence. Records contain no prompt, credentials or launch
environment. A publication failure reports unavailable interruption evidence but
does not prevent the provider from launching. Non-pane launches write nothing.

Nudge's independent live-pane lookup now resolves provider input encoding from
that record rather than the mutable roster. It checks the exact pane/session,
record schema and kernel birth, and corroborates the running executable using
existing proc_identity::inspect. The pre-exec clank process, ended/zombie process,
wrong pane, unsupported schema, changed executable or unreadable observation does
not authorize input. No hooks, inbox or session binding are consulted. File reads
reject symlinks and unsafe ownership/permissions, and bound metadata parsing;
publication replaces a symlink instead of following it. Native inspection reads
only the recorded PID without subprocesses, process-table scans, timers or retries.

This proves the selected managed launch is still present, under the provider name
or configured executable, rather than recognizing every arbitrary wrapper. A
wrapper that changes executable identity beyond those names, or starts a separate
child and exits, is refused as unverified. Older running sessions without a launch
record require a restart through the refreshed clank launcher before this lookup
can work. These limitations must be handled/documented in final rollout, not
hidden by a roster fallback. Process/pane observations are not an atomic lock over
future exec or terminal changes; transports still must report only their observed
acceptance, never inferred completion.

Validation: six target tests passed, including per-pane isolation, stale/wrong
identity rejection, pre-exec/changed executable rejection, symlink publication and
a real native birth/argv inspection of the test process after publication. The
last is kernel/record evidence, not a real Grok provider or model-wake acceptance.
All 85 agent tests, nine nudge tests, two launch acceptance tests, three Zellij
ownership tests, the Zellij cost boundary and both Git boundary tests passed.
Formatting/whitespace checks passed. A direct live pane query had no active
session in this shell, so no live interruption or queue-survival experiment was
performed. No installed binary has been changed by this milestone.

Nudge remains submit-only until queue-survival acceptance; shared cancellation is
not wired yet. Bounded asynchronous transport, durable event policy/claims,
eligibility triggers and provider cancellation-to-wake handoff remain required.

## Milestone: bounded asynchronous terminal transport

Added write_pane_action_async in the sole Zellij spawner module. It uses an owned
client process group, drains stdout to a sink and retains only the last 4 KiB of
stderr. A three-second per-action deadline covers process completion and both
pipe drains; client descendants retaining a pipe cannot hold the request open.
Deadline/I/O failure terminates the owned client group and attempts bounded
reaping. It never signals the agent process directly, nor starts another server.
Timeout is reported as unknown acceptance and never retried automatically.
Diagnostics do not echo the submitted text or entire input argv.

pane_input now has async submit, interrupt-only and combined operations. The
combined path uses one async sequencing helper for ordering and partial outcomes;
the unused synchronous combined path was removed. Terminal key/submit encodings
remain shared with legacy remote controls. The existing Codex paste-window wait
is an asynchronous one-shot delay here, not a polling loop. Legacy synchronous
remote controls remain unchanged; monitor integration will use the async path.

Nudge is now async at command dispatch. Existing bounded pane discovery plus
native target corroboration run in one explicit spawn_blocking invocation, while
submission uses bounded async writes. This adds no idle task. Nudge still does
not send cancellation until queue-survival acceptance is established.

Validation passed: two transport tests (large discarded stdout/failure stderr and
descendant-held pipes), four async sequence/partial-outcome tests, nine nudge
tests, ten exact command-dispatch/parser tests, three Zellij ownership boundary
tests, the Zellij cost boundary and both Git boundary tests. An initially broad
cli::command filter also selected command_source tests; its existing native
process look test failed under sandbox process restrictions, then passed in a
targeted rerun with native access. No production source changed for that test.
Formatting and whitespace checks passed. This source milestone is not installed.

Still required: queue-survival and busy-agent acceptance, nudge cancellation
wiring, event policy/claims, eligibility initiation, native fallback selection,
provider cancellation-to-wake handoff and final installation refresh.

## Milestone: durable interruption policy and attempt ledger

Inbox WAL records now optionally carry interrupt policy, absent/false for old
records and ordinary append callers. A separate interrupt record claims the exact
sequence plus canonical logical identity; its optional outcome records Submitted,
Unavailable, Failed or Indeterminate without modifying the event payload, reply,
receipt or acknowledgement. Unknown future outcomes remain non-retryable evidence.
Inspector rows expose the captured policy and attempt independently of ack state.
Conflicting physical copies conservatively suppress interruption permission.

claim_interrupt takes the existing exclusive sidecar lock, re-reads the actual
row, refuses a missing/wrong identity and skips ordinary, already handled or
already claimed entries. Its append is file/directory synced before returning
permission for a side effect. A complete claim whose sync failed remains a claim:
retry syncs the evidence but does not grant another attempt. Outcome completion
requires a prior claim; same-outcome retries are idempotent and conflicting
outcomes are refused. All network/terminal work will occur after this transaction
releases the lock. There is no separate sidecar ledger, scan or retry timer.

Generic compaction preserves claims/outcomes for retained pending and recent
handled logical rows and drops them when that row ages out. Recurring strict
recovery recognizes the same secondary records only for an existing interruptible
occurrence, and its compaction preserves them with the retained occurrence. Old
recurrence append paths still default to non-interrupting; source policy propagation
is subsequent work. A crash leaving a claim without outcome remains observable
and never grants automatic replay permission. Durable custody stays pending.

Validation passed: 55 WAL tests (including seven new interruption/recurrence
cases), 24 inbox tests, 28 event/reply tests, 21 timer-source tests and both Git
boundary tests. New coverage includes four concurrent claim writers yielding one
winner, failed durability sync, ordinary/acked/wrong-identity refusal, immutable
outcomes, retained handled rows, and pending/recurring compaction and restart.
Formatting and whitespace checks passed.

This milestone introduces internal persistence APIs only, not a CLI option that
pretends to interrupt before dispatch exists. Source/message/schedule policy
propagation, eligibility-triggered ownership/dispatch, cancellation-to-wake
handoff, nudge cancellation acceptance and installation remain open. No interrupt
was issued and the installed release is unchanged.

## Milestone: capture source policy at ingestion

Command, GitHub and timer source configuration now accepts `interrupt`, defaulting
to false and omitted when false. The policy does not change source identity.
Command producer lines may override it per event with `interrupt: true|false`;
absence inherits the source default. The chosen value is captured in the inbox
append, before emission. Replayed producer IDs do not rewrite that value. Source
failure notices deliberately remain non-interrupting even for an interrupting
source. GitHub poll and relay share the same policy at their existing emission
gate, while baseline history remains non-interrupting.

One-shot and recurring timers capture policy only when accepting a due occurrence.
A pending occurrence retains its policy across source changes and restart, without
minting another occurrence. The existing JSON timer configuration and `wait --event`
interface carry the option; this milestone does not invent another scheduling CLI.
The inbox also provides a message append API that captures policy while retaining
normal pending delivery. Ordinary sends and legacy message migration remain false.
The public `send --interrupt` flag will be added with actual dispatch, so an option
is not advertised before it can perform the requested operation.

Validation passed: 19 core configuration tests, 20 command-ingestion tests, 25 inbox
tests, 22 timer-source tests, 14 recurring WAL tests, 44 coordinator tests, five
wake-receipt acceptance fixtures and both Git boundary tests. The new coverage
checks producer overrides in both directions, source-failure suppression, baseline
suppression, duplicate replay, message custody, and timer-policy retention after
restart. The timer test initially used the one-shot storage key for recurrence;
that test was corrected to use the recurrence key. The existing relay and receipt
fixtures needed local socket access and passed outside sandbox restrictions.
Formatting and whitespace checks passed.

This is persistence propagation, not active interruption dispatch. There is no new
idle task, timer or poll. No agent was interrupted and the installed release is
unchanged. Eligibility-triggered dispatch and cancellation-to-wake handoff remain
next, followed by nudge cancellation acceptance and installation refresh.

## Milestone: shared targeting and batch dispatch transaction

The async pane-discovery operation and its exact, ambiguity-rejecting selector now
live in pane_target alongside launch provenance. Nudge uses that shared operation;
its independent recovery path still requires no hook, binding, inbox or role. The
single scan remains bounded and the live process corroborates the provider. This
is a relocation of the existing selector, not another discovery mechanism.

The event_interrupt dispatcher takes one recipient's event directory and one
provider driver. It reuses the existing event inspector to discover logical rows,
selects only pending interruptible unclaimed entries, then re-checks and durably
claims each under the exclusive WAL transaction. Candidates across sources are
coalesced into one driver call. No eligible claim means no driver call, including
no target lookup. Concurrent callers cannot attempt the same event twice.

Claim and outcome persistence run in explicit spawn_blocking operations; no
terminal or network I/O runs under a WAL lock. The driver is awaited under its
caller's request deadline, and is required to own/cancel its I/O on drop. Timeout
records Indeterminate, not Failed. Caller cancellation or a crash retains an
unconfirmed claim and grants no automatic retry. Completing any outcome leaves
custody, receipts, required reply and ack independent. If result persistence fails,
the report contains errors and the existing claim still prevents replay. A corrupt
source is diagnosed and skipped without quarantine or repair, while healthy rows
remain eligible. The normal inspector is still the owner of that read contract.

Seven dispatcher tests pass: cross-source batching, lock release during transport,
concurrent contenders, unavailable/failed attempts, timeout/drop, caller cancellation,
already-acked suppression and corrupt-source isolation. Nine nudge selector tests,
six target/provenance tests, 28 event/reply tests, all three Zellij ownership boundary
tests and both Git boundary tests pass. The kernel process fixture ran with native
inspection access. Formatting and whitespace checks pass.

The dispatcher is intentionally not admitted by the live monitor or sender yet.
Grok StopCancelled currently cancels its automatic wait and OpenCode marks aborted
turns as human stops; cancellation alone is not sufficient to wake either harness.
A provider driver must arrange the specific cancellation-to-wake handoff before
eligibility admission is wired. Monitor integration must own dispatch tasks through
retirement, distinguish event invalidations from ordinary heartbeat beats, and
never put target scans on the idle fold. Explicit startup catch-up and source/due
notifications will use this same dispatcher. Nudge queue-survival acceptance,
provider drivers, public send option and installation remain open. No live agent
was interrupted and no installed files changed in this milestone.

## Milestone: live Grok queue survival and nudge cancellation

The earlier lack of visible Zellij sessions was a sandbox restriction, not a
missing server. A native read listed the live local sessions. The existing Grok
reviewer had completed review and cooperated in a controlled probe in its own
bound session, not another repo's agent. Installed Grok was 1.0.46 (2765805b9442).
A one-shot background controller verified GROK_SESSION_ID, repository cwd and
Zellij session, then listened on a Unix datagram socket. The disposable foreground
tool sent the busy cue. After that cue the controller submitted a new nonce prompt
with write-chars, Enter (13), then Ctrl+C (3), each bounded by a three-second client
deadline. It used no process/task polling. Only the disposable probe was interrupted.

The controller recorded session clank-clank, pane 4, sequence text-enter-ctrl-c.
Native chat history then contained a NEW user message beginning "The user
interrupted the previous turn", containing the queued probe 4967 instruction.
That native content's SHA-256 was
ef19e8858838f1e4e7aeb9924645af3ea892c35263aac9ccd6dcc6e352a48e18.
The authenticated Grok model handled that new instruction and wrote the survived
artifact with nonce 4967. It independently reported the outcome through messages@6,
which the master read and acknowledged. It resumed normal automatic waiting.
Artifacts are under /private/tmp/clank-grok-interrupt-probe for this session.
No repository files were changed by the probe.

The harness's old-tool result was user_cancel and said the tool was not executed;
Grok also reported cancellation before the sleep ran. The busy cue proves the
probe reached the foreground tool path, but this does NOT establish that a running
sleep subprocess was killed, nor that arbitrary subprocess descendants terminate.
The established acceptance is the required queued instruction surviving a busy
native turn's cancellation and being handled in the next native user turn.

Nudge now calls the bounded async submit_and_interrupt operation rather than
submit alone: provider-specific submission first, then Ctrl+C for actual Grok or
Escape for other actual providers, using verified live launch provenance. Failed
or uncertain submission never sends cancellation; partial submission/cancellation
errors explicitly forbid automatic duplicate sends. Existing default key and
Codex paste/kitty-Enter behavior remain shared with other controls.

For event handoff, pane_input now has a wake_for_events operation using the same
sequence with one fixed brief inbox instruction. It submits no event body, selector,
role snapshot or reply, and does not write a receipt or acknowledge anything. This
fresh user prompt crosses cancellation hooks without depending on StopCancelled to
inject context: Grok may cancel an old automatic waiter, but the retained prompt
starts a new native turn that can read durable work and establish its next wait.
The hint is independent of healthy hook metadata. It deliberately instructs the
agent to read pending events before acknowledging and honor required replies.

Four input sequencing/partial-outcome tests, nine nudge target tests and three
Zellij ownership boundary tests pass. Formatting and whitespace checks pass. Live
Grok acceptance is separate from these fixture results. Nudge is changed in source;
the installed binary and hooks are still unchanged pending the whole plan rollout.

Codex protocol was rechecked against https://learn.chatgpt.com/docs/app-server:
turn/interrupt requests cancellation and turn/completed reports its end, but
thread/read does not subscribe to notifications. A notification-based native driver
must establish an ownership-safe subscription; merely opening a control connection
and waiting for completion is insufficient, and thread/resume must not silently
load a persisted thread into another server. Do not copy the existing remote-chat
release polling as a substitute. Native fallback and other providers' busy-context
acceptance remain subsequent work. Event admission/monitor task ownership, public
send --interrupt and final installation remain open; the new event helper is not
connected to live ingress yet.

## Milestone: eligibility admission, owned requests and public sending

The monitor now admits interruption on startup catch-up, source emission (including
due timer acceptance), and event-WAL/inventory invalidation from its existing repo
watcher. It does not admit requests on ordinary heartbeat folds, git changes,
feedback or configuration sidecars. A dirty invalidation survives a failed fold.
The watcher adds only an atomic dirty signal to the existing callback, not another
watcher or poll cadence. Startup and event-directory invalidation cover messages
without a waiter; source channels give immediate admission for hosted producers.
Future timers have no accepted row and therefore grant no interruption.

Scheduler owns one task per recipient plus one coalesced dirty bit while that task
runs. Completion is collected explicitly and admits one follow-up if needed;
there is no per-event idle task or retry timer. OwnedTasks hands interruption
handles to retirement alongside producers and handshakes, cancelling and awaiting
them before the monitor can release its lease. Removed roster members fail the
current-role check before transport. The WAL claim still precedes target discovery
and I/O; no eligible claim means no pane query. Each valid batch uses the shared
verified-target and inbox-hint submission/cancellation operation. Transport failure
is diagnostic only; normal event custody and delivery remain independent.

`send --interrupt` persists and prints the message first, then uses the same batch
dispatcher as sender fallback, including when no monitor exists. Sender/monitor
contenders share the same durable per-event claim. The default remains false.
CLI send is async to await owned control I/O, and the command dispatch follows it.
Interruption outcomes are available through event JSON and attempt diagnostics.
README documents source and producer policy, timed eligibility, graceful fallback,
legacy-pane provenance requirements, partial outcomes and at-most-one automatic
attempt. This milestone uses verified Zellij control; it does not create or resume
another native provider process to control a live session.

The earlier spawn_blocking persistence/discovery scopes could outlive an aborted
owner because dropping a JoinHandle does not cancel blocking work. Local WAL and
kernel-provenance work now use block_in_place on a multi-thread runtime (direct
scopes on small current-thread tests), so the owning task cannot end while its
blocking scope still owns resources. A multi-thread cancellation test proves that
property. This keeps unrelated owner-loop work scheduled without detaching an I/O
job during retirement.

The old Zellij global-list query and pipe-reader joins could block beyond their
nominal scan deadline. Shared control discovery now uses the existing bounded
async query transport for the global listing and concurrent per-session listings.
Every query owns its client process group and pipe drains, including when cancelled.
No reader thread or new polling loop is added. Legacy layout discovery stays as-is;
only nudge/event targeting moved to the scoped async path. The driver has one
request deadline spanning target discovery, submit and cancellation; ambiguous
acceptance is Indeterminate and never grants automatic replay.

Validation passed: ten dispatch/ownership tests, eight watcher tests, seven send
tests, ten command/parser/README tests, the monitor inventory-retirement test,
three Zellij ownership tests, the Zellij cost test, both Git boundary tests and six
real CLI wake-receipt/shared-monitor fixtures. New tests cover 100 invalidations
coalescing into one active recipient task, one follow-up and no idle task, shutdown
ownership, scoped-blocking cancellation, unavailable interrupting sends retaining
custody, and the actual monitor due-event admission without a waiter. The latter
uses a fake Zellij executable returning no sessions: its query counter remains one
across the claim's own invalidation and a seven-second idle-heartbeat window. It
asserts the event remains unacked with Unavailable recorded and no receipt output.
The existing required-reply, receipt and recurring-monitor contracts continue to
pass. Formatting and whitespace checks pass.

This source milestone is not installed. Final rollout must refresh the binary and
managed integrations, account for old panes lacking launch stamps, and validate
remaining provider busy-context/held-hook behavior. The Grok live queued-turn
acceptance from the preceding milestone remains distinct from these isolated
fixtures. Native fallback discovery/ownership and non-Zellij local-wait behavior
must be explicitly assessed rather than assumed from the presence of a documented
API. No live agent was interrupted during this integration milestone.

## Milestone: one-shot native Codex fallback and non-Zellij local delivery

Added codex_interrupt as an owned async driver of the existing Codex control
socket. Event dispatch tries it after durable batch claims and before terminal
discovery. Nudge retains its binding-independent verified-terminal recovery path.
The target comes from the current binding's actual Tool::Codex/session identity;
the roster supplies candidate launch-profile paths, not proof of a live provider.
An explicit absolute target CODEX_HOME wins, otherwise target HOME/.codex is used,
with the controller HOME only as a default candidate. Caller CODEX_HOME is never
borrowed. Relative roots with unknown original launch cwd are refused. The socket
must be a same-user Unix socket rather than a symlink or ordinary file.

Read-only initialize, loaded-thread listing and thread summary must identify the
exact bound thread already loaded on that endpoint, with its canonical cwd equal
to the target repository. The latest-turn request asks for one row without items.
No thread/resume, thread/start, server start, history load or subscription is used
to make a missing session controllable. Unsupported/foreign/unloaded read-only
preflight results allow the existing verified terminal fallback. Role and exact
binding are rechecked immediately before cancellation and before submitting input.

This refines the earlier notification-subscription research contract using the
current primary implementation checked on 2026-10-04:
https://github.com/openai/codex/blob/main/codex-rs/app-server/src/request_processors/turn_processor.rs
turn_interrupt_inner with a nonempty turn ID checks the active turn, records the
correlated pending request and defers its RPC response until TurnAborted. The
empty startup-interrupt path is different and is never used here. Consequently a
notification subscription is unnecessary: await the correlated response, then
perform exactly ONE confirming latest-turn read. Only the same ID with completed,
interrupted or failed status permits the wake prompt. An immediate-ack older
implementation that still shows inProgress is conservatively refused without
polling. A replaced turn, unknown status or missing confirmation likewise refuses
submission. An idle thread receives the fixed inbox hint without cancellation.

Errors after a native mutation may have been issued are Indeterminate and never
fall through to a second terminal send. RPC rejection is conservatively treated
the same way. The dispatch's existing whole-request deadline owns connection,
handshake, all RPC waits and confirmation; timeout or retirement drops the socket.
There is no reader task, subscription owner, extra process, retry timer or idle
probe. Native Submitted reports accepted control/input, not proof of model reading
or termination of arbitrary subprocess descendants. Inbox bodies, required replies,
receipts and acknowledgement remain exclusively under their existing contracts.

The actual local fallback Stop-hook fixture now enables interrupt:true on its
required-reply command source. With the monitor lease held but no published
monitor, and a clean PATH/environment without Zellij, it accepts the source event
and emits normal blocking context directly. The persisted policy remains true;
there is no detached interruption attempt for this already-armed local hook.
Its normal wake receipt is emitted and ack without a reply is still rejected.
This proves graceful local delivery, not busy native-provider cancellation. A
busy agent's hosted-source requests remain the shared monitor's responsibility;
an explicit sender also retains its ledger-backed fallback without a monitor.

Other native transports remain deliberately unavailable where live ownership has
not been established. Grok ACP/headless launches own stdio sessions, not an
attachable control endpoint for an existing TUI. Claude's documented interactive
queue/interrupt keys remain the established channel. OpenCode's existing server
API can abort a session, but its plugin's SDK client is not an authenticated
attachable endpoint exposed to this dispatcher. Starting another server, guessing
a port or resuming a persisted session would violate ownership. These providers
therefore use verified Zellij control, or ordinary delivery when unavailable.
The primary provider references in the research milestone remain the basis of
this assessment. No speculative permanent service was introduced to fill a gap.

Validation passed: nine native-control tests, ten dispatch/ownership tests, seven
send tests, the real interrupting local Stop-hook/receipt fixture, both Git boundary
tests and three Zellij ownership boundary tests. Native fixtures cover loaded and
foreign ownership, busy/idle sequencing, one confirmation without polling,
replaced/unconfirmed turns, mutation rejection, binding changes, timeout/drop and
a real isolated Unix-socket WebSocket handshake selected through a typed roster
and binding. The socket fixture initially used a six-character session ID; it was
corrected to satisfy SessionId's existing eight-character minimum. The local
fixture requires Unix-socket access and passed with native test permissions.
Formatting and whitespace checks pass. These are fixtures, not authenticated
live Codex/Claude/OpenCode busy-context acceptance; the earlier live Grok probe is
still the independently established live acceptance.

Installation is still pending review and final rollout. Remaining rollout must
refresh the release binary/managed integrations and monitor, address legacy panes
without launch provenance through managed launches, and validate held-hook/busy
contexts for the remaining supported provider paths. No live agent was interrupted
or installed binary replaced during this milestone.

## Milestone: capability upgrade, installation and installed live Grok acceptance

Rollout exposed a missing capability distinction: old capability-9 monitors can
host the same event inbox but do not promise eligible-event interruption. Merely
replacing the binary can leave such a host serving indefinitely. Bumped PROTO to
10 using the existing cooperative-retirement protocol, with no new transport or
restart loop. The admission, obsolete-holder response and replacement-ownership
fixtures pass. A current observer asks an older host to retire, but observation
itself does not start a service; the replacement must be started through an agent
attachment or explicit monitor start. The canonical lease still refuses duplicate
owners. This is a lifecycle capability change, not a cosmetic wire-version bump.

Built the release offline and atomically installed it at ~/.cargo/bin/clank. The
installed and target/release/clank SHA-256 both equal:
0b52dd6ada7170cb67a7399eda5db46534dbf3f29be25658e04dddcce124b2c4
Ran setup --grok-agent grok --repo for both ~/src/clank and ~/src/clankstack.
Setup merged managed Claude/Codex hooks and confirmed Grok adapters, all four
providers' embedded skills and OpenCode's plugin. No forced overwrite of edited
assets was needed. Doctor reported zero FAILs: clank 36 OK/4 WARN and clankstack
36 OK/3 WARN. Existing warnings describe orphan local agent state and the limits
of generic Grok runtime evidence; installation is not proof of live model reading.
Both repos now publish capability-10 monitors (observed PIDs 80341 and 14641).
Their existing read-only TUI panes were replaced in place through Zellij with the
updated binary, without moving focus, so they can share the new host. No agent's
session identity was changed to upgrade a monitor or panel.

The current live panes initially lacked launch stamps. The existing Grok reviewer
explicitly confirmed readiness while idle, including its exact bound session
01a0dad3-149a-7980-ba25-cc835ff8f3de. Its pane was replaced in place using the SAME
clank agent start grok --repo command. This resumed the existing session, not a
new agent or conversation. The launcher published verified provenance for
clank-clank terminal_20, actual Grok process PID 47690. No stamp was synthesized
for an old process. The current Codex pane was left running rather than being
terminated during this master turn; it still requires a managed restart for
terminal control. The native candidate socket is absent for this session.

The installed path then passed a controlled busy-Grok acceptance test. A finite
controller listened on one Unix datagram socket. The reviewer ran a disposable
FOREGROUND Python tool which checked its exact session, canonical repo cwd and
Zellij session, sent a ready cue with PID 74169, then blocked on a socket with a
60-second deadline. Only after that cue did the controller invoke installed
clank send --to grok --interrupt with nonce rollout-5702. No direct terminal writes
were used by the controller: this exercised persistence, sender/monitor contention,
durable claims, launch selection and the shared provider handoff in the release.
The command exited zero and reported Submitted for exactly one event. The logical
message row (sequence 5) retained interrupt:true, recorded submitted and was later
acknowledged by Grok after it handled the message.

Native Grok history contains a NEW interrupted-user envelope with the fixed inbox
hint. That JSONL record's SHA-256 is:
51b1bac8c570258eacd21f65efdac08d9e3951a311e8cc51c7e9cef39f2525e9
The authenticated model read the durable nonce message, wrote handled=true and its
same GROK_SESSION_ID to /private/tmp/clank-grok-rollout-survived.json, then acked it.
Grok independently sent a cancellation report (master messages@7), which was read
and acknowledged. Its generic native tool result said user_cancel and claimed
the Python command was not executed. The authenticated ready cue proves the tool
DID execute; that generic claim is inaccurate here. The evidence proves busy-turn
cancellation followed by retained inbox-hint processing and actual event handling.
It does not prove termination of arbitrary subprocess descendants. The test PID
was subsequently absent, the controller had exited, and its stopped probe's stale
socket was removed. No permanent controller, watcher or retry job remains. Probe
artifacts are under /private/tmp/clank-grok-rollout-* and no repo code was changed
by the reviewer to handle the probe.

Codex CLI here is 0.160.0, Claude is 2.1.280. Current primary Codex daemon/transport
source still selects app-server-control/app-server-control.sock; the absence in
this session is NOT evidence that the documented path changed. Only the updater
socket is present. Starting a second app-server or resuming the current thread in
it would not establish ownership, so neither was attempted. Native Codex acceptance
remains the isolated real-socket and protocol fixtures from the previous milestone,
not authenticated live acceptance. Claude/OpenCode are installed but are not agents
on this repo's active roster; no unrelated repo's working agent was interrupted to
manufacture coverage. Their busy contextual queue/cancel behavior remains documented
and fixture-tested, with no new claim of authenticated live acceptance. The live
Grok result is kept separate from these limitations. Future managed launches get
the provenance automatically; legacy live panes must be restarted deliberately.

Validation passed: three targeted monitor compatibility/retirement/replacement
checks and all six actual CLI hook/receipt/monitor integration fixtures under
capability 10, including interruptible local fallback, required replies, recurring
deadlines and no idle pane query after a due-event claim. Formatting and whitespace
checks pass. Release setup, doctor, monitor publication, managed Grok restart and
installed live event handling were verified separately from fixture results.
The plan is not self-finalized; live coverage limitations above remain explicit
for the review gate to assess.
