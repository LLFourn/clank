# a-waiting-message-is-a-line-under-the-name

An agent with something in its inbox says so in the panel, and Enter
opens it.

## Why

A message sent to an agent is invisible until that agent next wakes.
The sender has no way to see it is still waiting, and nobody watching
the TUI can tell an agent that is busy from one that has been asked
something and has not got to it. The inbox is a real queue on disk —
`.clank/agents/<label>/inbox/`, one file per message — and the panel
is where every other pending thing about an agent is already shown.

The panel already has the shape this needs: `PanelRow::Wait(i)` is
"the wait attended by that agent, drawn beneath its row"
(`status_tui/input.rs`), a cursor position of its own, drawn only when
there is something to draw. A waiting message is the same kind of
thing and should look like one.

## The build

**A row under the agent, when the inbox is not empty.** A
`PanelRow::Inbox(i)` beside the existing `Wait(i)`, built in
`panel_rows` from the agents whose inbox has messages, so a row the
cursor can reach is a row the user can see — the rule that function
already keeps.

The line shows the message's TITLE and how long ago it was sent. Both
already exist and neither needs deriving again:

- `inbox::Message::subject()` is "the body's first non-blank line,
  without a heading's `#`s" — the title the user asked for, already
  stripped.
- `Message.sent` carries the timestamp, and `age::coarse(secs)` is the
  ladder the rest of the TUI phrases ages with.

More than one message waiting is one row saying so, not one row each:
the panel is a summary, and an agent with eleven messages should not
push the rest of the list off the screen.

**Enter opens it.** `agent_panel_action` maps the key to an action for
the selected row; an inbox row's Enter opens the message to read — the
whole body, not the title. Reuse whatever the commit and feedback
pages already do for showing a body rather than inventing a third
thing.

**Nothing is consumed by looking.** Reading in the TUI must not take
the message out of the inbox or stop the agent waking for it: the
recipient is the agent, and the human is looking over its shoulder.
`clank inbox` is what handles a message.

## Tests

- An agent with an empty inbox has no inbox row; one with messages
  has exactly one, whatever the count.
- The row's title is the first non-blank line with `#`s stripped — a
  body that opens with `## Something` shows `Something`.
- The row is a cursor position: it appears in `panel_rows` in screen
  order under its agent, and the cursor can land on it.
- Enter on it opens the body, and the message is still in the inbox
  afterwards.
- An agent whose inbox empties loses the row, and the cursor does not
  end up somewhere it cannot be.

## Out of scope

- Sending, replying or handling from the TUI. `clank send` and
  `clank inbox` own those; this plan is a window, not a desk.
- Any change to how messages are stored, named or ordered.
