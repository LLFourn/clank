# attending-is-not-deaf
# Attending is not deaf

## Why

A controller agent in fs-war-room attended a subagent ("Open esp-hal
HMAC PR", 45m expect). Two `story`/`asked` events landed in its inbox,
and it was not woken for either. The user had to ask why.

The cause is in `turn_policy::SessionState::turn_end`. When attendance
qualifies, it returns `Verdict::Silent(SilentWhy::Attending)` before
`deliver_or_park` runs. A silent verdict ends the Claude stop hook with
no parked wait (`stop_hook.rs`, the `HookOutcome::Silent` arm). So while
an agent attends anything:

- work already owed at that turn end is not delivered, and
- nothing is parked, so work that arrives later (events, messages,
  review items) cannot wake it.

The agent goes deaf until the subagent finishes or the check-in
deadline fires, which can be up to the full `--expect`.

Attendance exists to stop clank re-presenting work the agent has
already taken on and is waiting out, which was the old
nudge-every-minute loop. It was never meant to withhold new work.

## What changes

### Quiet only what this session has received

"Owed" and "received" are different facts. Attendance may quiet only
work the agent has provably received, never work that is merely owed.

- `SessionState` keeps a `received` set: the identities of owed items
  whose presentation got a `Delivered` result on this binding. It is
  added to only in `settle(.., Delivered)`, from the presentation's own
  identities. Selecting or rendering an item adds nothing, and neither
  does ending the next turn. EmitFailed, EOF, an abandoned presentation
  and a stale binding add nothing either, so those items stay
  deliverable. This reuses the existing presentation custody, with no
  second acknowledgement.
- The set is scoped to the binding: `rebind` replaces it with the rest
  of the state. It is pruned on every step to identities still owed,
  so it holds only what is still needed to quiet. It persists in
  `turn.json`. Saved state without the field loads as empty, which is
  the conservative default: the agent may be shown work again, but is
  never deaf.
- While attendance qualifies, the deliverable work is `owed − received`.
  One function computes it, `deliverable(facts)`, and every path that
  can present goes through it: `turn_end`, `parked` re-evaluation, and
  session-start catch-up where attendance applies. If nothing is
  deliverable the verdict is `Park`, not `Silent`, so a waiter stays
  armed and later work wakes the agent. Repeat and escalation compare
  the deliverable identities, so quieted items never tick a repeat.
- The filter applies only while attendance qualifies. It stops
  applying, and everything owed is deliverable again, when attendance
  is cleared or replaced, its task ends, it stops qualifying, or its
  original deadline passes. The deadline and check-in lifecycle are
  unchanged.
- This adds no polling and no new worker. A parked waiter is woken by
  the monitor's existing watcher, socket and timer events.

### Provider scope, as the code stands

- **Claude:** the only route where attendance can silence today
  (`completion_wake` qualifies only Claude). This plan changes it from
  Silent to Park-plus-deliverable.
- **Codex and OpenCode:** attendance never silences them today. That
  is unchanged.
- **Grok:** `handoff_verdict` is stateless (ArmWait/WaitArmed) and is
  dispatched before `SessionState`. Its one background wait already
  carries every owed item. That is unchanged, and a test pins that an
  attending Grok's background wait still delivers a new event.

## Tests

All of this is one implementation commit after the intro is approved.

- **Unseen item before the first qualifying stop:** attend T, then an
  event arrives, then the first qualifying turn end. The event is
  delivered. Mutant: seeding the quiet set from everything owed.
- **Received work is quiet:** an item delivered (Delivered result)
  before or during attendance is not re-presented at a later turn end.
  The verdict is Park.
- **Failed or lost delivery:** a new item presented while attending
  that gets EmitFailed, or no result, stays deliverable at the next
  turn end. Mutant: adding to `received` at presentation instead of at
  Delivered.
- **Parked re-evaluation:** repeated `parked` steps with only received
  work owed keep parking, never re-presenting. Mutant: `parked` going
  straight to `deliver_or_park`.
- **Lifecycle:** clear, replacement attendance, task ended, and
  deadline passed each make received-but-owed work deliverable again.
  Rebind empties the set. A restart round-trips it through
  `turn.json`, and an old `turn.json` without it loads empty.
- **Repeats:** quieted items do not increment the repeat count.
- **Wiring, Claude:** drive the real Claude stop-hook path through the
  monitor. An attending agent's turn end parks a wait, and an event
  appended afterwards wakes it. Mutant: the current early `Silent`
  return.
- **Wiring, Grok:** an attending Grok's background wait delivers a new
  event.
- **Codex/OpenCode:** attendance does not change their verdicts
  (unchanged behaviour, pinned).

## Out of scope

- Changing how attendance is resolved or how check-ins are typed.
