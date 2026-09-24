# the-message-box-grows-down

What you're typing to an agent stays readable while you type it.

> "on the remote web the text entry box doesn't grow vertically when
> you use it -- it grows to the side :S" — lloyd

## Why

`page.html`'s say box is `<input id="text">` (line 297), one line
forever. A sentence longer than the box scrolls sideways under a caret
that sits at the right edge, and the beginning of what you wrote is
gone — on a phone, which is where this page is read, that is most
messages. You can't re-read what you're about to send, which is the one
thing you do before sending it.

A message to an agent is a paragraph, not a search term.

## The build

**The box is a textarea that grows downward.** One row at rest,
growing with its content to a cap of roughly a third of the viewport,
then scrolling inside itself. It grows on input, and shrinks back when
the text shrinks or after a send — a box that only ever gets taller is
the same bug pointing the other way.

**Enter still sends.** A textarea does not submit its form on Enter, so
the page binds it: Enter sends, Shift+Enter (and the phone keyboard's
own newline) inserts a line. Nothing else about sending changes.

**Everything that touches the box keeps working.** The `input` listener
that retargets the shown agent, the draft that holds a message while a
pane is switched, the file-attach path that appends into it, the
placeholder that names the agent, and the clear after a send — each
reads or writes `.value` and must still do so, including the growth
after a programmatic write (setting `.value` fires no `input` event,
which the draft-restore path already knows).

## Tests

- The page still parses, and the shipped-script inventory still
  covers it (`the_pages_scripts_parse`).
- The grow/shrink and the Enter/Shift+Enter split are page logic, and
  the plan that gives this page's logic a test harness
  (`the-pages-logic-can-be-tested`) is where they get asserted; if it
  has landed by then, assert them there, and if it has not, this plan
  does not invent a second harness.

## Out of scope

- Markdown, mentions, or anything else about composing.
- The transcript's own layout.
