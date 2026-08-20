# attending-is-one-turn-not-a-duration

## Why

`attending` is a turn-scoped INTENT stored as a duration-scoped FACT,
and every failure it has had is that mismatch.

What the agent means when it runs `clank attending X` is: *I am
yielding now, and I am yielding because I am waiting on X — do not
nudge me for THIS yield.* What the marker says instead is: *X is
running; silence everything until it stops.* The second claim has to
stay true over time, so it needs liveness, so it needs reaping, so it
needs a reaper — and the reaper is the very Stop hook the marker
silences.

Observed: reviewer `ruthless` in the frostsnap worktree
`psbt-pure-transform` held a marker for task `bnjigi8tn` for 3h25m
while owing a review on HEAD `6a82c108` that `codex` had already
delivered. Auto-mode was on. It was not waiting on anything.

The deadlock is structural. A silenced Stop returns before the
arm-the-wait hint, so the agent goes dormant WITHOUT arming the
background `clank wait` that would bring it back. Silence removes the
mechanism that ends the silence.

Reaping also has four independent ways to never happen, because
`attending_live_task` is reached only when a turn ends AND auto is on
AND `clank wait` returned NON-EMPTY items:

- the agent never ends another turn (dormant — see above);
- `items.is_empty()` short-circuits to `Silent{NoWork}` first;
- `AutoMode::Off` returns before the marker is read at all;
- the task is still reported live, including FALSELY — confirmed
  separately: `bctmopf2c` had exited and its pid was gone while the
  hook still listed it.

## The model

**A marker is consumed by the Stop hook that reads it.** It silences
at most one yield, and it cannot outlive the turn that wrote it.

That single change deletes the whole category. A one-shot marker
cannot go stale, because staleness requires surviving. Nothing needs
to check whether the work is still running — not the harness's task
list, not the pid — because the marker is not making a claim about the
future.

## Delete on EVERY Stop, not only the ones that read it

The four escape routes above are all "the hook ran but never reached
the marker". So deletion must happen UNCONDITIONALLY and EARLY: read
the marker once at the top of the Stop handler, delete it immediately,
and carry the value forward to decide silence.

Read-then-delete, not delete-if-used. A marker that survives a Stop
for any reason is the bug returning.

## What becomes redundant

`attending_live_task`'s liveness conjunction — the task-list check and
the pid veto — exists solely to bound a lifetime this model removes. If
a marker cannot outlive its turn, the veto can never fire.

Evaluate it and DELETE it if unreachable rather than leaving it as
reassurance. A dead guard for an impossible state is a false model, and
this plan's whole point is that false models are what went wrong here.
The pid itself stays: `clank status` uses it for display, which is a
separate and still-true purpose.

## The cost, stated plainly

Each yield needs its own `clank attending`. An agent woken mid-wait for
something else must re-attend when it yields again or it gets nudged.

This does NOT reintroduce the loop the previous work removed. The loop
was: wake → nothing new to do → yield → wake. Here a nudged agent that
re-attends is silent again immediately, and the hint fires exactly in
this situation (live task, no marker), so the agent is told every time.
The bound is "the agent obeys the hint", not "the agent remembers".

## Visibility: record the DECISION, not the claim

Settled in intro review, and not optional: the display does not go
dark. It was built one plan ago for a live failure — attending broken
and invisible — and that need is unchanged.

So the marker stays one-shot, and the Stop hook writes what it DECIDED
at the moment it consumes one:

    .clank/agents/<label>/attended
    {"task": "bnjigi8tn", "pid": 54321, "at": "2026-08-20T10:13:13Z"}

The pid rides across from the marker at consumption time, which is the
only moment both are in hand.

**The hook writes this record on the path where attending caused the
silence, and DELETES it on every other outcome.** That is what keeps it
honest: the record always describes the LAST Stop decision, so a
decision superseded by a wake is gone at that wake, not left to age.

**Status renders it with the pid-liveness display the previous plan
already built** — `attending: claude → 54321 (bnjigi8tn) · 4m`, or
`· stale, ended` when the pid is gone, or the bare task with no
liveness claim when there is no pid. `Attending::summary` is that
renderer; reuse it rather than growing a second one.

What changes is the MEANING, and it is strictly more honest: this is
history about a decision plus a live OS check, never a claim that
survived. A record cannot wedge anything, because nothing reads it for
suppression — which is precisely why it is safe to let it live longer
than the marker it came from.

## Required tests

- A marker silences ONE Stop; a second Stop with the same conditions
  wakes, proving it was consumed.
- The marker is deleted when `AutoMode::Off` returns early.
- The marker is deleted when `clank wait` returns no items.
- The marker is deleted when the task is STILL reported live — the
  falsely-live case can no longer wedge anything.
- A marker written before this change (any shape, pid or not) is
  consumed identically; none can outlive its turn.
- Consuming a marker WRITES the decision record, carrying the pid.
- Any non-attending outcome DELETES the record, so a decision cannot
  outlive the wake that superseded it.
- Status renders a record with a live pid as attending, one with a
  dead pid as ended, and one with no pid with no liveness claim —
  through the existing renderer, not a second one.
- No test spawns zellij, an agent binary, or the stop hook, and none
  shells out to `ps`/`kill`.

## Out of scope

- `clank attending --run -- <cmd>`, the wrapper that would let clank
  learn the pid by spawning the work itself. Still worth doing for
  `status` display and eager cleanup, but it is a separate mechanism
  and this plan removes the urgency behind it.
