# a-nudge-is-typed-plainly-and-confirmed
# A nudge is typed plainly, after a space, and confirmed by its record

## Why

In `~/src/life` a manual `clank nudge` to the master (Claude) left
`loyd voice316 …` in its input and never pressed Enter. Logged outcome:
"the interrupt was delivered, but the message was not confirmed …: after
typing, its input read as text clank did not type". Two causes, both
reproduced live (Claude Code, a 50-column scratch pane):

1. **Esc on an idle Claude is not inert.** `nudge` presses Esc first. The
   master was idle. A key typed straight after an idle Esc is swallowed:
   `Esc`, a dump-screen, then `Hello world` shows `ello world`, twice out
   of two. Waiting 0.05–1.5s after the Esc also kept the text intact, but
   that is a race, not a fix.
2. **The exact screen match models Claude's wrap wrongly.** `measure`
   rebuilds the payload from the drawn rows. It treats a row break as a
   wrap space only where the next word would overflow `shown.width`,
   which is the rule's full width. Claude wraps at **width − 2**, both
   between words and inside a long word (measured: 50 columns → rows end
   at column 48). In life, the row `… Status` ended at 115, and the next
   word `currently` would have ended at 125 = width. So clank saw a hard
   line break where the payload has a space, read the input as foreign
   text, and stopped before Enter.

The deeper problem is cause 2's whole class. Reconstructing the payload
exactly from the screen is the fragile part of `deliver`, and its only
purposes were the "clear a suffix and retype" rule and seeing the whole
text before Enter. Lloyd's call: drop it, and put a space before each
message, so the one key a terminal race swallows is the space. Verified
live: `Esc` then ` Hello world` gives `Hello world` intact (3/3, space
swallowed 2/3), and a message opening with a space submits and runs
normally.

## What changes

`pane_input::deliver` for Claude and Codex becomes:

1. **Look once.** Proceed only if the input reads as **empty**. Anything
   else (any draft, unreadable, shell mode) is refused untouched, and
   nothing is sent. No suffix or whole-payload admission.
2. **Interrupt only a running turn.** With `interrupt`, Claude gets Esc
   only while its turn is running (the existing `claude_turn_running`).
   Idle, no key. Codex keeps its current interrupt behaviour. After an
   interrupt is actually sent, look again: the input must still read as
   empty before any text is typed. The earlier frame does not cover a
   draft shown after the interrupt. If it isn't empty (a draft,
   unreadable or shell mode), stop with only the interrupt sent, and
   report it as interrupted but unconfirmed.
3. **Type a space, then the payload,** in one `write-chars`.
4. **Wait a second, then press the submit key once.** This is the
   existing per-tool key: Claude Enter; Codex kitty Enter, or Tab when
   queuing.
5. **Confirm by the harness's record past the mark**, as now. The record
   must hold the complete payload, with the leading space present or
   absent. A record with any other character lost does not confirm.
   There is **no second submit key**: wait for the record until the
   deadline, and without it the outcome is Indeterminate. The old
   Claude-only second Enter is removed. A non-empty input after the
   first Enter doesn't show that clank owns it: the first Enter may have
   landed with its record late, and a person may have started a
   different draft. A lost first Enter ends Indeterminate and is not
   replayed. The one-second wait before Enter covers the startup window
   in which Enter was seen lost (0.6–0.8s after launch).

Removed, with their tests:
- `Held::{Whole, Suffix, Arriving}`
- `measure`, `Tok`, `glob`, `pieces` and `placeholder` in pane_input,
  `starts`, `type_and_see`, `clear` and Ctrl-C (`clear_argv`), and
  `may_clear`
- the row/width model in `composer::typed` that only `measure` reads
  (keep what empty-detection needs)
- `Delivered::Altered` with `nudge_log::Outcome::Altered` and
  `recovery::Answer::Altered`

Keep `Altered` deserialisable only if existing `nudges.jsonl` lines are
parsed back. Check that, don't assume it.

The 8-second `Attempt` deadline, the pane lock, the binding/`Receipts`
record confirmation and the outcomes `Submitted`, `NotSent` and
`Indeterminate` stay.

Docs: README nudge paragraphs and any skill text describing the suffix
or the retype rule are updated to say what is done now.

## Acceptance

- A Harness test: idle Claude with `interrupt` gets no Esc. A busy one
  gets Esc, then text.
- The typed text is `" " + payload`.
- A non-empty input of any kind gets no write at all.
- A record of the payload with or without its leading space confirms
  Submitted.
- Exactly one submit key per attempt. In a fixture, the first submit is
  accepted, its record is delayed, and an unrelated draft then appears:
  no second key is sent, the draft is untouched, and the outcome is
  Indeterminate. A lost first Enter ends Indeterminate without replay.
- In a fixture, the input is empty before Esc and holds a draft
  afterwards (also unreadable and shell mode): only the interrupt is
  sent, with no text and no submit key, reported as interrupted but
  unconfirmed. An idle Claude still gets no Esc.
- A record missing any character other than the leading space does not
  confirm.
- Mutation checks: each of these fails a test:
  - removing the busy guard on Esc
  - dropping the leading space
  - admitting a non-empty input
  - skipping the post-interrupt look
  - restoring a second Enter
- `cargo test -p clank --lib` passes for pane_input, prompt_record,
  composer, nudge, nudge_log, recovery and monitor, then the whole lib.

## As landed

- `nudges.jsonl` lines are read back (`nudge_log::read`, used by the
  timeline, recovery and the monitor). Old `altered` lines therefore read
  back as `Submitted` through a serde alias.
- `composer::typed`, `Shown` and `Row` are gone; only `composer()` and
  `claude_turn_running` remain.
- `prompt_record::attribute` became `is_payload`, a whole-text match with
  the surrounding whitespace ignored.
- The one-second wait before the submit key also covers Codex's paste
  window. `CODEX_PASTE_WINDOW` remains only for `say_on`.
