# any-replyable-event-takes-many-replies
# Any replyable event takes many replies

## Why

`clank events reply` only accepts agent messages. A command event with a
`reply_command` gets exactly one reply: its stored intent is immutable, it is
delivered once, and its acceptance is the same record as its
acknowledgement. A second, different answer is refused.

This is a carve-out from `replyable-messages-and-delivery-state-in-the-timeline`.
That plan added deliberate, repeatable replies for messages and kept the
command backend's one-reply contract unchanged.

The user wants no carve-out. Any replyable event can be replied to many
times, and what further replies mean is up to the backend.

## What to build

- **One reply operation for every replyable backend.** `clank events reply
  <selector> -m …` works for any event whose backend accepts replies: a
  message, or a command event with a saved `reply_command`. A backend that
  takes no replies still refuses, naming why. GitHub and timer events are
  examples.
- **Acknowledgement stays distinct.**
  - `events ack --reply` still gives the one acknowledging reply that a
    required-reply event needs.
  - A deliberate `events reply` never completes or reopens the event, and
    works before or after acknowledgement, as messages already do.
- **Each reply is its own operation.**
  - Each deliberate reply gets an operation ID, as message replies do.
  - Retrying with `--operation <id>` and the same text re-delivers that same
    reply, never a new one. The same text sent deliberately twice is two
    replies.
  - For a command backend, the operation ID is the `idempotency_key` in the
    stdin JSON. A backend can then tell a retry from a further reply. What a
    further reply means is the backend's business.
- **History for commands too.** A command event records its replies as a
  history (intent, acceptance, operation ID) as a message does. The event
  timeline and `events read` show them.
- **Durability is unchanged.**
  - Intent is stored before delivery.
  - The backend's lease serialises dispatch.
  - A failed or interrupted delivery leaves the operation pending, to retry
    with its ID.
  - The five-second callback budget still applies.
- **Compatibility.**
  - Existing logs read as before. An existing acknowledging reply counts as
    that event's acknowledgement reply.
  - Today's stdin shape keeps its fields. The key for the acknowledging reply
    is unchanged, so a handler that saw one reply per event keeps working.
  - A handler that needs to tell further replies apart reads the new key.
- **Documentation.** The README's reply section describes one model for all
  replyable events, and drops the messages-only wording.

## Acceptance

- `events reply` on a command event with a `reply_command` invokes it once
  per operation:
  - each invocation has its own `idempotency_key`;
  - a retry with `--operation` re-sends the same key and text;
  - a deliberate repeat sends a new key.
- A further reply on an acknowledged command event is delivered, and the
  event stays acknowledged.
- A required-reply command event still cannot be acknowledged without an
  accepted acknowledging reply. A deliberate `events reply` neither
  satisfies that nor reopens the event.
- A backend with no reply support refuses `events reply` with a reason.
- Message replies behave exactly as before.
- An old log with a one-reply command transaction reads correctly, and
  retrying its acknowledging reply is still idempotent.
- `events read --json` lists a command event's replies with their
  operation IDs and their pending or accepted state.

## As built

- **One operation model.** The message reply operation became
  `ReplyOperation`. Its `route` is a `ReplyRoute`: an agent's inbox, or the
  saved `reply_command` of a command event. Its acceptance is an
  `Acceptance`: a message's receipt, or a backend's successful exit.
  - Both enums are untagged, and the durable record keeps its
    `message_operation` tag, so old message operations read and write back
    byte-for-byte (fixture).
  - A command operation lives in its source's own WAL. Compaction, restart
    and pending-parent retention are the existing generic ones.
  - A command operation must name the parent's saved route, and is never
    acknowledging.
- **One submission flow** (`events/operation.rs`) for both backends. The
  steps are the same: intent first, then the accepted-retry suppression,
  then the immutable-input check on retry, then refusal of an unknown ID.
  Only the transport differs:
  - a message goes to the destination inbox under the author's lease;
  - a command goes to the existing callback under the per-backend lease
    shared with `events ack --reply`.
- **The answer is unchanged.** A command event's acknowledging reply keeps
  its legacy intent/ack records, its key and its copy fanout. A deliberate
  reply belongs to the copy it was sent from and acknowledges nothing.
- **Callback stdin** gains `acknowledge`: true for the answer, false for a
  further reply, whose `idempotency_key` is its operation ID.
- **Rows** carry `replies` and `reply_history_pruned`, renamed from
  `message_*` because a command event's replies are there too. No page or
  doc read the old names.
- **Timeline.** An accepted command reply is a sent entry under its
  original's source, with `destination: None` (no agent recipient) and no
  receipt. The TUI says "to: the <source> backend".
  - The operation records its `source_name`, so it is shown from its own
    record once the handled original ages out.
  - A record without the name falls back to the original's name, then to
    the log's name. It is never dropped.
- **Discovery.** `events list/show --json` project a reply's route through
  an allowlist:
  - a message reply gives its agent and repo;
  - a command reply gives only `{"backend":"reply_command"}`.
  The handler's argv, cwd, state and every reply body stay behind
  `events read`.
