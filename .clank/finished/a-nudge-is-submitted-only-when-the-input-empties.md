# A nudge is typed, seen, then sent

## Problem

In `~/tmp/backup-bip-experiment` (2026-10-30), the master ran `clank nudge`
on four agents about 2 s after `clank agent start`. All four were logged
`submitted`.

- opus-v4 and haiku-v4 got the text with its first character eaten
  ("xperiment rules…"). Claude had queued it behind their launch turn.
- sonnet-v4 got neither of its two nudges. Its pane still shows both texts
  unsent in the input box, run together. The first was typed 40 ms after its
  launch turn ended; the second, at 02:32, landed on the first's leftovers.

Lloyd wants the nudge to always work and never drop a character.

### What a live Claude does (Claude Code 2.1.308, scratch zellij session)

The keys were clank's own: `write-chars` of the text, then byte 13.

| When the keys arrive | Result |
|---|---|
| Idle | submitted, text intact (4/4) |
| Busy mid-turn | queued, then sent intact |
| The instant a turn ends | submitted (4/4) |
| Text and CR in one write, idle | submitted |
| 0.45–0.55 s after launch | text lost entirely |
| 0.6 s and 0.8 s after launch | text in the box, Enter lost (sonnet's state) |
| 0.65, 0.7, 0.9 s after launch | submitted |

Ctrl-C on a non-empty idle input clears it and nothing else.

So the failures live in Claude's launch window, and inside it the same keys
give different results from run to run. No fixed delay is safe, because a
real agent's launch (MCP servers, plugins) runs longer than the scratch one.
The eaten first character did not reproduce. Whatever its cause, only
reading the input back before pressing Enter can catch it.

The fault in clank: `submit_on` and `interrupt_then_type` (`pane_input.rs`)
write the text and Enter blind, then report success. `admit_write` admits a
`Draft`, so a second nudge appends to an earlier one's leftovers.

## Change

The proof that a prompt arrived is native: Claude's transcript gains a user
record, or a `queue-operation` enqueue when it is busy, at the moment it
accepts input. In the experiment the stamps matched the typing to the
millisecond: haiku's enqueue was at 02:28:34.163, its nudge at 02:28:34.16.
Codex's rollout records a user message the same way. The record holds the
whole payload, so it can be compared in full, not by its first line. The
screen is used only to decide when it is safe to press a key.

One sequence is used by every path that types a prompt into a Claude or
Codex pane: `submit_on`, `interrupt_and_submit_on` and the manual nudge's
`interrupt_then_type`. It runs under the existing pane lock, inside one
absolute deadline. Reads happen only during an attempt: no idle polling and
no new ids.

1. **Touch no text clank didn't type.** A fresh read is taken before typing
   and again after any interrupt. The only text clank acts on is text that
   can only be its own: the payload it is about to write, whole or as a
   non-empty proper suffix of it. However many leading characters are
   missing, it is the same situation as the observed one-character loss.
   - `Empty`: type.
   - The whole payload: the box holds it, so pressing the submit key sends
     exactly that. Whether an earlier Enter was lost is not claimed; on
     Claude an Enter on an input already emptied is a no-op (see the key
     contract below). This is sonnet's stuck state. Press the submit key
     once without retyping, and go to step 3.
   - A non-empty proper suffix of the payload, with no turn running:
     clear it with Ctrl-C, confirm `Empty`, and type.
   - Anything else: do nothing. It is refused as a known no-send, never
     cleared or appended to, and the log says a draft was in the way.
2. **Type once, then look before submitting.**
   - The composer shows the payload intact: submit once.
   - The composer shows a non-empty proper suffix of the payload (the
     dropped first character), with no turn running (no interrupt hint on
     screen): by step 1's rule this is ours, and nothing is queued. Clear them with Ctrl-C, which the experiment showed only empties
     a non-empty idle input. Confirm `Empty`, and retype once. While a turn
     runs, Ctrl-C would interrupt it, so the outcome is indeterminate instead.
   - Nothing appears within the budget: the bytes may still arrive, so do not
     retype. The outcome is indeterminate, and step 1 of a later attempt
     finds the leftover if it lands.
   - Unreadable: indeterminate.
3. **Confirm by the transcript, attributed to this attempt.**
   - *Where to look.* Before the first write, note the bound session's
     transcript and its length. Only records appended after that point, in
     that file, under that same binding, count. If the file shrinks or is
     replaced, or the binding changes, the outcome is indeterminate.
   - *What counts.* A record of accepted user input: a user turn, or
     Claude's `queue-operation` enqueue when it is busy. An enqueue and the
     user turn it later becomes are one acceptance. Hook feedback,
     task-notifications and compaction summaries (`isCompactSummary`) are
     not user input and are skipped. A record from before the start point
     never counts, even if its text is identical.
   - *Matching.* The whole payload, equal after the harness's own wrapping
     (Claude's `<pasted_content>` tags) is removed: *submitted*. A non-empty
     proper suffix of the payload: *submitted, altered*. This is the same
     provenance rule as step 1, so the record can only come from this
     write; it was delivered and is never resent. Any other new record is
     not ours, for example a person typing; it is skipped and the wait
     continues.
   - *A second submit key, Claude only.* If no matching record has appeared
     and the composer still shows the whole payload after a settle delay,
     press Enter once more. This does not rest on the record being absent,
     since a record can be written late. It rests on a key contract
     observed live (Claude Code 2.1.308): an Enter on an empty input does
     nothing, whether Claude is idle, busy, or has a prompt queued (the
     queued prompt still ran once). So if the first Enter was accepted and
     the frame is stale, the box is really empty and the second Enter is a
     no-op. If the box really holds the payload, the Enter sends it. Codex's
     submit keys (Tab, which queues, and the kitty Enter) have no such
     verified contract, so Codex gets one submit only.
   - No matching record by the deadline: indeterminate.
4. **Outcomes, kept apart.**
   - *Submitted* and *submitted, altered*: delivered.
   - *Not sent*: refused before any write, or a positively classified
     `NotStarted` failure. Recovery retries it under its existing budget.
   - *Indeterminate*: everything else after bytes may have arrived. The
     existing `Unconfirmed`/`Indeterminate` handling is unchanged: no blind
     replay, no forged receipt, and pending work stays pending. A later
     attempt's step 1 still reconciles any leftover by its exact text.

   The nudge log records which outcome happened.

The launch window is where this was seen failing. It is an observed failure
mode, not a claim that input is safe everywhere else; the sequence does not
depend on it.

## As landed

- `pane_input::deliver` is the one sequence. `prompt_record` reads the
  harness's record past a mark, incrementally. The binding's key is its
  tool, id and stored transcript, so moving to another transcript under the
  same id ends the attempt indeterminate.
- `Attempt::within` bounds every read, write, settle and record read by the
  one 8 s deadline. A result at or after it counts for nothing.
- Whitespace is compared exactly. A row break stands for a space only where
  the row is provably full, and anything unproven is refused.
- Codex is never sent Ctrl-C, since for Codex it quits; a Codex suffix is
  refused before typing and indeterminate after. Claude gets at most one
  Ctrl-C per attempt, because a second on an empty input exits.
- Paste placeholders (`[Pasted text #N +K lines]`, `[Pasted Content N
  chars]`) stand for clank's text only after clank typed it.
- The outcome *submitted, altered* is `nudge_log::Outcome::Altered` and
  `recovery::Answer::Altered`.
- The manual nudge into Grok and OpenCode stays blind, as before. The web
  bridge's `say_on` is unchanged.

## Out of scope

- OpenCode and Grok, whose input cannot be read; they keep their refusal.
- Draft handling for callers outside these three paths, such as the user
  command's `!` line, which keeps its own contract and tests.

## Acceptance

- Fixtures play screens and transcript records in sequence, through the real
  writer paths (`submit_on`, `interrupt_and_submit_on`, the manual nudge):
  - a stale `Empty`, then the text appears;
  - text that appears only after the budget: no retype, indeterminate;
  - a missing first character while idle: cleared, retyped, submitted;
  - the same while a turn runs: no Ctrl-C, indeterminate;
  - a record whose first line matches but a later line does not: not
    attributed, so not a confirmation; it ends indeterminate and is not
    resent;
  - the first Enter accepted while the dump still shows the draft, its
    record appended only after the settle delay: at most one more Enter,
    and one *submitted* outcome;
  - the Enter lost (full draft visible): exactly one more Enter on Claude,
    none on Codex (Tab and kitty Enter paths), which ends indeterminate;
  - an identical record from before the attempt: not a confirmation;
  - an unrelated user record appended during the attempt: skipped;
  - a compaction summary or hook feedback appended: skipped;
  - an enqueue followed by its user turn: one acceptance;
  - a record that is a proper suffix of the payload: *submitted, altered*;
  - a transcript that shrinks or a binding that changes mid-attempt:
    indeterminate;
  - a draft that is the whole payload: Enter only, no retype;
  - a draft that is a proper suffix of the payload, short or long, idle:
    cleared, typed;
  - a foreign `Draft`, including the payload with text appended or a
    suffix with one character changed: refused, no key sent;
  - unreadable mid-attempt: indeterminate.
- Mutations, each failing a test:
  - confirm on composer clearance instead of the transcript record;
  - compare only the first line;
  - count a record from before the attempt's start point;
  - send a second submit key on the Codex path;
  - allow Ctrl-C while a turn runs.
- Recovery retries a known no-send and does not replay an indeterminate one.
- The `pane_input`, `composer`, `nudge`, `nudge_log` and `recovery` tests
  pass.
- The live table is recorded as observed evidence. The fixtures check
  clank's sequence; they do not claim to be the harness.
