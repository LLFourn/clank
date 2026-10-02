# an-event-that-needs-a-reply-cannot-be-acked-without-one
# Require a backend reply before acknowledging an event

## Why

Some events ask a question or need an answer. Delivering one must leave it owed,
and an agent must not be able to retire it with a plain acknowledgement. The
agent's reply must reach the producing backend. Initially the command backend
is the applicable transport. This is separate from notifying a source that its
event woke someone: a delivery receipt supplies neither an answer nor completion.

## Contract

- A command producer may emit `requires_reply: true` on an event. Omission means
  false. Persist the requirement with the event, preserve it across restart,
  replay and compaction, and expose it in wait items, event list/show JSON and
  human presentation. Replaying an existing ID must not silently remove its
  requirement. Existing events and source commands keep their current behavior.
- Add an optional `reply_command: [program, args...]` to command-source config,
  also supported by `--event`. The existing subscription argv is unchanged.
  Invoke the reply command directly, without a shell or appended subcommands.
  Its stdin is one versioned JSON object containing the original producer ID,
  reply text, author and an idempotency key. Supply the same resolved
  `CLANK_SOURCE_STATE_DIR` as the producer. Preserve opaque IDs, including `/`,
  Unicode and newlines, through JSON rather than shell interpolation.
- Support `clank events ack <selector> --reply <text>`. A required-reply event
  rejects an omitted, empty or whitespace-only reply. Preserve supplied text
  verbatim after validating it. A non-required command event may receive an
  explicit reply when its backend supports replies. Supplying a reply to an
  unsupported event is an error, not silent disposal of the text.
- A required-reply event without a usable reply handler stays visible and owed;
  report the missing capability at ingestion and acknowledgement. Never erase
  the event or weaken its requirement to make acknowledgement succeed.
- `--reply` addresses one logical event. Reject selectors that resolve to
  several unrelated events; do not send one answer to an arbitrary batch.
  Resolve and validate every target before changing any acknowledgement state.
  Existing bulk acknowledgement without a reply rejects the whole operation
  if any selected event requires one.

## Reply and acknowledgement transaction

1. Resolve the event by full source ownership (author, source key, config/CLI
   origin and invocation scope) and producer ID, not its display name alone.
   Persist reply-routing metadata when needed so invocation-only sources can
   still be answered after their producer exits. Config edits must not redirect
   an outstanding reply to an unrelated source or state directory.
2. Durably record a reply intent and stable idempotency key before invoking the
   backend. Repeating that intent after a crash uses the same key and text.
   Reject conflicting reply text while an earlier outcome is uncertain.
3. Run the backend on demand. Bound the complete invocation, including stdin
   writing, exit and cleanup, to five seconds. Reuse the existing owned-process
   facilities; drain or cap diagnostics and reap the process group on timeout.
   The monitor and event producers must remain responsive throughout.
4. Exit zero means the backend accepted this reply; nonzero exit, missing
   executable, timeout or interrupted execution leaves the event unacknowledged
   and produces an actionable error. A retry is explicit through the ack
   command, with no background retry loop or polling.
5. Persist successful reply acceptance and event retirement together under the
   event log's append lock, retaining the reply for inspection. Do not hold that
   lock while executing the backend. A crash between backend acceptance and the
   local commit may replay the callback: document at-least-once attempts and
   require backend idempotence. Never claim exactly-once external effects.

## All acknowledgement paths

Put enforcement in the shared event-acknowledgement policy used by both the CLI
and the TUI, including grouped/cross-agent acknowledgement. No existing bulk
path may bypass it with a direct WAL ack. Plain TUI acknowledgement of a
required-reply item must explain that an answer is needed and show the CLI
recipe; a TUI reply editor is outside this initial plan. If several stored
copies represent one logical event, validate all members and define one reply
transaction before retiring those members; do not issue a callback per copy.
Keep low-level replay/ingest bookkeeping separate from user acknowledgement.

## Acceptance

- Drive a real command-source fixture through ingestion, wake, failed plain
  ack, successful reply delivery, and retirement. Verify original ID, exact
  reply text, idempotency key and producer state directory at the backend.
- Cover persisted/replayed requirements, invocation-scoped routing after the
  producer exits, unsupported handlers and ambiguous or mixed bulk selectors.
- Drive nonzero, missing and hanging handlers: none retires the event or blocks
  unrelated delivery. Exercise retry and the crash window after backend
  acceptance, demonstrating stable keys and no duplicate logical answer with
  an idempotent fixture. Verify concurrent acknowledgements cannot substitute
  a different reply or retire a different row with a reused sequence number.
- Cover CLI and TUI/shared grouped-ack enforcement and unchanged plain ack of
  ordinary events. Update producer documentation and wake instructions.
- Run relevant event, source, WAL and TUI tests plus formatting. This plan queues
  implementation; it does not authorize installing a binary or editing other
  repositories' source configurations.

## Cost and scope

No idle worker, watcher, timer, process scan or periodic retry is added. Backend
work happens only when someone explicitly submits a reply. Delivery receipts,
event ownership transfer, GitHub replies and ordinary inter-agent message
reply transport are separate features. The requirement and its enforcement
must be extensible to another reply-capable backend without assuming that all
events can be answered by a command.
