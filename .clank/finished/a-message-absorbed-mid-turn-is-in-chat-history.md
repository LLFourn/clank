# a-message-absorbed-mid-turn-is-in-chat-history
# A message Claude absorbs mid-turn appears in Chat history as the person's turn, so its pending bubble reconciles

## Why

Reported by Life's assistent for Lloyd, 2026-10-17. In the web chat (secg-search / claude), a sent message stayed as a grey pending bubble, "Waiting for Chat history…", although Claude had received and acknowledged it. The screenshot is at `~/src/life/notes/attachments/clank-chat-pending-2026-10-17.jpg`.

Confirmed in `~/.claude/projects/-Users-llfourn-src-secg-search/9d65474e-38cd-47db-aef0-f82e21b527cd.jsonl`:
- Line 2742: `queue-operation remove`, `reason: "absorbed_mid_turn"`.
- Line 2743: a line with `"type":"attachment"` and `attachment: {type: "queued_command", prompt: "<the exact text>", commandMode: "prompt", origin: {kind: "human"}, humanTurn: true}`. It has no top-level `message` field.

The cause, in two parts:
1. **The parser drops the line.** `web/transcript.rs` `claude::parse` returns nothing for any line without `message`, so a message typed while Claude is working never becomes a person turn.
2. **So the bubble never retires.** `page.html` `reconcileOutbox` retires an outbox entry only when a later person text turn carries the identical text.

Send identity, acknowledgement and session routing are not involved.

## What to build

**Parse the attachment.** `claude::parse` turns a line with `type: "attachment"` into a person text turn when its attachment has:
- `type: "queued_command"`;
- `commandMode: "prompt"`;
- a human origin (`origin.kind: "human"`, or `humanTurn: true`).

The turn's text is `attachment.prompt`, its id is the line's `uuid`, and its time is the line's `timestamp`. Sidechain lines stay excluded, as today.

**Everything else unchanged.** Other attachment kinds, and queued commands that are not human prompts (slash commands, other origins), produce nothing. The page's reconciliation is unchanged: it now finds the turn.

## Acceptance

- A transcript line in the exact recorded shape parses to one `Who::Person` text turn with the prompt's text and the line's uuid.
- A non-prompt or non-human `queued_command`, and other attachment types, parse to nothing.
- The page's reducer test (`feed.rs` `PageModel`, or the browser check) shows an outbox entry for that text retired once the turn arrives.
- No resend, and no other outbox entry is touched.

## As built

- **`web/transcript.rs` `claude::absorbed_prompt`**: a line with `type: "attachment"` whose attachment has `type: "queued_command"`, `commandMode: "prompt"`, and a human indicator (`origin.kind: "human"` or `humanTurn: true`) becomes a `Who::Person` text turn.
  - The text is `prompt` verbatim, the id is the line's `uuid`, and the time is the line's `timestamp`.
  - The turn is built directly as `Who::Person`, so no injected-text heuristic can reclassify it.
  - It runs after the sidechain exclusion and before the `message` guard.
  - `queue-operation` bookkeeping lines still parse to nothing.
- **Page:** unchanged. `reconcileOutbox` now finds the turn.
- **Tests:**
  - `cli::web::transcript`:
    - the recorded shape, with curly quotes, under each human indicator, gives one person turn with the line's uuid and timestamp;
    - the absorbed turn serializes exactly like a typed message's turn, apart from its id;
    - a non-human origin, a non-prompt mode, another attachment type, a sidechain line and a `queue-operation` line are not turns.
  - `tests/browser/check.js` (by hand, the shipping `page.html`), first section:
    - two pending submissions, then the absorbed turn arrives: only its own row retires;
    - a replay leaves the other row pending;
    - `/say` was called exactly twice, with no resend;
    - no page errors.
  - The rest of `check.js` fails the same Timeline checks with and without this change, then times out at that section, so the new case sits first in order to run.
- **Mutations, each caught:** not calling the parser; accepting any origin; accepting any mode.
