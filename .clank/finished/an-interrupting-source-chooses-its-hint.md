# an-interrupting-source-chooses-its-hint
# An interrupting source chooses its hint

## Why

When an event interrupts an agent, Clank submits one fixed hint afterward,
`pane_input::EVENT_WAKE`, through either the terminal or Codex's native
`turn/start`. That is a paragraph about `clank events list` and required-reply
contracts. For a voice source, Lloyd wants the agent to see
`Interrupted for new voice message`, followed by the event's qualified
selector so the agent can read that event at once, for example
`Interrupted for new voice message: command-voice-7f9bc9f9@15`. Request from
`assistent` in `/Users/llfourn/src/life` (utterance
`utt/6ac42117-f051a4a07db72b1a`, clarified by life voice event 15).

## What to build

- **Schema.** Add an optional `interrupt_message: Option<String>` beside
  `interrupt: bool` on `CommandSource`, `GithubSource` and `TimerSource`.
  - A command producer line may override it, as it already can override
    `interrupt`.
  - **Expected, not yet required.** An interrupting source should describe
    its events, but a config file cannot say whether a source is new.
    Rejecting a missing message would break every existing
    `interrupt: true` config at load, and silently stop its delivery.
    - A missing message still loads and still interrupts.
    - It is reported by config validation, by a warning at load, and by
      `clank doctor`, naming the source and the field to add.
  - `interrupt` stays a bool. A message without `interrupt: true` has no
    effect, and config validation warns about it.
  - No template. Clank always appends the selectors in the same place, and
    a template can be added later if a real need for another position
    appears.
- **Persisted with the event.** Store the chosen message on the event, as
  `interrupt` is stored now. Changing config, or replaying an ID, does not
  change old events.
- **The selector is Clank's, not the config's.** The submitted hint is the
  custom text, then `: `, then the qualified selector of each event it
  covers, the same selector `clank events read` and `clank events ack`
  take (e.g. `command-voice-7f9bc9f9@15`).
  - Nobody hard-codes an occurrence id.
  - Several selectors are joined with `, `, and capped (for example three,
    then `+N more`), so the hint stays one short line.
- **Coalescing keeps every description.** The monitor coalesces one
  recipient's pending interrupting events into one request.
  - Events are grouped by their message, each group rendered as
    `<message>: <selectors>`, and groups are joined with `; `, e.g.
    `Interrupted for new voice message: command-voice-7f9bc9f9@15; New PR review: github-x@3`.
  - Events with no message form one group titled `New events`.
  - Interleaved sources never get one source's words, and no group loses its
    selectors.
  - The whole hint is capped, for example at three groups and three
    selectors per group, with `+N more` for the rest, so it stays one short
    line.
- **Validation guards the pane, never the event.** The text is typed into a
  terminal pane, so a usable description is:
  - one line;
  - free of control characters;
  - non-empty after trimming;
  - within a short cap (for example 200 characters).

  The one rule, `hint_text`, trims and checks it the same way for command,
  GitHub and timer events, on both the terminal and the native Codex path.
  It bounds only the submitted presentation: event content has no new
  limit, and source identity does not change with the hint.
  - **Where config is written,** config validation may refuse an unusable
    value before writing it, naming the source.
  - **Where config is loaded, a producer line is read, or a stored event is
    rendered,** only the description is refused. The event is shown under
    `New events` with its selector, and everything else is untouched:
    - the source still starts;
    - the event is still accepted, and keeps its payload and its
      interruption eligibility;
    - its required-reply obligation still holds;
    - the ordinary hook-owned wait still delivers it;
    - other sources are unaffected.

    An unusable description never reaches a pane. This is the lesson of
    `event-icon-validation-cannot-disable-an-agents-wait`: presentation
    must not decide custody.
  - **Diagnostics follow config changes.** A source's unusable description
    is reported once per configuration change, not on every monitor fold.
- **From the claimed batch.** The hint's descriptions and selectors come
  from the exact batch of events the interrupt claimed, not from an earlier
  inbox snapshot. Rendering the hint creates no wake receipt. The
  per-event interrupt ledger, and its failure and indeterminate behaviour,
  are unchanged.
- **Delivery unchanged.** The custom text replaces only the hint string.
  - The events stay pending in the durable inbox.
  - The Stop hook still delivers them in full, with their required-reply
    contracts.
  - Wake receipts, acknowledgement and the one-attempt-per-event rule are
    untouched.
  - A short hint is safe because it never carried the work.

## Acceptance

- A source with `interrupt: true` and
  `interrupt_message: "Interrupted for new voice message"` submits exactly
  `Interrupted for new voice message: <selector>` after the interrupt,
  through both the terminal path and Codex's native path. `<selector>` is
  that event's qualified selector, and `clank events read <selector>`
  resolves it.
- Two coalesced events with the same message list both selectors, and more
  than the cap end in `+N more`.
- Coalesced events with different messages keep each message with its own
  selectors, and undescribed ones appear under `New events`.
- An existing `interrupt: true` source without a message still loads and
  interrupts, and config validation, the load warning and `clank doctor`
  each report it with the field to add.
- A command line's `interrupt_message` overrides its source's.
- Config validation refuses unusable text (a newline, a control character,
  empty, or over the cap), naming the source.
- A hand-edited source with an unusable description still starts, and its
  valid events are delivered next to another source's:
  - the submitted hint shows those events under `New events` with their
    selectors;
  - a required-reply event stays pending and readable;
  - the hook-owned wait keeps working.
- A bad description on one command line, and unsafe text in a stored
  event, are each refused at the transport boundary, and the event is still
  delivered.
- The diagnostic for an unusable description appears once per config
  change, not on every fold.
- The interrupted event is still pending, still delivered by the ordinary
  wait, and still keeps its required-reply contract.
- `clank events show --json` shows the persisted message.
- The README's interruption section documents the field.

## Out of scope

- A custom hint for `clank send --interrupt`. Messages keep `EVENT_WAKE`.
- Any change to when or how an interrupt is sent.

## As built

- **Recorded per event.** `interrupt_message` is added to the command,
  GitHub and timer sources, and to command producer lines.
  - Each event records it in the WAL as `interrupt_hint`, only when the
    event interrupts.
  - Physical copies that disagree keep none.
  - `clank events show --json` shows it.
- **Rendered from the claimed batch.**
  - `event_interrupt::hint_text` is the single check on a description.
  - `event_interrupt::hint` renders the hint from exactly the events the
    attempt claimed.
  - Both the Codex native `turn/start` and the terminal pane receive that
    text.
  - A sent message (`clank send --interrupt`) keeps the old fixed wake,
    `MESSAGE_WAKE`. A batch of messages alone gets exactly that text. A
    batch mixing messages with source events gets the source groups, then
    that text, in one interruption.
- **Reported, never refused.** A missing or unusable description is
  reported, and nothing refuses it:
  - by `event_interrupt::hint_problem`;
  - by the monitor, when an author's sources change (not on every fold);
  - by `clank doctor`, as `agent: <label> interruptions`.

  Nothing in admission, ingestion or delivery checks it.
- **Untested wiring.** The terminal path passes the rendered text through
  `wake_for_events_async`, and its wiring has no unit test, since it needs
  a live pane. The Codex path's wiring is tested, and both receive the same
  text.
