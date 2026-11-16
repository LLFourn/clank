# the-serve-loop-survives-a-held-blocks-lock

Invariants: I9.

## Why

Serving a report takes `.clank/blocks.lock` with a blocking `flock` on the monitor's loop: `serve_reports` → `gather` → `admitted_items` → `wait::admitted_for` → `check_blocks` → `block::owed_answers` / `scan_blocks` (`block.rs`), and `Gathered::of` → `wait::blocked`. The fold's `serve_to` (`owed_for`), the retirement's final `serve_to_cached`, `FsPlanStateLookup::blocks_for` and `recovery_facts` → `block::ended` reach the same lock. Agent and CLI processes hold it: every `clank block` subcommand (`list` holds it while printing to stdout, so a full pipe holds it indefinitely), and a `clank wait` client's own `check_blocks`. One of them stalls serving for every agent.

The loop needs a result from this lock: which blocks pend and which answers are owed decide what a verdict presents and what it suppresses. `ended` dates answers for the ledger. `ended` and `owed_answers` already read a lock failure as "no blocks", so a naive try-lock would turn contention into "nothing blocks" and present work a block should hold.

## Build

Readers stop taking the lock; only writers take it. A lockless read must be coherent and fail closed. Atomic publication of each file is not enough on its own (review of 8f4f82b):

- `Pair::read` reads the question, answer and attempt separately. A completion and a reuse can land between those reads.
- An unstamped question pairs with any answer, so an old question could be presented with a new occurrence's answer.
- `ended` reads the answer's metadata after the pair, so it can date one occurrence from another's file.
- Correction to the earlier draft: an occurrence mismatch reads today as no answer (`(None, None)`), not as damage.

1. **A coherent read, by question generation.** A question's generation is its file identity: device, inode, length and modification time, from `symlink_metadata`. Every publish is a rename, so a new ask or a rewrite gives a new inode, and a completion removes the file. A read goes:
   1. take the question's generation;
   2. read the question, the answer (with the answer's own metadata, for `ended`) and the attempt;
   3. take the question's generation again.

   If the two generations match, the read is coherent. If they differ, or the question has vanished, the read retries, up to 4 times. Past that it is `Unsettled`. Nothing is presented or owed from an `Unsettled` read: the name counts as a pending block and holds work back, and it is reported as changing. An answer is paired only within one coherent read, under today's pairing rule unchanged. A stable unstamped question beside a stamped answer from an older writer stays answered (`a_half_stamped_legacy_pair_stays_answered`): the generation check is what stops a mixed read, not a stricter pairing. The answer's bytes and its metadata for `ended` come from one opened file handle (`File::open`, then `metadata` and read on that handle), so a later replacement at the path cannot supply the timestamp.
2. **Read errors fail closed, everywhere.** Only a genuine `NotFound` is absence.
   - `question_names` returns its error instead of flattening a `read_dir` error, an entry error or a `file_type` failure.
   - `Pair::read` distinguishes `NotFound` from any other `symlink_metadata` error.
   - `scan_blocks` uses `symlink_metadata` instead of `exists()`.

   An unreadable block directory, or a question that cannot be inspected, becomes a blocking damage entry (`unreadable`), never an empty set. `owed_answers`, `ended`, `check_blocks`/`wait::blocked` and `FsPlanStateLookup::blocks_for` all consume that one outcome type, so every serve and admission reader treats damage and `Unsettled` alike: blocking, owing nothing.
3. **Writers** (`ask_with`, `answer_block`, `nudge`/`settle`, `complete`) keep `block::lock`. Each re-reads under the lock before deciding, as today. Under the lock a read is always coherent.
4. **Registry:** I9's `Today` drops the blocks lock, and this plan leaves I9's `Plans`.

## Acceptance

- **Held lock:** another thread holds `blocks.lock` for the whole test. The monitor serves a report from an agent a pending block holds back, holding it back, and a block-free report, then retires, all before the lock is released. The test uses a channel, no sleeps, and is bounded by a timeout.
- **Interleavings inside one read,** through a seam between the read's steps:
  - completion and reuse between the question read and the answer read, for a stamped and an unstamped question, is retried, or `Unsettled` past the bound, and never pairs across occurrences;
  - a stable half-stamped legacy pair stays answered (the existing test, unchanged);
  - a completion between reads never yields an owed answer;
  - `ended` dates an answer only from the same coherent read.
- **Errors:**
  - an unreadable block directory, an entry error and a question whose metadata fails each produce blocking damage;
  - `NotFound` alone is absence;
  - every listed reader treats damage as blocking.
- With the lock free, behaviour is unchanged. The existing reuse and late-completion tests pass.
