# event-rows-show-an-icon-and-their-destination
# Event rows show an icon and their destination

## Problem and outcome

Timeline rows such as `signal signal`, `message codex`, and `voice voice` repeat
implementation labels instead of telling the user who handles the event. Use the
event icon to communicate its type and a compact destination label to communicate
its handlers: `@codex` for one agent or `@foo,bar` for several. Keep useful event
content beside that label without redundant type/source words.

## Contracts and implementation

- Use the existing configured icon and shared safe icon interpretation. Provide
  meaningful default icons for sources without overrides; icon presentation must
  never affect event admission, delivery or waits. Retain the mail icon for messages.
- Derive destinations from the authoritative event projection and its actual
  recipient copies, never the current viewer, an actor/sender field, environment
  label or incidental source name. An event held for several agents lists their
  labels once in deterministic order as `@foo,bar`.
- Preserve the existing distinction between outgoing reply custody and recipient
  delivery. For an outgoing message/reply to another team, show its actual target
  and provide enough repository context to distinguish identical agent labels.
  Do not claim a remote recipient woke or acknowledged from a local sent record.
- Apply the same concise description to the TUI, remote timeline and human log
  presentation. Keep the existing unwoken, woken and acknowledged state indicators
  alongside the type icon. Destination formatting must not alter these states.
- Remove redundant kind/source prefixes from ordinary event rows. Preserve useful
  content, recurrence information and actionable failure indicators. Keep source
  identity, sender, routing and original event metadata available in details and
  structured output rather than discarding them from storage.
- Keep genuinely useful GitHub context, such as repository, issue/PR number and
  action, while using the same destination convention. Unknown or historical
  destinations must be represented honestly without inventing a handler.
- Preserve complete multiline content in storage, agent delivery and event details.
  Derive compact previews only at the timeline row boundary; never pass a preview
  back into the authoritative event. Safely render terminal controls, escape browser
  text appropriately, and retain readable truncation at narrow widths. This is
  presentation work: no new polling, watchers, delivery mutations or receipt effects.

## Validation and delivery

Cover the reported Signal, voice and message examples, one and multiple handlers,
duplicate recipient copies, different delivery states, outgoing cross-repo replies,
missing historical routing, useful GitHub context and narrow terminal/mobile rows.
Verify matching descriptions across the TUI, remote projection and log, and ensure
event details and receipt/acknowledgement behavior remain intact. Include concrete
before/after visual examples. Implement the complete plan in one commit with no
internal named milestones.
