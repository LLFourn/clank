# a-source-is-told-when-its-event-woke-someone
# A source receives a receipt when its event is presented in a wake

## User decision — 2026-10-03

The user clarified: "read receipts are wake receipts. Let's try that first."
Use the hook's emitted wake as the observable boundary. The agent need not invoke
`clank events show`, and this feature does not observe arbitrary file/backend reads.
This replaces the earlier draft's unresolved alternatives, breaking subscribe/ack
conversion and contradictory migration requirements.

The first version deliberately chooses best-effort bounded callbacks owned by the
presenting hook. It does not need a durable receipt outbox, another daemon, a detached
child or a polling recovery loop. This plan queues behind the active timer plan;
the one-active-plan rule and review gates still apply.

## Keep the meanings separate

1. Ingesting a producer ID into the event WAL accepts work.
2. Emitting and flushing an authorized hook wake presents that work to the runner.
3. The optional receipt tells the backend which IDs that wake presented.
4. A later event ack, or accepted required reply, handles the Clank event.
5. The backend separately decides when the user's instruction is complete.

Only step 2 triggers receipts. Monitor queue insertion, a monitor socket write,
wait subprocess output, parsing, `events list/show/ack`, and a callback's success
are not additional receipt triggers. This is Clank's hook-output boundary, not proof
the provider accepted the continuation or that a model/human read or obeyed it.
Successful wake emission means the adapter's payload write and flush succeeded;
Claude's continuation uses stderr plus its continuation exit code, while Codex and
OpenCode use their established stdout formats. Preserve all exit/wire semantics.

Anx's receipt records delivered/read metadata by original utterance ID only. It must
not invoke `anx ack` or `anx say --done`, erase owed work, or prevent unfinished work
from transferring to a new master. Required replies retain their existing transaction
and cannot be fulfilled by a wake receipt.

## Additive backend interface

Add optional `receipt_command: Vec<String>` to a configured command source alongside
the unchanged `command`, `reply_command` and `glyph`. Omitted means no callback and
no receipt cost. Reject an empty argv/program at validation. The source name/key,
subscription argv and `CLANK_SOURCE_STATE_DIR` stay unchanged. Never append or insert
subscribe/ack subcommands. This preserves stateless clankstack sources and Anx's argv
with flags after its events subcommand.

Proposed handler protocol (becomes supported only after implementation/review):

```json
{"version":1,"kind":"wake","source":"voice","author":"codex","ids":["utt/6abf08c1-89fdad476ff26c8d"]}
```

Invoke the exact configured argv without a shell or added arguments. Stdin is one
JSON object followed by EOF. IDs are original producer IDs, not selectors or hashes;
slashes, Unicode, quotes and newlines survive. The handler receives the source's
resolved working directory and `CLANK_SOURCE_STATE_DIR`. No answer, completion flag
or event-ack permission is in the receipt envelope. Exit zero accepts the batch;
partial effects followed by failure remain possible, so handlers must be idempotent
per source/utterance ID. Receipts may repeat on later wakes and master handover.

Capture the route and IDs at the presentation decision from the author-scoped
configured source and authoritative WAL row, using the exact resolved source key
and selector. Receipt routing is presentation configuration, unlike an answer's
immutable saved reply route. An already-pending legacy command row becomes eligible
when its configured source gains a receipt handler; no event reingestion or new ID
is required. Removing a handler disables future receipts but does not retire work.
Editing a handler affects subsequent presentation decisions; an in-progress batch
keeps its captured argv/cwd/state identity. Do not resolve a label from a role claim,
loose name match, current unrelated source or default backend directory.

## Presentation scope and ownership

The first version covers configured command sources presented by the generic
Codex/Claude/OpenCode Stop-hook adapters, including Claude SessionStart's actual
additional-context emission. `--peek` itself stays side-effect-free; an adapter
that actually emits its peek result owns the receipt. Native Grok task handoff and
other direct backend/file reads are explicit follow-ups, not falsely treated as the
same emission path. Document these limits. Invocation-only `--event` receipt handlers
are outside v1 and must be rejected with a clear unsupported-scope diagnostic;
existing invocation sources without a handler remain unchanged.

Both monitor-shared and local-fallback waits converge on the same hook presenter,
so receipts are dispatched there rather than in either source host. Retain structured
selected event identities through outcome construction; do not scrape rendered hints.
Only items actually included in the final wake count. Unknown/noncommand items,
synthetic source failures, unpresented rows, silence, renew-wait control envelopes,
diagnostics, malformed wait output, failed emission and superseded bindings produce
no receipt. Recheck the bound session before output using the existing guard. A stale
hook cannot borrow the replacement agent's receipt identity.

The hook owns each callback child/group, waits for its bounded outcome and attempts
validated cleanup within the same deadline. An uncertain cleanup leaves an owned
process identity record that prevents another callback for that source until safe
recovery is proven. No callback is intentionally detached. The monitor need not
outlive a client just to send a receipt and owns no new dispatch queue or worker.
Child callback stdout/stderr must never inherit the hook payload streams.

## Cost, failure and latency

- Group IDs by captured source/backend route and deduplicate within a wake. Attempt
  one batch per route, sequentially; at most one receipt child owned by this hook at
  a time. Use a nonblocking per-source receipt lease across concurrent hooks so an
  occupied handler is skipped, not queued behind an unbounded callback backlog.
- Limit attempted IDs to 256 and encoded stdin to 64 KiB for the whole wake. Do not
  truncate an ID. Diagnose omitted receipt candidates; their work is still delivered
  and owed. Enforce these limits during collection/encoding, before cloning oversized
  IDs or building an unbounded grouping buffer. Spend no resources on a source without
  a configured handler.
- Give the entire receipt phase a one-second monotonic budget, including launch,
  stdin writing, handler exit and group cleanup. Reserve cleanup time inside that
  budget and existing hook-ceiling margin. Later routes are skipped when the budget
  is spent. This bounds software waiting, not OS syscall latency or uninterruptible kernel
  activity. Cleanup uncertainty must contain future dispatch rather than extend the
  deadline or assert that a process vanished. Do not repeat the old impossible
  promise that owned callbacks have zero latency or hard realtime cleanup.
- Emit and flush the wake before attempting callbacks. Receipt launch failures,
  busy leases, nonzero exits, blocked pipes or timeouts never replace its wire result,
  exit code, local ack state, reply intent or backend completion state. Use bounded
  stderr diagnostics on adapters where stderr is diagnostic-only; Claude's wake is
  on stderr, so do not append callback failures to that prompt. Store Claude's last
  callback failure in one replaceable, size-bounded diagnostic file per repo/author
  in the existing trusted runtime namespace. Declare and document its exact safe
  path; it is diagnostic-only, never receipt/retry/completion authority. No append-only
  error history or new worker is needed.
- This is best-effort: there may be no receipt after a crash between emission and
  callback or after overload/failure. Another presentation may retry naturally;
  there is no guarantee after the event is acknowledged. Do not label this durable
  at-least-once or exactly-once delivery. Callback effects can be duplicated, hence
  backend idempotence. Accepted work is never dropped to compensate for a receipt.
- No idle polling, periodic retries, process-table scan, child per event, permanent
  receipt service, completion ledger or durable receipt backlog. Reuse the existing
  process identity/signal/cleanup and owned-callback seams where their contracts fit;
  do not weaken them to meet the time budget. Receipt leases must use declared paths.

## Implementation milestones

1. Verify the current hook emit/exit boundary and session guards, typed wait JSON,
   command source keys/WAL resolution, callback ownership and runtime paths. Record
   the route snapshot, exact Claude diagnostic path and one-second cleanup proof.
   Add the optional config and pure batch/request projection with compatibility
   tests. Keep presentation metadata in the CLI boundary, not a new core completion
   model or a modification of immutable reply routes.
2. Carry selected identities through the generic presenters, make wake writing/
   flushing fallible and observable, and implement the bounded owned dispatcher.
   Dispatch only after successful authorized emission. Cover both shared-monitor
   and local-fallback presentation, and actual SessionStart additional context.
   Preserve existing hook protocol and deadline behavior.
3. Drive the complete configured-source-to-hook-to-backend flow, document the
   opt-in protocol, latency/best-effort/provider limits and installation status.
   Coordinate Anx adoption only once the handler contract is supported. Existing
   source configs need no migration and no receipt handler must be silently enabled.

## Required evidence and exit

- Real output writer tests: receipt observes the wake already emitted/flushed;
  broken/partial output, silence, diagnostic, supersession and unpresented events
  never notify. Cover each supported adapter's stream and exit contract.
- Shared and fallback driven wakes notify the same original IDs with the exact
  source cwd/state directory. Configured sources without a handler are byte/argv
  compatible. Adding a handler to an existing backlog produces a receipt. Scope
  collisions and a replaced session cannot notify the wrong backend or author.
- A burst is batched within explicit memory/process/time bounds. A busy lease,
  failing/hanging handler, blocked stdin and child descendants do not suppress the
  wake or escape cleanup. Concurrent hooks cannot grow per-source callback count.
- Repeated presentation is harmless for an idempotent fixture. Receipt success and
  failure both leave the WAL row unacked, required replies unanswered and Anx-like
  work owed. A new master's independent subscription can still present that work.
- Run affected hook/config/wait/command/reply tests and source/process/signal/identity
  boundary gates. Record any provider/real-clock/platform limits and installed binary
  status accurately. Do not test only request construction and call it a wake receipt.
- Finish after these guarantees. Backend direct-read tracking, native Grok delivery,
  invocation-only receipt routes, durable receipt delivery and completion policy
  remain outside this first version.

## Milestone 1 — configuration and projection

Implemented the optional `CommandSource.receipt_command`, omitted on serialization
when absent. Neither subscription argv, source key nor saved reply route changes.
Arm-time validation rejects empty argv/programs and invocation-only receipt handlers.
Configured legacy sources and invocation sources without handlers still resolve.
`cli::wake_receipt` provides pure route capture and bounded JSON projection; it
performs no filesystem mutation, process launch, event ack or hook emission.

Route capture accepts a configured source, its exact source key, resolved execution
cwd and author agent directory. It snapshots handler argv and cwd; backend state is
`agents_root(repo)/<author>/sources/<source-key>` via the existing helper. Producer
IDs are borrowed from selected authoritative WAL rows, deduplicated per captured
route, and encoded once. Across all routes, at most 256 IDs and 65,536 stdin bytes
are retained. Oversized candidates are omitted whole, and later fitting candidates
may still be included. These are best-effort receipt cost budgets, not restrictions
on the underlying event or wake. No receipt handler means route capture returns
immediately without copying argv or ID data.

Exact runtime paths, using the existing validated uid namespace and repo hash:

- Diagnostic: `/tmp/clank-run-<uid>/<repo-tag>.wake-receipt-<author>.diagnostic`.
  `repo-tag` is the first 32 hex characters of BLAKE3 over resolved repo path bytes.
  Maximum diagnostic content: 4,096 bytes; replace rather than append. Claude's wake
  stderr stays untouched. The file is diagnostic-only.
- Lease: same prefix plus `.wake-receipt-<author>-<source-key>.lease`.
- Child ownership: same prefix plus `.wake-receipt-<author>-<source-key>.owner.json`.
  This records only process identity for containment/recovery. It is neither a
  pending receipt nor callback acceptance, ack, reply or completion authority.
  Path derivation creates nothing; dispatch must vet/create the runtime namespace.

Emission audit: generic Stop currently ends in `emit_and_exit` using infallible-looking
print macros. Codex emits JSON on stdout with exit 0, Claude emits continuation text
on stderr with exit 2, and OpenCode emits its established stdout text with exit 0.
The binding guard precedes final outcome emission. Claude SessionStart has its own
binding guard and actual JSON additional-context emission. Milestone 2 must preserve
these contracts while exposing write-and-flush success. The wait already resolves selectors against the author WAL and its typed JSON
includes original producer IDs. The hook mirror must retain those IDs alongside
selectors; human hint parsing is not an acceptable substitute, and only final
selected items are eligible. No second WAL read is needed or desirable.

Execution-cwd audit: producers presently inherit the source host's process cwd;
`run_leased_command_source` captures that cwd for saved reply routes. A shared
monitor also inherits its starter's cwd, which can differ from the hook caller's.
Therefore milestone 2 must carry the resolved source-host cwd through the typed
presentation boundary; passing the hook's cwd or blindly taking the repo root is
not proof of the required route. Callback routing must not borrow a historical
reply route (which may be absent or intentionally immutable). Exact author, source
key, selector, active configuration and source-host execution context must agree.

Budget/cleanup audit: the existing reply callback owns a process group, captures a
kernel birth-identity token before stdin delivery, kills descendants even on parent
success, and retains its owner record if group cleanup is uncertain. Reuse this
ownership/signal seam without coupling receipts to reply acceptance. One receipt
phase has an absolute monotonic deadline of start + 1 second, with the last 200 ms
reserved for cleanup; do not restart that deadline per route. At/after the work
cutoff, launch no new route, cancel stdin/exit work, signal the owned group and reap
within the remaining deadline. If cleanup cannot be proven, retain containment
metadata and refuse another callback for that source until safe recovery is proven.
Lease acquisition is nonblocking and receipt recovery runs only on a later actual
wake, never an idle retry loop. No implementation can prove a hard wall-clock bound
for synchronous OS operations or guarantee immediate removal of an uninterruptible
process; the contract above explicitly distinguishes the software deadline from
that limitation. Failure never alters the already emitted wake or event handling.
Runtime cleanup/writer proof remains milestone 2/3 evidence, not claimed by pure
projection tests.

Installation status: callbacks are not yet connected or installed. Configuration is
preparatory on this reviewed branch. Do not enable Anx receipt routing until the
complete presenter/dispatcher tests pass and the reviewed CLI is installed.

Validation for this milestone: 17 core agent-configuration tests passed; seven CLI
receipt-filtered tests passed (five projection/route tests, configured-source
validation and one existing Grok diagnosis test); the existing arm-time source
validation test passed; all six config-write boundary tests passed. CLI lib and
integration harness compiled with the additive field. `cargo fmt --all` and
`git diff --check` passed. Existing unrelated test warnings remain unchanged.
These checks establish configuration/projection compatibility, not real delivery
or bounded child cleanup; those remain required before this plan can finish.

## Milestone 2 — successful output owns receipt dispatch

Connected the generic Stop presenter and Claude SessionStart. `emit_wire` performs
fallible write-all, newline and flush using the existing adapter stream/exit format.
Only successful continuation emission can dispatch. Silent, diagnostic, renew-wait,
unsupported native Grok output, malformed wait replies, unselected/failure items,
partial writes, failed flushes and replaced bindings cannot notify. The real emission
helper is shared with writer/callback tests, rather than reconstructed in tests.
SessionStart uses its actual additional-context writer and its own binding guard.
Callback stdout is null, stderr is drained into a bounded tail, and Claude callback
failures never enter the continuation prompt. The last diagnostic replaces a bounded
runtime file; a successful subsequent receipt phase clears old failure text.

The hook retains the original producer ID from typed `wait --json`, alongside the
exact source-key/sequence selector that the wait already resolved from this author's
WAL. Route selection verifies a configured handler, exact source name and source key,
and a valid positive sequence, then captures current presentation configuration.
This is the private wait-process/monitor protocol, not arbitrary backend JSON or a
rendered hint. A second WAL read was deliberately removed: it would add redundant
parsing and locks and could invalidate a real presented snapshot after a concurrent
ack or compaction. Event selectors and IDs are retained through final selection;
no callback reads or writes event WAL, ack, reply-intent or completion state.

Execution context is additive CLI metadata: monitor `Note::Items` carries its host
cwd, shared wait preserves that value, and the CLI wait envelope carries it to the
hook. Local fallback and read-only peek carry their own wait-host cwd. This is current
presentation-host execution context, not an immutable historical producer cwd. The
same field serves all command rows in that presentation; the hook shares it through
an Arc. Existing JSON without the field still parses. With an opted-in handler but
missing execution context, receipts are skipped with a bounded diagnostic requesting
that the monitor be restarted with the current CLI. No cwd is guessed or borrowed
from the saved reply route. `--peek` alone has no receipt effect; only a real adapter's
successful output can notify. Human wait output remains unchanged.

`owned_callback` extracts the existing reply process-ownership executor. Replies
retain their immutable route, original envelope, five-second budget, intent/acceptance
transaction and recovery rules. Receipts pass their own bounded stdin and two shared
absolute deadlines: work cutoff at phase start + 800 ms, cleanup cutoff at + 1 second.
No later route receives a fresh deadline. The hook takes a nonblocking per-source
runtime flock; busy means skip. It creates the resolved backend state directory
if necessary without inspecting its private contents. Kernel process birth identity is saved before any
request bytes are delivered. The executor joins stdin writing, bounded stderr draining
and child exit, then kills/reaps the owned group and its ordinary descendants even
when the parent reports success. Uncertain cleanup/ownership remains recorded and
blocks another callback for that source until safe recovery can be proven. Recovery
runs only on a later presentation, with no idle task or process-table scan. Handlers
must consume the stdin request before applying effects, remain within their owned
process group and avoid daemonizing; arbitrary process-group escape cannot be owned
by a process-group contract. Crashes can miss/duplicate receipts and retain recovery
metadata; no stronger delivery or realtime cleanup guarantee is claimed.

Validation so far: 17 receipt-filtered tests pass on real macOS callback children;
99 hook tests, 63 local/shared wait tests and nine required-reply tests pass. Two
signal boundary checks, one identity environment boundary, six config-write boundary
checks and two doc-comment boundary checks pass. The shared wait test retains a host
cwd different from the client's. Real writer tests cover all three Stop adapters,
SessionStart, partial writes, failed flushes, supersession and bounded isolated Claude
failure diagnostics. Tests verify unchanged unacked/reply-required WAL rows, repeat
receipts, exact cwd/state/producer IDs, handler removal/edit snapshots, busy leases,
unknown ownership and preservation of selected identity despite later WAL removal.
The blocked-stdin/descendant fixture runs approximately 0.8 seconds with a 1.5-second
assertion allowing OS scheduling noise, proves both processes gone, removes the owner
record and skips a later route. This proves the software deadline on this platform,
not a hard realtime promise. On platforms with larger pipe buffers, the same fixture
still verifies the blocked handler/exit deadline; the existing reply fixture also
uses stdin larger than its pipe capacity. Existing unrelated test warnings remain.

Remaining milestone 3: drive built-CLI configured producers through both
actual shared-monitor and local-fallback hook paths, including an actual SessionStart
catch-up; document the opt-in backend protocol and supported scope; run any remaining
boundary checks; install only after review and coordinate Anx adoption after the
supported CLI exists. No receipt handler was enabled and installed binaries remain
unchanged in this milestone.

## Milestone 3 — real CLI contracts and protocol documentation

Added four subprocess acceptance tests in the single integration harness, using
`CARGO_BIN_EXE_clank`, temporary repos/homes, configured Python command producers,
and local Unix readiness signals. No model, terminal, account or external backend
is needed. A held monitor lease forces genuine local fallback without starting a
second monitor. The shared test starts a real monitor in a directory different from
the hook, waits for its accepted-agent debug event, then releases the producer's
FIFO. It proves the producer belongs to the monitor, that admission itself sends
no receipt, and that the hook callback receives the monitor's exact cwd and private
state directory. Normal newer-protocol retirement ends the monitor's source group;
the test also checks the producer is gone. Fixtures retain kernel birth identity for
failure cleanup and remove only their own repo-hash runtime entries.

Both paths feed the same original ID with slashes, Unicode, a quote and newline.
Producer argv retains its `events --flag` arguments and callback argv retains
`receipt --flag`; neither receives invented subcommands/arguments. The callback
gets one JSON object and EOF. Its stdout/stderr never pollute the hook result.
Real `events list/show` and `wait --peek` produce no receipt, and inspections start
no additional producer. The event stays unacked and reply-required; a plain CLI ack
still refuses it. A second master's independently configured subscription receives
the original still-owed ID with its own author/state route while the old row also
remains unacked. Receipts do not grant completion or prevent handover.

The SessionStart test starts with a configured producer without a receipt handler,
ingests its event through a real local wait, then adds the handler. This exposed a
pre-existing catch-up gap: initial `--peek` considered workflow/messages/timers but
never loaded configured command/GitHub backlog. Moved the monitor's existing source
custody projection to `event_source::owed_from_sources` and reused it for explicit
peek. It reads retained rows, stamps exact selectors, preserves ack filtering and
shown-item deduplication, and starts no producer, transport, timer or receipt. It
skips missing directories/logs without creating an event directory. Configured
source scope is explicit; an invocation-only same-named source cannot borrow its
configured counterpart's WAL. Normal non-peek wait hosting remains unchanged.
This is on-demand inspection, not an added polling path or receipt WAL reread.
Two focused tests prove unchanged WAL bytes and isolation of missing/invocation
custody. The real SessionStart now emits the legacy backlog as additionalContext
before notifying, without reingesting the event.

That test also exposed SessionStart ignoring `StopHookArgs.repo`, unlike ordinary
Stop. It now honors the explicit override while keeping the session/subprocess cwd
for callback execution. The test deliberately runs the session from a repo subdir
and supplies the actual root. This prevents fail-open silence from the wrong roster
path and is exercised through the real hook command.

README now documents opt-in configuration, exact version-1 envelope, original IDs,
handler argv/stdin/EOF, cwd/private-state ownership, presentation versus ack/reply/
completion, backend idempotence, supported adapter/direct-read limits, overload and
one-second cleanup bounds, isolated diagnostics/runtime path, configuration edits
and installation/restart requirements. No source is opted in automatically.

Validation: four full CLI acceptance tests passed (about 12 seconds together),
63 wait tests, 99 hook tests and four source-custody/sink tests passed. The earlier
17 receipt tests, nine reply tests and configuration/projection evidence remain
applicable. Final affected README and git/signal/identity/config/doc boundary gates
are recorded below after completion. Formatting and diff checks are required before
commit. These are real Clank subprocess/output/child-group checks on macOS; no live
provider continuation, native Grok delivery or real Anx completion operation was
performed, and those must not be inferred from this evidence.

Installation/adoption handoff: all implementation milestones and backend protocol
are ready for final review. Installed binaries still predate receipts, and no Anx or
other live source configuration has been changed. On a FINISHED gate, install the
reviewed CLI, notify the Anx agent of the supported opt-in contract, then finalize.
It must implement an idempotent delivery-metadata handler before opting in and must
not use task completion as receipt handling. Restart old monitors when adopting the
new CLI metadata. This installation/coordination remains post-review work; no claim
of live adoption or installed feature support is made by this milestone.

Final milestone checks: all three affected README checks passed; git boundary 2/2,
signal boundary 2/2, identity environment boundary 1/1, config-write boundary 6/6,
and doc-comment boundary 2/2 passed. `cargo fmt --all -- --check` and
`git diff --check` passed. No additional provider or backend claims are made.
