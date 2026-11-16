# block-answers-are-best-effort

Invariants: I3, I6. After `blocks-have-no-migration`.

## Why

An answered block is delivered to the master and then recorded as "received" per `Destination{agent, tool, session}` (`block::receive`, `presentation.rs:150-165`, `unblocks/*.received`). After any rebind (`/clear`, restart) the new session has received nothing. Every un-cleaned answer is shown again, one per turn, pausing review each time.

The user's decision: answer delivery is best effort with no state. Try to deliver, and once the wake carrying the answer has been emitted, delete both the block and the answer.

## Build

1. **What a completion deletes.** Each delivery carries the answer's `AnswerRef` in memory: name, occurrence, question digest and answer digest. Today's comparison stays: under the repository block lock, the pair on disk must still carry exactly that ref, or the completion deletes nothing. So a delayed or stale completion can never delete a replacement question or answer, even one with identical text under a reused name, because a new ask has a new occurrence. The ref is never written down; it lives only as long as the delivery does.
2. **Deletion order.** Under the lock, the question file goes first, then the answer, the creator and any attempt record.
   - The question is what makes a block pending, so once it is gone the block can never be pending again, whatever is left.
   - Leftover answer, creator or attempt files with no question are inert. Nothing reads an answer without its question.
   - A new ask of that name removes the leftovers, still under the lock, before it writes its own occurrence. So leftover answer bytes can never attach to a future ask.
3. **Reuse.** Re-asking a name whose question still exists, pending or answered and undelivered, is refused, naming the existing block. Once the block is delivered and deleted, the name is free. Refusing needs no ordering of writes, and replacing would discard an answer no one has seen.
4. **What completes each path.**
   - **Wake:** the hook reports the emit succeeded (`TurnResult::Delivered`). An emit failure deletes nothing, and the answer is offered again on the next wake.
   - **`clank block answer --nudge`:** only an attempt whose outcome is `Outcome::Submitted` completes it. `Refused` and `Indeterminate` delete nothing. An outstanding attempt still stops automatic re-typing, as today, and `--resend` or `--delivered` settles it.
   - **`--delivered`:** the person has seen a nudge's text arrive and says so. This is the explicit confirmation, and it completes the block.
   - **A permission answer the person types themselves:** clank cannot see that it happened. It completes only when the person runs `clank block answer <name> --delivered`. The person-only permission check (`--override-permission-classifier`) stays.
5. Delete:
   - `block::receive`, `Receipt`, `Destination` as a delivery key, and `.received` files
   - the `answers` half of `presentation::Carried` as a durable receipt; the in-memory ref in step 1 replaces it
   - the ledger keys tied to received answers
   - nudge and attempt bookkeeping that only existed to prove receipt, keeping the attempt record that blocks double typing
6. Surfaces that promised answers stay readable after delivery: `clank block list` ("including received decisions"), `clank block clean` (settled state), the master skill ("Your wake records receipt; `clank block list` keeps decisions available until explicit repository cleanup") and the README. They say an answer is gone once it is delivered. If `clean` has nothing left to do, delete it.
7. I3 in `docs/invariants.md` stays a target until `one-state-file-per-agent` lands. Update its `Today` to drop block answers.

## Acceptance

- An answer delivered in one emitted wake leaves no block files.
- After a master rebind nothing is replayed.
- An emit failure leaves the answer in place and it is delivered on the next wake.
- Mutation: deleting before the emit fails the emit-failure test.
- A delayed completion for an old occurrence, arriving after the name was delivered and re-asked, deletes nothing.
- Cleanup interrupted after the question is removed leaves nothing pending. A new ask of the name does not pick up the leftover answer.
- Re-asking a name with an undelivered pair is refused.
- Nudges: a `Submitted` nudge completes the block. An `Indeterminate` one does not, and does not type again on its own. A later `--delivered` completes it.
- A permission block completes on `clank block answer <name> --delivered`, never on its own.
