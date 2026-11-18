# recovery-looks-when-something-moves

Invariants: I10.

## Why

Recovery re-looks every `RETRY_LOOK` (60 s) at an episode, with no bound,
through `recovery_due`: `cli/src/cli/monitor.rs start deadline#0` in
`crates/cli/tests/it/timed_wait_boundary.rs`, a residual poll. The
episode logic lives in `crates/cli/src/cli/recovery.rs`.

`RETRY_LOOK` stands for three different waits behind one
`Episode::next_at`:

- **the gate.** Only `Driver::due` asks it, so a decision made on a fold
  (gate `None`) re-arms 60 s to have the gate asked later: `decide_as`'s
  `(_, State::Waiting, None)` arm and `due`'s `ctx.gate != Some(Gate::Go)`
  arm.
- **the record.** `due`'s `!stands(e, snap)` arm waits for the record to
  show the failed turn as latest again.
- **an unwatchable turn.** `watch`'s fallback arm, and `decision`'s "always
  has a next look" safety net.

Replacing the 60 s with "ask at once, then wait for the next event" was
tried in no-polls-but-github and makes a hot loop: `next_at = None` means
both "wait for a fold" (after a hold, or the record not standing) and "no
deadline yet", and the `(_, Waiting, None)` arm turns the second into "ask
now", so `!stands` → `None` → ask now → `!stands` → … with no event between.

## Build

1. Type what an episode waits on, instead of one `next_at`: `Gate` (asked
   on the next loop pass, then `Event`), `Event` (a fold, a record change,
   a binding or presentation change — all already wake the loop), or
   `Deadline(at)` for scheduled domain work only (idle settles, stall warn
   and nudge, fallback checks, answer and confirm windows).
2. `Driver::next_due` reads only `Deadline`s and `Gate`s; `Event` waits
   produce none.
3. An unwatchable running turn: decide whether it escalates, or waits on
   the binding or session changing.
4. The `held_until = now + RETRY_LOOK` arms in `recovery.rs` are this
   plan's too (failed-work-waits-for-a-cause finished without them): an
   unreadable `issued`, a recovery that could not be recorded, a
   `upgrade::Stale` permit, and the "always has a next look" safety net.
   Follow the model that plan settled: a `Stale` permit waits on the
   installation moving (`Authority::subscribe`, failure-aware); an I/O
   failure on the agent's state retries on that file changing, with one
   fixed report deadline per episode, after which it is reported (in
   `clank status` / the TUI) and not retried until something re-triggers
   it; a watcher that cannot arm ends the wait with why. The safety net
   is replaced by the typed wait, not a timer.
5. Tests: no lost wake (a fold between the decision and the wait still
   reaches the episode), progress after the hold clears, and no periodic
   wake (`next_due` is `None` while nothing moves), and for each failure
   hold, that unrelated activity across its deadline cannot postpone the
   report. Mutation-check each by putting `RETRY_LOOK` back.
6. Reclassify the gate site; `RETRY_LOOK` is deleted; update I10.
