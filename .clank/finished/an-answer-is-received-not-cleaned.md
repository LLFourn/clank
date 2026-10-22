# an-answer-is-received-not-cleaned
# An answer is received, not cleaned

## Why

When the human answers a block, the asking agent is woken with an
`unblocked` item that "fires until you acknowledge it — `clank block
clean`" (`wake_render.rs`, `K::Unblocked`). The agent's only way to
discharge it is a shell command that deletes files. Claude Code's
permission mode can refuse that command, and clank deliberately
installs no allow rules (`init.rs`, `LEGACY_CLAUDE_ALLOW_RULES`, retired
in c8f6f11c). When it is refused, nothing the agent can do ends the
wake. The stop hook re-fires on every turn end and the agent loops
begging the human to type `! clank block clean`. A user hit exactly
this.

Acknowledging an answer is housekeeping, not a decision. The agent has
received the answer the moment the wake carrying it is delivered into
its context. The monitor already knows that moment: presentation
custody records Delivered on the connection that carried it
(`turn_policy`, `turn_custody.rs`).

## What changes

### The answer a presentation carries is named exactly

- `WaitItem::Unblocked` carries an `AnswerRef`: the asking agent, the
  block name, and digests of the exact question bytes and answer bytes
  it rendered. `WaitItem::identity` includes those digests, so a
  re-asked question or a new answer under a reused name is a different
  item. The wake still renders the answer text (`wake_render.rs`
  already does). The `clank block clean` duty and `Duty::Unblocked`
  go away.

### Each ask is an occurrence with its own identity

Bytes do not identify a use of a block name: the same question can be
asked again and get the same answer. So every ask gets an occurrence
id, a random 128-bit value minted under the block lock and never
reused. It is written into the question file itself (a leading
`occurrence: <id>` line, stripped before rendering), so question and
id publish together in one temp-file + rename.

- `answer_block` reads the question's occurrence under the lock and
  writes it as the answer file's first line. An answer belongs only to
  the occurrence it names.
- The `AnswerRef` a presentation carries is (agent, name, occurrence,
  question digest, answer digest), and the received marker records the
  same tuple. `receive` matches on all of them.
- **The ask transition.** Under the lock, `block ask` removes any
  answer and marker left for that name before publishing the new
  question. Even without that removal, a leftover answer or marker
  naming another occurrence is ignored by every reader, so a new ask
  can never inherit an old answer or an old receipt. An interrupted
  clean is handled by the same rule: whatever it left behind names a
  dead occurrence.
- Legacy question files with no occurrence line get one written the
  first time they are touched under the lock: when they are presented
  for migration, answered, or re-asked.

### Receipt is one durable, atomic transition on the exact pair

- New primitive `block::receive(repo, &AnswerRef)`, taken under a
  short per-agent block lock (`.clank/agents/<label>/blocks.lock`).
  Under the lock it re-reads `blocks/<name>.md` and `unblocks/<name>.md`
  and compares both digests to the ref. Only on a match does it publish
  `unblocks/<name>.received`, holding the two digests, by temp-file +
  fsync + rename. That one rename is the retirement. A mismatch (the
  name was re-asked, re-answered or cleaned since it was presented) is
  a no-op, logged with both digests.
- A pair whose `.received` marker matches both current digests is
  retired. It is not presented, not counted as pending or answered-owed,
  and never blocks finish. A marker that does not match the current
  pair is stale and ignored, so a reused name is never mistaken for an
  already-received answer. Deleting the files is housekeeping and is
  never what decides retirement: `clank block clean` (still a human
  maintenance command) removes retired triples and stale markers.
- `block ask`, `block answer`, `block clean` and `receive` all take the
  same lock around their read-compare-write, so a new question or
  answer cannot land between receive's comparison and its rename.
- Crash behaviour: a crash before the rename leaves the answer owed,
  and it is presented again (at-least-once). A crash after the rename
  cannot revive it, because the marker is the retirement. A failed
  receive leaves the answer owed and logs why.
- Inspection is read-only. `clank status`, `clank wait --peek`, the TUI,
  `block` listings and `events` views never call `receive`.

### Every successful delivery boundary reaches the same primitive

There are exactly two places where clank knows that a wake reached the
model, and both already call `presentation::record`:
`turn_custody.rs` on a Delivered receipt (the Claude, Codex and
OpenCode hook and plugin routes, including SessionStart catch-up
presented through custody), and `wait.rs`'s automatic Grok wait after
`emit_checked` succeeds on `Shared::Woke`.

- Replace that call with one `presentation::record_delivered(repo,
  author, &Carried { events, answers }, current)`, which records event
  presentations and receives each carried `AnswerRef`. Both boundaries
  pass the whole carried selection, so neither can forget answers.
  `events read`'s existing use keeps recording events only.
- The `current()` binding check gates answers exactly as it gates
  events. A stale binding receives nothing.
- EmitFailed, EOF with no receipt, failed output on the Grok path, and
  a presentation that never lands receive nothing, so the answer is
  presented again.
- No PostToolUse hook and no handoff registration handshake. This uses
  the existing provider and session contracts.

### The rest

- `clank finish`'s open-block refusal (`finish.rs`) counts only
  unanswered blocks. An answered pair, received or not, never blocks
  finalization.
- Migration: pairs already answered when the new binary starts have no
  marker, so they are presented once more with their answer and
  received on delivery. They are never silently dropped.
- Docs stop teaching agents to clean after an answer: the master and
  reviewer skills, the README's block text, and the book's block
  workflow (`book/group-coding.html`, `book/for-agents.html`). The
  `book_is_true` gate still passes.

## Tests

All of this is one implementation commit after the intro is approved.

- **Turn custody, success:** answer a block, drive a turn end through
  policy and custody with a Delivered receipt. The marker is written
  for that exact pair, and the next turn end carries no `unblocked`
  item.
- **Turn custody, failure:** EmitFailed / no receipt leaves the answer
  owed and presented again.
- **Grok:** drive the automatic wait's `Shared::Woke` branch. Success
  receives the answer; an `emit_checked` failure, or a binding that
  went stale before recording, does not. Test through the real branch,
  not only the primitive.
- **Two answered blocks, one presented:** only the presented one is
  received.
- **Late receipt after reuse:** present answer A for `name`, then
  re-ask `name` with identical question bytes and answer it with
  identical answer bytes. Run this case directly, then again after
  `clank block clean`, and again after a simulated restart. Then
  deliver the receipt for A. In each case it is a no-op and the new
  occurrence stays owed. Mutants: matching by name alone, and matching
  by name plus digests without the occurrence.
- **Re-asking does not inherit:** re-ask a name that has an answer and
  a marker. The new question is unanswered and owed to the human, and
  no old answer is rendered for it.
- **Duplicate receipts and restart:** two receipts for the same ref
  write one marker. After a simulated restart the received answer is
  not presented.
- **Crash ordering:** a marker present without file deletion counts as
  retired. Files present with no marker count as owed.
- **Read-only inspection:** `wait --peek`, status and TUI derivation
  leave no marker.
- **Finish:** an answered pair without a marker does not block finish.
  An unanswered block still does.
- **Wake text:** it contains the answer and no `clank block clean`
  instruction.

## Out of scope

- Installing permission allow rules for clank commands.
- Changing how events are acked. An event is handled, not merely
  received, so it keeps its explicit ack.
