# the-monitor-loop-never-waits-on-an-agent-lock

Invariants: I9.

## Why

`outage_served` (`agent_store.rs:759`, called at `monitor.rs:8258`) takes a blocking `flock` on an agent's `binding.lock` on the monitor's serve path. Any process holding that lock stalls serving for every agent in the repo.

Two designs were rejected in review:

- **Moving acquisitions onto detached blocking tasks** (be14394): a blocked flock cannot be aborted, shutdown waits for it, and work piles up behind one held lock.
- **Relying on short critical sections** (c9b36f0): a holder can be descheduled or stalled on storage while holding the lock, so a short section is not a bound on acquisition time. And at least one holder rightly keeps its lock across an external side effect: `user_command/dispatch.rs` holds the binding across `type_user_command(..).await`, so the command is typed only into the session that is bound. Moving that typing outside the lock could send a person's command to the wrong session.

So the monitor must not block, and it must not take holders' locks away from them.

## Build

1. **A non-blocking acquisition.** Add `agent_store::try_exclusive_lock(path) -> io::Result<Option<File>>` using `LOCK_EX | LOCK_NB`, and `BindingGuard::try_acquire`. `None` means the lock is contended, which is never read as an empty state.
2. **`outage_served` on the serve path uses `try_acquire`.** When the lock is contended it clears nothing and serving goes ahead unchanged. The notice is best effort: it stays told, and the next serve for that session clears it. Retire and shutdown are unaffected, because nothing blocking was started.
3. **Sweep.** List every lock the serve loop reaches through its helpers. For each, record whether an agent or hook process can hold it, and what the loop needs from it: a result it replies with, an ordering, or best effort.
   - Monitor-only locks are fine as they are.
   - Each lock an agent process can hold that the loop needs a result from gets a queued follow-up plan, `the-serve-loop-survives-a-held-<lock>`, with its own contention outcome. Examples: question state for `question_control` and `observe_questions`, and `blocks.lock` for `block::ended`.
   - Holders that rightly keep a lock across a side effect, like `user_command` typing, keep it. Their contention is the monitor's problem to avoid, not theirs to give up.
4. **Registry:** I9 stays `target`. Its `Today` names what the sweep found still blocking, and its `Plans` names the follow-ups. Its sentence is unchanged.

## Acceptance

- With another thread holding the agent's binding lock for the whole test, the monitor serves that agent's report and another agent's, leaves the outage notice in place, and then retires. All of this happens while the lock is still held, before the test releases it. Synchronization is deterministic, with a barrier or channel and no sleeps.
- With the lock free, the notice is cleared as before.
- The sweep's classification is in the commit message, and every follow-up it names is queued.
