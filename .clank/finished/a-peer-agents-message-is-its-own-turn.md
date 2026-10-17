# a-peer-agents-message-is-its-own-turn
# A message from another agent session shows in Chat history as that agent's message: attributed, structured, never as the person's

## Why

Reported by Life's assistent for Lloyd, 2026-10-17 (screenshot: `~/src/life/notes/attachments/clank-agent-message-rendering-2026-10-17.jpg`, secg-search / claude, 10:33). A subagent's report showed as Lloyd's own beige bubble, beginning with "Another Claude session sent a message:" and the literal `<agent-message from="ab3b5297c68b0ddd2">` envelope. Its numbered lines ran together into one paragraph, and the bubble filled the viewport.

Confirmed in `~/.claude/projects/-Users-llfourn-src-secg-search/9d65474e-38cd-47db-aef0-f82e21b527cd.jsonl`. Claude Code records a peer message in two shapes, and both carry structured provenance:
- **Delivered while Claude is idle** (lines 3107, 3136, …): `type: "user"`, `isMeta: true`, `turnOrigin: "peer"`, `promptSource: "system"`, with `origin: {kind: "peer", from: <task id>, senderTaskId, name: "general-purpose", body: <the message alone, newlines intact>}`. `message.content` is the envelope string: preamble, `<agent-message from=…>`, body, `</agent-message>`, and a trailing safety note.
- **Absorbed mid-turn** (lines 3026, 3045): `type: "attachment"`, `attachment: {type: "queued_command", commandMode: "prompt", origin: {kind: "peer", …same fields…}, prompt: <the envelope>}`.

The parser causes all of it; the page renders what it is given:
1. **Misattributed.** `claude::user` reads the idle shape's string content as `Who::Person`, because `is_harness_text` does not know `agent-message`, and it ignores `origin`, `isMeta` and `turnOrigin`.
2. **Missing.** The mid-turn shape is dropped by `absorbed_prompt`, correctly, since it is not human. So those peer messages are missing from Chat history entirely.
3. **Flattened.** The envelope string goes through `Body::prose`, which renders markdown, so single newlines between `(1) … (2) …` collapse into one paragraph.

## What to build

**One parse for both shapes.** A line whose `origin.kind` is `peer`, whether it is the `user` shape or the `queued_command` attachment shape, becomes one turn with:
- a new `Who::Peer`;
- a body carrying the sender's `name` and its `from` id as metadata, and `origin.body` as the text;
- the line's uuid and timestamp.

The text is `origin.body` verbatim. The envelope, the preamble and the trailing safety note are not user-facing prose, and are never parsed out of the string when the structured `body` is present. With no `origin.body`, fall back to the content inside `<agent-message …>…</agent-message>`, with the envelope removed.

**Rendering.** The body is rendered safely. Raw HTML stays escaped, through the existing `html::render_markdown`, and line breaks are kept: a single newline in the body is a line break, so numbered lines stay on their own lines.

**The page.** A peer turn is distinct from the person's bubble and from the receiving agent's own answer:
- a labelled card, "message from `<name>`", with the id available but not leading;
- on a narrow screen it does not cover the composer; a long body is collapsible, or scrolls within the card, and stays fully reachable.

**Never the person's.** A peer turn is never `Who::Person`, so it can never settle a pending outbox row, and never reads as Lloyd's instruction.

## Acceptance

- **Parser:** the idle `user` shape and the mid-turn attachment shape, as recorded, each parse to one `Who::Peer` turn whose text equals `origin.body` and whose sender is `general-purpose`.
  - Neither contains `<agent-message`, the preamble, or the safety note.
  - A human `queued_command` still parses as before.
- **Rendering:** a body with single-newline numbered lines keeps them on separate lines, and an embedded `<script>`-like string stays escaped.
- **Page** (browser check, narrow and wide viewports): the peer turn renders as a labelled card distinct from `.turn.person`; a long body stays reachable without covering the composer; it does not retire a pending outbox row with the same text.

## As built

- **`web/transcript.rs`:**
  - `Who::Peer`, and `Body::Message { name, from, text, html }`, built by `Body::message` with `html::render_markdown_lines`.
  - In `claude::parse`, after the sidechain exclusion and before the absorbed-human path, any line whose `origin.kind` (top level, or the attachment's) or `turnOrigin` is `peer` goes to `peer_message`.
  - `peer_message` builds the turn:
    - the text is `origin.body` verbatim;
    - only without a body is the `<agent-message …>…</agent-message>` envelope opened (`envelope_body`), and only for a line already identified as a peer's;
    - a peer line yielding no text is dropped and never falls through to `Who::Person`;
    - `name` and `from` come from `origin`, and the id and time from the line.
  - A person's own typed text that quotes the tag stays `Who::Person`.
- **`html.rs`:** `render_markdown_lines` is `render_markdown` with soft breaks rendered as hard breaks. It is used only for peer messages; plain prose is unchanged. Raw HTML stays escaped either way.
- **`page.html`:**
  - a `message` turn renders as a `.turn.peer` card with a "message from `<name>`" label, the sender id behind a collapsed `id` toggle, and the server-rendered body (text as the fallback);
  - the body scrolls within the card, at most 50 vh;
  - outbox reconciliation still matches only `who === 'person'`.
- **Tests:**
  - `web::transcript`: the idle `user` shape and the mid-turn attachment shape, as recorded, each give one `Who::Peer` message with name, id and body verbatim, kept line breaks, and no envelope, preamble or safety note in text or html; without a structured body the envelope is opened; a peer line with no text is dropped; a sidechain peer line is excluded; a person quoting the tag keeps their own attribution.
  - `html`: `render_markdown_lines` keeps each line and escapes raw HTML, while `render_markdown` is unchanged.
  - `tests/browser/check.js`, at 390 px and 1280 px:
    - the peer turn is a labelled card, not a person bubble;
    - it does not settle a pending outbox row with the same text;
    - a long body scrolls within the card while the composer stays on screen;
    - escaped markup stays text;
    - no page errors.
- **Mutations, each caught:**
  - no peer branch;
  - a text-less peer line falling through to the person;
  - soft breaks for peer messages;
  - no in-card scroll.

### Revision after codex on 19d9b1f

- **Identification before extraction.** Peer provenance is decided first, by `is_peer`: any marker counts, either an origin (top level or the attachment's) of kind `peer`, or `turnOrigin: peer` alone. Only then does `peer_message` read the metadata, all of it optional:
  - the origin's `body`, else the envelope;
  - the origin's `from`, else the envelope's `from` attribute (`envelope_sender`);
  - the origin's `name`, else empty.

  A peer-marked line with no usable text is dropped. It outranks the absorbed-human fallback.
- **Test:** `a_peer_marker_without_an_origin_is_still_a_peers_line`. A `turnOrigin: peer` line with no origin and an envelope gives a peer turn from `task`. The same marker with no envelope is dropped. An absorbed `queued_command` with `humanTurn: true` but a peer marker is dropped, not made the person's. The old origin-required identification is mutation-caught by it.

### Revision after codex on 5d99b28

- **Each origin checked independently.** `origins` yields every origin object on the line (top level, then the attachment's), skipping `null` and missing ones. `peer_origin` finds the one with `kind: peer` wherever it sits, so an unrelated top-level value (`null`, `{}`, `{kind: human}`) cannot hide an attachment's peer origin.
- **Identification:** `is_peer` is `peer_origin` or `turnOrigin: peer`.
- **Extraction:** metadata comes from `peer_origin`, or, for `turnOrigin`-only lines, from whichever origin object exists.
- **Test:** `a_nested_peer_origin_is_not_shadowed_by_a_top_level_one` covers `null`, empty and human top-level origins over a nested peer origin with `humanTurn: true`. Each gives a peer turn with its name, sender and body. Restoring first-origin-wins is caught.
