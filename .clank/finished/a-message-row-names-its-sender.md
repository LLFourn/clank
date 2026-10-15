# a-message-row-names-its-sender
# A message's timeline row reads sender → recipient, never "@assistent → @assistent"

## Why

In `~/src/fs-war-room` the TUI showed:

```text
✓✓ ✉ @assistent → @assistent  “Fixed: deviceRef() in index.html now returns 'device N' …”
```

That reads as assistent messaging itself. What actually happened:
- **claude replied** to a message from assistent. claude's `messages.jsonl` holds the
  reply operation, routed to assistent (`message_operation` seq 390).
- **assistent received it.** assistent's `messages.jsonl` holds it arriving, `from:
  claude` (inbox seq 128).

**The cause.** The timeline merges those two records into one `MergedEvent`. Its row
prefix is `MergedEvent::destination()` (`github/timeline.rs`), which is:
- `local`: who saw it in this repo (`seen_by`), here assistent;
- then `→` and `outgoing`: where its replies went, here also assistent.

For a message delivered to an agent of this repo, both halves name the recipient,
and the sender (claude) is in neither. The row text deliberately leaves "who sent
it" to the details, which is right for events from outside, such as GitHub or
Signal, but wrong for a message between agents. There, the sender is half of what
the row is.

## What to build

### 1. A message row says who sent it to whom

A `Backend::Message` row reads `@sender → @recipient(s)`. The sender is the message's
`from` (with its repo, when it is not this one), whichever record it came from:
- an inbox row: its `from`;
- an outgoing reply or send operation: its `author`.

A message merged from both is one row with one sender, never the recipient on both
sides. One sent out of this repo shows `@sender → @agent (repo)`. One received from
another repo shows `@sender (repo) → @recipient`.

### 2. Other backends are unchanged

GitHub, command and timer rows keep `destination()` as now: who saw it, and where
replies went.

### 3. One rendering

`event_row_text` is shared by `clank log`, the TUI and the remote page, so the change
reaches all three.

## Acceptance

- **The fs-war-room case.** claude's reply to assistent, merged from claude's
  outgoing operation and assistent's inbox row, reads `@claude → @assistent`.
- **Across repos.** A message sent from this repo to another reads
  `@sender → @agent (~/that/repo)`. One received from another repo reads
  `@sender (~/that/repo) → @recipient`.
- **A message delivered to several local agents** names each recipient once.
- **Unchanged:** GitHub, command and timer rows render as before. The existing row
  fixtures for them still pass.
- **Everywhere:** `clank log`, the TUI and the remote page show the same text.

## As built

- **`MergedEvent` gains `sender: Option<Sender>`** (`github/timeline.rs`), messages
  only. `merge` takes the local repo, and `sender_of` takes the first copy whose
  message item names a `from`. Inbox and outgoing copies both carry one; an outgoing
  copy's names its author. The sender's repo is kept only when it is not the local
  repo.
- **The row:** `MergedEvent::route` renders `@sender → @recipient, …`.
  - The recipients are the local holders and the outgoing routes, each once, with a
    repo when it is not this one.
  - `@?` stands in when no recipient is known.
  - `event_row_text` uses it for a `Backend::Message` row with a sender, so
    `clank log`, the TUI and the remote page all change together. Other backends
    keep `destination()`.
- **Tests:**
  - `a_message_row_reads_sender_to_recipient`, through real logs:
    - a same-repo reply reads `@worker → @coordinator` (the fs-war-room case), and
      its original `@coordinator → @worker`;
    - a reply sent to another repo reads `@worker → @coordinator (<repo>)`;
    - a message received from another repo reads `@coordinator (<repo>) → @worker`.
  - Four existing tests that asserted the old `→ @target` prefix now assert the
    sender comes first and the target after it.
- **Mutation checks:** "the row uses the holders again" and "the sender's repo is
  never shown" are both caught. A fallback to an outgoing copy's author survived,
  because the copy's message item already names it, so the fallback is removed.
