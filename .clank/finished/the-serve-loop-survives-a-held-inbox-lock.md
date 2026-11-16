# the-serve-loop-survives-a-held-inbox-lock

Invariants: I9.

## Why

Serving still blocks on `inbox/.lock` and `events/messages.writer.lock`: `gather` → `items_for` → `message_items` → `inbox::pending_with` → `inbox::migrate`. `clank send`, `clank events` and `clank wait` can hold those locks. The serve loop takes them only through `inbox::migrate`, which moves messages left in the old file inbox (`.clank/agents/<label>/inbox/`, replaced on 2026-09-29 by `a-message-is-an-event-like-any-other`) into the event log. `inbox::migrate_all` does the same from the TUI timeline (`github/timeline.rs:630`), and `cli/events.rs:73` calls the migration too. `messages.writer.lock` is not migration-only: ordinary `log_message_with_key` and `message_transaction` still serialize writers with it, off the loop, and keep it.

One more blocking path remains on the loop, outside the inbox. `timer_source::accept` and `EventLog::recurring_writer` take the ingest lease with `IngestLease::try_acquire`. That delegates to `clank_user_config::lease::take`, which sleeps the calling thread for up to `PATIENCE` (250 ms) while the lock is held, and several due timers add up. A bounded sleep is still a wait, so I9 cannot hold while it stays.

The user's rule is that clank keeps no migration code; a format change is a breaking change (decided for blocks in `blocks-have-no-migration`). So the right fix is not a non-blocking migration but no migration. That also removes the last blocking lock I9's `Today` names. No `.clank/agents/*/inbox` directory exists in any repo on this machine.

## Build

1. Delete `inbox::migrate`, `inbox::migrate_all`, `migrated_action_key` and every call: `pending_with`, `timeline.rs:630` and `cli/events.rs:73`. `inbox/.lock` goes if nothing else takes it. `messages.writer.lock` stays for ordinary message writers off the loop. Already-migrated WAL messages stay readable.
1a. **The ingest lease on the loop is taken without waiting.** `timer_source::accept` and `recurring_writer` use the existing non-blocking `try_acquire_now` (or `lease::take_now`). A held lease is contention like a held sidecar:
   - the timer stays on the `TimerSet`'s retrying list at the right `Stage`;
   - the contention retry from `the-serve-loop-survives-a-held-event-log` re-runs it;
   - it is never dropped or emitted twice.

   Off-loop producers keep the patient `take`.
2. `log::MIGRATED_MESSAGE_PREFIX`:
   - If its only role is pinning migrated idents in the dedup horizon so a re-run of the migration cannot mint a duplicate, delete it and its matcher. With no migration, nothing replays those files.
   - If any reader still depends on recognising existing migrated records, keep only that reading, and say so.
3. A leftover old file inbox is neither read nor touched.
4. CHANGELOG `Unreleased` → Breaking: messages left in an old per-agent file inbox (`.clank/agents/<label>/inbox/`) are no longer moved into the event log. Read or delete them before upgrading; the directory can be deleted.
5. Registry:
   - I9 becomes `holds`. Name the held-lock tests: binding, blocks, event log, and this plan's held ingest lease and held legacy and writer locks.
   - I9's `Scope` says what the sweeps covered, and that a holder may keep a lock across a side effect (signed user-command delivery) because the loop never waits on it.
   - Remove `Plans` and `Today`.
6. Delete the migration's own tests. Keep or add one test that a leftover old inbox is ignored on the serve path, and that serving with `messages.writer.lock` held by another thread completes before release, if that lock remains.

## Acceptance

- No production code reads `.clank/agents/<label>/inbox/`.
- With a leftover old inbox and its `inbox/.lock` and `messages.writer.lock` held by another thread, the monitor serves and retires before release, and the old inbox's files are byte-for-byte unchanged.
- With the ingest lease held by another thread for the whole test, a due one-shot timer and a recurring timer's acceptance stay pending without the loop sleeping. A report owed nothing is served and the monitor retires before release. After release, the contention retry accepts each exactly once.
- Ordinary message sends still serialize on `messages.writer.lock`, and messages migrated earlier still read.
- I9 holds, and the registry gate passes.
