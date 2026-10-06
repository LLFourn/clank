# a-wake-the-agent-keeps-ignoring-escalates
# A wake the agent keeps ignoring escalates

## Why

An agent can loop on a wake without acting on it. In this repo, the master
answered eight consecutive `continue an-interrupting-source-chooses-its-hint`
wakes with "still paused". Each turn ended at once, the Stop hook fired the
same item again, and nothing changed until the user interrupted.

The cause was an interrupted tool call. The harness said "STOP … wait for
the user". The agent read that as a hold, with nothing in the nudge to
resolve the conflict and no sanctioned way to express it. Every wake looked
identical, so the agent never saw that it was repeating itself.

Clank sees the whole loop: the same items delivered to the same agent again
and again, each turn ending without changing them. That is the place to
notice it and push back.

## What to build

Repeated delivery is evidence that the same work is still owed. It is not
proof that the agent did nothing: edits, tests and investigation can all
happen before the commit that moves a gate. It also grants no authority to
override a pause or a rejected action. The diagnostic states the fact and
the valid ways forward, and nothing more.

- **One owner, one count.** The Stop hook is the only place that counts. A
  delivery is one continuation the hook actually emitted to the agent.
  - The count is kept per agent and session, in that agent's state.
  - Monitor frames, reconnects and retries never count.
  - `events list`, `events show`, `events read` and `wait --peek` never
    count.
  - A duplicate hook invocation for the same turn, or a continuation that
    failed to emit, never counts.
- **Typed identity, canonical order.** "The same items" compares the set
  of the delivered items' typed identities, sorted, never labels or
  rendered text.
  - A workflow item is identified by its kind, plan and sha.
  - An event is identified by its source, seq and logical identity.
  - A reordered but identical set is the same set.
- **What resets the count.** The count resets, and the status line clears,
  when any of these change:
  - the item set (a commit that moves a gate, an item handled, acked or
    added);
  - the bound session;
  - a block raised or answered.

  These are all state the monitor's existing folds and signals already
  carry, so clearing needs no polling, timers or watchers.
- **Human turns reset it; clank's own do not.** A new turn also resets the
  count, but only when it is a human-origin turn whose provenance is known.
  - An automatic hook continuation, or a clank interruption hint, also
    appears as a user turn. Neither resets it, or the loop being detected
    would hide itself.
  - Provenance comes from data clank already has: the wake receipt and
    interruption records, and the turn's text matching a continuation clank
    emitted.
  - Where provenance is unknown, the turn does not reset the count. That
    can last across several deliveries, until the items, the session or a
    block change.
- **The escalated message.** From the fourth counted delivery of an
  unchanged set (a constant, defaulting to 4), the Stop hook adds a
  paragraph to its message:
  - **It states the fact:** "These items have been delivered to you N
    times unchanged."
  - **It says what ending a turn does not do:** ending turns does not
    discharge pending Clank work. The items stay owed and will be
    delivered again.
  - **It defers to real instructions:** follow the user's latest
    instruction, any explicit pause, and the actual reason given for any
    rejected action.
  - **It gives the valid ways forward:**
    - if the work can proceed, act on it;
    - if it genuinely depends on the user, describe the dependency with
      `clank block create <name> -m "<question>"`.
  - **It describes a block accurately:** a block asks a person to resolve
    a real dependency and pauses review work. It is not a mute button:
    messages and events are still delivered during a block, and their
    required replies still hold.

  Below the threshold, the message is unchanged.
- **Visible to the person.** While the count is at or past the threshold,
  `clank status` shows the measurable fact for the agent, e.g. "claude:
  same items delivered 6 times". It never claims the agent is inactive.
- **No delivery is dropped.** Escalation changes only the wording; every
  owed item is still delivered.

## Acceptance

- **Threshold:** three counted unchanged deliveries produce today's
  message. The fourth adds the escalation paragraph with the count.
- **Reset:** a commit that moves the gate, an item handled or added, a
  change of session, or a block raised or answered resets the count. The
  status line clears without waiting for another delivery.
- **Identity:**
  - A reordered but identical item set counts as the same set.
  - Monitor reconnects, `events list`, `events show`, `events read`, `wait
    --peek`, a duplicate hook invocation and a failed emission do not
    advance the count.
- **Human vs clank turns:** a human-origin turn resets the count. Clank's
  own hook continuations and interruption hints do not, and an unknown
  provenance does not.
- **Blocks:** `clank block create <name> -m "…"`, the command the message
  names, runs as written.
  - Raising a block stops review-work wakes.
  - An unacked message still wakes a blocked agent, with its required reply
    intact.
- **Legitimate ongoing work:** work that has not yet committed (edits and
  tests between deliveries) may still reach the threshold. The message and
  status report only the repeated delivery, and never claim inactivity or
  override an instruction.
- **Restart:** a restarted monitor or hook can restart the count from zero,
  and never inflates it.
- **Cost:** no new polling, timers or watchers.

## As built

- **Counted as it is sent.** `cli::wake_repeat::send_counted` is one
  decision under the agent's `repeat.lock`.
  - It reads the committed count and works out this delivery's count.
  - It hands that count to the emission, which renders, writes and
    flushes the continuation.
  - It publishes the new count only if the continuation was presented and
    the binding still holds.
  - Capturing a delivery publishes nothing, and a failed or abandoned
    send leaves the count as it was. Counts follow the order sends
    succeed, and the count a continuation states is the count committed.
  - Receipt callbacks run after, outside the lock.
- **Three outcomes for a delivery, decided against the committed count.**
  - *Counted:* it advances the count.
  - *A duplicate of the same item set:* it states that set's count and
    stores nothing.
  - *Superseded:* placed at or before a counted Stop but for other items.
    It is sent uncounted, stating no count, and never another set's.
  - Two captures sent out of order count once. The earlier-placed one,
    arriving second, is overtaken. This is conservative by design.
  - The binding is re-checked after the lock is acquired. A sender rebound
    while waiting for the lock sends the agent nothing.
- **Placed by the agent's own record, read at hook entry.** Where the Stop
  sits is read before the wait, and a high-water mark rejects any Stop
  placed at or before one already counted. That is constant state, and it
  catches replays however late they come.
  - **Claude** is placed at its newest turn's own prompt record. A tool
    result is never a turn's start, so a turn whose prompt lies beyond the
    window is unknown.
  - **Codex** is placed at the newest event of its own `turn_id`: that
    turn's start, end or abort, or a message after its start. Bookkeeping
    appends do not move it, a replayed Stop of an earlier turn stays where
    that turn ended, and a turn not in the window is unknown.
  - **OpenCode** keeps no such record, so each continuation counts.
  - Placement assumes an append-only record. A replaced or compacted
    record can make a new Stop look old until the items change.
- **Monitor.** The fold forgets a count without ever waiting for the lock;
  it skips a held lock and looks again at the next fold.
  - The binding is read under the lock, as it is then.
  - Owed items count as evidence only for a count recorded before the fold
    began reading.
- **Blocks are items.** A `Blocked` or `Unblocked` item is part of the
  delivered set, so raising or answering a block resets the count, and a
  standing block keeps counting.
- **Provenance.** Only Claude records origin. A turn is a person's only
  when its `origin.kind` is `human`.
- **Limitations.**
  - Grok's handoff runs through its own wait and is not counted.
  - If the hook dies after a successful send but before publishing, the
    count undercounts by one. It never overcounts.
  - The hook's wait closure calls `ReceiptCapture::delivering`. That one
    call has no direct test, since it sits inside the full hook entry
    point; the constructor itself is tested.
