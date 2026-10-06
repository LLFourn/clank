# a-backend-reply-belongs-to-its-event
# A reply to a command backend belongs to its event, not a row of its own

## Why

Since `any-replyable-event-takes-many-replies`, the timeline shows each
accepted reply to a command backend as its own "sent" entry. The user's `life`
log (newest first):

```
 1m →  ⚡ @?  voice/reply  "I will make that four tomorr…
 1m ~ 0d2b866 Schedule tomorrow afternoon tax and BAS re…
 2m ✓✓ 🎙 @assistent  "Um, actually, tomorrow it's going …
 2m ✓✓ 🎙 @assistent  "All right, let's let's deal with t…
 4m →  ⚡ @?  voice/reply  "Understood, that was the wro
```

Each agent reply to a voice message is a separate row:
- with a lightning bolt, the command default badge, not the voice source's 🎙;
- with `@?` for a destination, because no agent receives it;
- detached from the voice message it answers.

The outgoing reply entry was designed for one case: a message reply to an
agent in ANOTHER repo, which has no inbox copy here to show it. A reply to a
command backend has an event right here, the one it answers, so it belongs
there.

## What to build

- **No standalone entry for a backend reply.** A reply whose route is a
  command backend is not its own timeline entry, in the TUI, `clank log` or
  the remote timeline.
- **It shows with the event it answers.** The original event's row says it
  has replies, for example with a reply count or a `↩` mark beside its
  icon. Its event page lists the replies (text, when, operation, pending or
  accepted), as `events read` does.
- **Message replies are unchanged.** A reply to an agent in another repo
  keeps its `→ @agent (repo)` outgoing entry. A same-repo message reply
  keeps merging with the recipient's inbox copy.
- **Retention.** When the original has aged out of the log, its retained
  accepted replies are still reachable. Say how, for example from
  `events read`, or one compact line, and make it never a `@?` row.

## Acceptance

- In a repo with a voice source, an agent's reply to a voice event produces
  no new timeline row. The voice event's row shows it has a reply, and its
  page shows the reply text.
- No timeline row anywhere shows `@?` or a default command badge for a
  backend reply.
- A message reply to an agent in another repo still shows as `→ @agent
  (repo)`.

## As built

- **One shared model.** `MergedEvent.replies` holds the `BackendReply`s
  for the event, oldest first, pending and accepted alike. Each carries:
  - the original member key it answered (agent, log, seq, ident);
  - its operation id, text and time;
  - whether it was accepted.

  `reply_history_pruned` says older accepted ones were pruned.
  `backend_replies` takes each log's command-route replies by the row they
  answer. `merge` unions them across copies, deduplicated by owner, log
  and operation id, so identical-text replies stay distinct. Attaching
  one changes nothing else about the event.
- **One provenance check.** `reply_provenance` is this repo, this author,
  and a route the log can hold. It runs before a reply is attached to its
  event and before one is shown alone. A reply that fails it is shown
  nowhere and is named in a notice; its record stays where it is.
- **No standalone entry while its event exists.** The timeline skips a
  backend reply whose original row is in its log.
- **Retention.** Once its event has aged out, an accepted reply keeps a
  compact entry from its own record:
  - its icon is the source's configured one, else `REPLY_GLYPH` (`↩`),
    never the command default;
  - it reads "reply to an earlier `<source>` event";
  - it has no synthetic kind label;
  - its page works as before.

  Pending replies keep their original retained.
- **Shown everywhere.**
  - `event_row_text`, shared by the TUI, `clank log` and the remote rows,
    ends with `reply_mark`: `↩N`, where N counts the retained replies,
    then `+` when older ones were pruned, then `(n pending)`.
  - The TUI event page lists each reply. Its owner, time, state and
    operation are wrapped whole. Its text goes through the same safe,
    layout-keeping body renderer as event content.
  - `clank log --json`'s `command_event` carries `replies`,
    `reply_history_pruned` and, for a reply standing alone, `outgoing`
    with its operation and the copy it answered.
- **Unchanged.** Message replies to another repo keep their
  `→ @agent (repo)` entry. Same-repo message replies merge with the
  recipient's copy.
