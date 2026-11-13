# Follow-up drafts from the Codex instrumentation investigation

These are proposed plans, not approved production changes. Promote each only
after the research review, against the then-current code. The completion plan
can proceed independently of the larger native-connection plan. Their evidence
is in [codex-integration.md](codex-integration.md).

## a-completed-run-cannot-strand-its-agent

### Why

Clank removed its own completion delivery on the assumption that native
background completion always continues the agent. A registered run ending
must not silently leave its owner parked if that handoff does not happen.
Restoring one durable message and timeline row per run would recreate the
noise the earlier removal was intended to solve.

### Build

1. Define a completion obligation for an attended run, owned by the monitor
   and tied to the registering agent's concrete session plus run identity
   (including PID/birth). Keep it separate from messages and the TUI timeline.
   Completion means “collect the result”, never “the command succeeded”.
2. Use existing lifecycle evidence to trigger delivery, with no process
   polling. When the matching session is parked, return the obligation through
   the existing monitor-to-hook decision. While genuinely working, retain it
   for an appropriate boundary. A session that has ended or rebound must not
   receive work belonging to an unrelated predecessor. Specify explicitly
   what the operator sees if the original session cannot resume.
3. Define receipt, deduplication and retirement before implementing storage.
   Removing attendance, writing a response to a socket, and the model actually
   receiving work are different events. Preserve an undelivered obligation
   through monitor restart and a dropped hook reply. Coalesce a batch of
   completions into a succinct wake. Use existing delivery custody where
   possible instead of creating a parallel inbox protocol.
4. Retain native output/exit-status ownership. Do not invent exit status from
   a dead PID, recreate tool results, or treat an ambiguous native update as
   proof that a completion was consumed. If a reliable native receipt can
   discharge the obligation, specify the exact evidence; otherwise prefer
   one explicit result-collection task over silently losing it.
5. Update shared skills, help and book text to state the tested contract and
   fallback. Remove unconditional promises that the harness will always wake
   a parked agent. Preserve legacy retired `run_ended` log compatibility.

### Acceptance

Exercise the state machine in Rust: exit before park, during park, while
working, and after a failed reply; multiple runs; restart; PID reuse; agent
rebind; already-consumed native completion. The positive path causes one
logical result-collection task with a receipt and no message/timeline row.
Unknown liveness never implies completion. Run the opt-in installed-Codex
probe with an adapter integration case to prove that a real parked Stop hook
continues on both success and failure. Do not use a timer or a user prompt as
the successful handoff in that acceptance case. One complete implementation
commit, followed by normal review corrections if needed.

## codex-native-evidence-has-an-owner

### Why

Clank has several native Codex RPC paths, but a guessed shared socket need
not belong to the TUI holding the target session. Both observed team sessions
used `--no-daemon`; the default control endpoint was absent. Native events
cannot replace transcript inference until the connection is real and owned.

### Build

1. Establish and document a supported launch/attach arrangement with a
   private, monitor-discoverable endpoint and an explicit owning server
   generation. Evaluate an explicit app-server with a connected terminal
   against any supported per-TUI control endpoint. Use the smallest verified
   arrangement; do not silently change every existing session or restart a
   shared daemon. If neither meets the requirement, stop at evidence and
   rescope rather than shipping a guessed endpoint.
2. Share the existing Codex protocol client machinery. Validate server,
   concrete thread ID, repo cwd and current Clank binding before subscribing.
   Never use a fork's shared `sessionId` as the individual agent identity and
   never load a stored thread into another server to make a lookup succeed.
3. Project turn/item/hook facts into the existing monitor state machine.
   Implement one connection reader that demultiplexes replies, notifications
   and native requests without losing notifications during RPC calls.
   Prefer explicit synchronous Stop-hook lifecycle over final-answer inference.
4. Reconnect by subscribing and reconciling snapshots with overlap
   deduplication. Distinguish not observed, not loaded and idle. Handle missing
   methods and unknown variants as compatibility limits. No recurring snapshot
   polling and no assumption that every missed notification is replayed.
5. Route input by verified state. Steer working turns with an expected turn
   ID; release only the intended parked turn; await its terminal event before
   starting replacement input. Preserve existing admission and approval
   boundaries. Ambiguous mutation failures must not trigger duplicate sends.
6. Keep hooks minimal and policy-free. Move only the transcript dependencies
   for which native evidence is demonstrated; retain historical rendering and
   clearly identified compatibility fallback rather than a wholesale rewrite.

### Acceptance

An isolated installed-Codex fixture proves connection ownership and captures
turn, command and hook events for two independent agents. Wrong endpoint,
wrong cwd, stale binding and shared-root fork identities cannot authorize a
send. Reconnect across completion, duplicate events, mixed RPC replies/native
requests, absent experimental APIs and interrupted turn release are covered.
No unrelated session is loaded or cancelled. Record tested version ranges and
capabilities. Deliver the complete adapter in one implementation commit; split
only by revising the plan before implementation if evidence requires it.

## codex-hook-readiness-is-visible

### Why

A discovered/enabled hook can still be skipped because its exact definition
is untrusted or changed. An installed executable can also differ from the
version serving a live session. These are distinct from a hung monitor or a
missing completion notification.

### Build and acceptance

Extend doctor/setup diagnostics to report installed versus verified serving
version, endpoint availability, and whether Clank's exact expected hooks are
present, enabled, trusted and synchronous. Use supported discovery data where
available, and report unknown when it is unavailable. Guide the user to review
the exact hook through Codex's trust UI; do not bypass trust globally or approve
unrelated hooks. A changed definition must require the native review flow.

Fixture current, untrusted, modified, disabled and absent hooks; unavailable
server, stale binary and unknown API; multiple hook sources without duplication.
The user-facing remedy must distinguish restart, hook review and monitor
availability. Validate the trust transition in an isolated profile with only
a generated test hook. Document the supported range and never imply that
healthy hook discovery alone proves background completion delivery. One
complete implementation commit.
