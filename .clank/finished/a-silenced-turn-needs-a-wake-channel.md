# a-silenced-turn-needs-a-wake-channel

An agent can be silenced into a permanent sleep. TWO have been, by
two different branches of one invariant:

- **codex, this repo.** Reviewed `d249197` at 2026-08-26 11:59, then
  went silent for 22h with `44b7584` waiting on its intro review. Its
  in-hook poll hit the 23h55m deadline at ~11:54 on 27 Aug and
  returned `Silent { PollDeadline }`. No wait process survives it.
- **claude, `~/src/penlock-experiment`.** Detailed below.

Observed in
`~/src/penlock-experiment`: `claude` has owed a revision since
2026-08-27T14:05Z (`gate: changes_requested`, `waiting on: master`)
and has not woken in 9+ hours. It has no `clank wait` process and no
parked watcher; `codex` in the same repo has one, alive 9h32m. The
only trace is `.clank/agents/claude/attended`:

    {"task":"bsq30qg8z","desc":"tesseract strips",
     "at":"2026-08-27T14:05:33.890546Z"}

## Why (the missing half of the safety argument)

`attending-is-one-turn-not-a-duration` fixed a real bug — a marker
that claimed a DURATION had to stay true over time, needed reaping,
and the only reaper was the Stop hook it silenced. The fix made the
marker one-shot, and argued:

> it needs no liveness check: staleness requires surviving, and this
> cannot.

That is sound about the MARKER and silent about the AGENT. Silencing
a turn-end is only safe when something else will wake the agent, and
the marker's lifetime says nothing about whether anything will.

In the asyncrewake loop the hook's own park IS the wake channel.
`outcome_from_wait_output` returns `Silent { AttendingBackgroundTask }`
while holding actionable items; `asyncrewake_park` then drops the
lease and `emit_and_exit` ends the turn. Work discarded, park gone.

The replacement channel is Claude Code's task-completion wake — which
exists only while the task is RUNNING. So the branch is safe exactly
when the attended task is live, and the code never checks:

```rust
Ok(items) => {
    if let Some(rec) = attending {
        record_attended(agent_dir, &rec);
        return HookOutcome::Silent { why: AttendingBackgroundTask };
    }
```

`live_ids` is in scope on that very line — used four lines later for
the nudge, never for this decision. A test PINS the hang:
`the_task_list_no_longer_decides_anything` asserts silence even when
`live` is EMPTY, i.e. when the attended task is already gone and no
notification can ever arrive.

The asymmetry names the rule. `YieldArmed` also returns `Silent`, and
is safe — the disposition itself proves a live background task, so a
wake is coming. The attending branch returns `Silent` with no such
proof.

> A turn may be silenced only when a future wake is guaranteed.

## The ceiling is the terminator, and it must stop being one

`poll_deadline` is `started + HOOK_TIMEOUT_SECS - POLL_MARGIN_SECS`
= 23h55m (`stop_hook.rs:684`, `setup.rs:227`). A park waits for an
UNBOUNDED event — work arriving — so any finite ceiling eventually
fires on a repo that is merely quiet, and the agent never runs again.

`codex-idle-is-not-a-hook-failure` reached the right diagnosis:

> a hook whose lifetime is bounded by the runner's ceiling is being
> used to wait for an unbounded event

and then shipped a fix for the BANNER. Before it, the runner killed
the hook at 86400 and the user saw `Stop hook (failed)`. After it, the
poll expires 5 minutes earlier and the user sees nothing. The agent is
equally dead in both; only the cosmetics changed. Its conclusion —
"raising it buys nothing now that the poll self-expires" — followed
from believing the death had been fixed. It had not.

### Raise it to `i32::MAX`

`HOOK_TIMEOUT_SECS = i32::MAX as u64` (~68 years): effectively never.

- NOT dropping the field. Absent it, each runner applies its own
  default — claude documents 600s — which cuts a legitimate park short.
  The field is doing real work; it needs a bigger number, not deletion.
- `i32::MAX` over `u64::MAX` because a runner holding this in a signed
  32-bit field is the plausible limit. Neither runner documents a
  maximum, so this is a judgement about what a parser will accept, and
  it should be stated as one rather than dressed up as a discovered
  bound.
- Check `Instant + Duration::from_secs(i32::MAX - 300)` does not
  overflow or panic (`Instant::add` panics on overflow). Assert it,
  do not reason about it.

### Rollout order matters

The configs are USER-scope — `~/.codex/hooks.json`,
`~/.claude/settings.json` — one file per tool, not per repo. The live
codex file currently reads `"timeout": 86400`.

A binary whose `poll_deadline` derives from `i32::MAX` running under a
config that still says `86400` is the WORST combination: the poll
believes it has 68 years while the runner kills at 24h, which is the
failed-hook banner all over again. So run `clank setup` with the
freshly BUILT binary to rewrite the configs BEFORE `cargo install`,
and verify both files afterwards.

## Change: decide attendance at ENTRY, never after the park

(Reworked per codex on 81981f2 — the first draft had the check in the
right spirit and the wrong place.)

A membership test against `live_ids` at wait-return is NOT proof a
wake still exists. `live_ids` is snapshotted at hook entry
(`stop_hook.rs:95`, from stdin) and then carried through
`compute_wait_outcome`, which parks for an unbounded time. When work
finally arrives the snapshot may be hours old: the attended task can
have completed long since, its completion wake already spent, and
suppressing on that stale evidence recreates the permanent sleep this
plan exists to remove.

Checking harder at the end cannot fix that. Deciding EARLIER can:

    at hook entry, with live_ids fresh by construction:
      marker && attended-task live  -> consume, return Silent.
                                       The completion wake is still
                                       OUTSTANDING; it is the channel.
      marker && not live            -> consume, fall through and PARK.
      no marker                     -> park, exactly as today.

The marker is consumed on every path — it stays one-shot, which
`attending-is-one-turn-not-a-duration` got right.

Every path now ends holding a channel: an outstanding completion wake,
or a park. Not as a checked property but as a structural one — the
branch that abandons the park is the same branch that has just
confirmed a wake is pending, and it is the only one that can reach
`Silent`. The unbounded gap between evidence and decision is gone
because the decision moved to where the evidence is fresh.

`consume_attending` already runs at entry (`stop_hook.rs:122`). This
moves the DECISION to join it, and deletes the attending branch from
`outcome_from_wait_output`.

### As built

The decision is `attendance_silence(attending, live_ids, agent_dir)
-> Option<HookOutcome>`, called from the driving arm before
`match disposition` so it covers the asyncrewake park and the in-hook
wait alike. `Some` = silenced, wake handed to the task's completion;
`None` = park.

It is a named function rather than an inline `if` because the
decision is the whole point of the plan and needs to be testable
without standing up a hook input, a repo, a roster and a config.

`outcome_from_wait_output` loses its `attending` parameter, and with
it `agent_dir` — whose only remaining use was writing the record from
that branch. The wait-return path no longer has the means to suppress,
not merely the instruction not to.

### Suppression requires PROOF of liveness, not the absence of doubt

(Reworked again per codex on 5624f97. The previous draft named this
hole and then left it open — while the penlock marker that motivated
the whole plan had no pid, so the plan would not have fixed its own
reported case.)

`live_ids` is not trustworthy even when fresh.
`a-dead-pid-voids-a-falsely-live-marker` recorded direct observation
of claude reporting a completed task as live: task `bctmopf2c`, a
`cargo install` that exited 0, pid 98707 gone from the process table,
still announced live by the very next hook.

This subsystem is biased against missed wakes — a spurious wake is
noise, a missed one is an agent that never runs again. So the rule is
not "suppress unless we can disprove liveness" but its opposite:

> Attendance may suppress ONLY on proof that the attended process is
> still running. Absent proof, consume the marker and PARK.

Proof is a pid whose `ProcToken` still holds it:

    provably_live = task ∈ live_ids
                 && pid is Some(p) && pid_is_alive(p)
                 && token is Some(t) && t.still_holds(p)

`live_ids` alone is not proof, because it has lied. A pid without a
token is not proof, because pids are reused. Everything else —
pid-less, token-less, legacy, unparseable — is consumed and falls
through to the park. Noise, never silence.

### What that costs, stated plainly

**A `clank attending` without `--pid` no longer suppresses anything.**
It writes a record, the record is consumed, and the turn proceeds to
park. That is a behaviour change for every caller today, including
every invocation in this session, none of which passed a pid.

That is the correct failure direction, and it makes the gap
load-bearing rather than theoretical: attendance is now only useful
to a caller that can supply a verified pid, and a harness background
task cannot — it hands back a task id and an output file, never a
pid. So `clank attending --run -- <cmd>`, where clank spawns the
process and therefore knows its pid, stops being a nice-to-have and
becomes the only way attendance works for the common case. It is out
of scope here and should be queued next.

Until it exists, `clank attending` must SAY so when the marker it
just wrote cannot suppress — a caller silently getting a no-op is how
this class of bug stays invisible. The nudge now says it too: both
flags are described as load-bearing, `--pid` as the one that lets
clank prove the task is running.

**Demonstrated while implementing this plan.** Every background task
run during the work — six of them — produced a marker with no pid,
because a harness task hands back an id and an output file and
nothing else. Each turn-end then re-nudged about the live task, which
is precisely the loop attendance was built to stop. That is not a
hypothetical cost: it is what every agent will experience the moment
[[attending-requires-a-knowable-end]] starts refusing unprovable
markers, so `--run` must land WITH that refusal, not after it.

## This also closes the display lie

`record_attended` is only ever called from the suppress branch, and
suppression now requires proof. So an `attended` record can only
describe an attendance that proved itself — which means the row
`clank status --tui` renders as an ongoing wait always has a pid and
token behind it, and can always be resolved.

The reported symptom was the opposite: a pid-less record rendered as
`attending: claude → bk3qnvo12 · 2m` and would have counted upward
forever, because nothing in clank could ever learn the task had
ended. That record can no longer be created.

This narrows [[attending-requires-a-knowable-end]] rather than
duplicating it. What remains there: refusing unverifiable markers at
WRITE time (so the caller is told, instead of silently getting a
marker that suppresses nothing), rendering legacy records already on
disk, and `clank attending --run -- <cmd>`.

## The docs still describe the abandoned model

Both survived the one-shot rewrite and now contradict the code. They
must say what the liveness check is FOR, not resurrect the old story:

- `attending.rs` module doc — "the Stop hook validates the recorded
  id against the tool's live task list and discards it the moment
  that task is gone, so the silence lasts exactly as long as the
  work". False today: the silence lasts one turn-end.
- `hook_io.rs::live_task_ids` — "The authority for whether an
  `attending` marker still means anything ... validated here, never a
  claim the agent has to remember to retract."

## Every `Silent` branch, classified

The invariant is one line — *a turn may be silenced only when a future
wake is guaranteed* — so every branch must say which side it is on:

| reason | wake guaranteed by | verdict |
|---|---|---|
| `YieldArmed` | the armed task's completion re-fires Stop | safe |
| `WaiterAlreadyParked` | the other waiter | safe |
| `StaleGeneration` | the newer incarnation's own park | safe |
| `AutoOff` | nothing — and that is the human's instruction | safe by intent |
| `PollDeadline` | **nothing** | **unsafe** |
| `NoWork` | **nothing** | **unsafe** |
| `AttendingBackgroundTask` | the task's completion, IF it is live | **unsafe as written** |
| `BusyOwnWork` | the live background task's completion | safe* |
| `PeekFailed` | the live background task's completion | safe* |

Raising the ceiling makes `PollDeadline` effectively unreachable
rather than safe. That is the pragmatic fix and it is worth taking,
but it does not discharge the invariant — a park can still end for
other reasons. Whether expiry should RE-ARM instead of going silent
is the remaining question, and it has a real constraint: the code
rejects a nudge relay because "a nudge relay would loop an opencode
session forever" (codex `8000d6e`). That hazard is about a tool which
re-idles instantly, not about a 24h re-park, so if re-arm is adopted
it must be per-loop-policy rather than blanket.

### *The asterisk: three branches trust a signal we just stopped trusting

`BusyOwnWork` and `PeekFailed` are reachable only under
`BgDisposition::NeedsWorkCheck`, and `YieldArmed` only under its own
disposition. All three are returned because `background_tasks` says a
process is live, and all three hand the wake to that process's
completion. So they are safe exactly as far as that field is truthful.

It is not reliably truthful. `a-dead-pid-voids-a-falsely-live-marker`
recorded claude naming already-finished tasks as live, which is why
attendance in this plan stopped accepting `live_ids` as proof. The
same field decides these three branches, and
`has_non_wait_background_work` (`hook_io.rs:265`) is weaker still — it
does not even apply the `status != "completed"` filter that
`live_task_ids` does.

The consequence is the same permanent sleep by a third route: a
falsely-live task yields the turn, no completion wake ever comes, and
nothing is parked. Not observed in the wild yet, unlike the two paths
this plan fixes.

Not fixed here, deliberately. Attendance had a per-record identity to
verify — a pid and a token — and these branches have nothing
equivalent: the disposition is computed before repo and identity
resolution, from the turn alone, and there is no marker to carry a
pid. Making it provable means changing what the disposition is allowed
to conclude, which is core routing for every turn rather than the
attendance path. That belongs in its own plan with its own review, not
smuggled into this one's last commit.

So this plan closes the two paths it set out to close and names the
third precisely rather than leaving the table with two shrugs in it.

## Tests

- **The temporal regression (codex on 81981f2).** Task live when the
  hook arms, COMPLETED before the wait emits, then actionable work
  arrives: the work must be DELIVERED. Under the entry-time design
  this case never parks holding a marker at all, so it passes by
  construction — which is the point, but it must still be pinned, and
  it must fail against a wait-return check.
- Attended task not live at entry + actionable work → parks, work
  DELIVERED.
- Attended task live at entry → Silent, and NO park is taken.
- The marker is consumed on every path.
- A marker whose pid is dead is void even while `live_ids` still
  claims the task (the falsely-live payload), and so is one whose
  `ProcToken` no longer holds the pid.
- **False-live + no pid → DELIVERED, not silent** (codex on 5624f97).
  `live_ids` claims the task, the marker carries no pid, so liveness
  is unproven: park and deliver. This is the penlock shape exactly,
  and it is the case the previous draft would have got wrong.
- Same with a pid but no token, and with a token that no longer holds
  its pid.
- `clank attending` with no `--pid` reports that the marker cannot
  suppress.
- An unprovable marker leaves NO `attended` record, so there is
  nothing for `clank status` to render as an ageing live wait.
- A provable one records the decision with its pid, and the NEXT
  entry does not suppress — the marker is still one-shot.
- Legacy marker shapes (no pid; pid but no token; a dropped `since`
  field) still parse and are still consumed, and none of them
  suppresses.
- `the_task_list_no_longer_decides_anything` inverts: the task list
  decides again. Replace it, and say in the name what it now pins —
  the list is the wake-channel evidence, not a staleness probe.
- Empty `live_ids` with no marker is unchanged (ordinary delivery).
- `poll_deadline` at the raised ceiling does not overflow or panic,
  and still sits exactly `POLL_MARGIN_SECS` under it (the existing
  relationship test must keep passing at the new value).
- The hook config clank writes carries the raised timeout, and the
  existing "timeout refreshed from the binary's constant" test still
  holds.

## Out of scope

- The one-shot marker itself: it stays one-shot.
- A status indicator for "silenced with work and no watcher". The
  mechanism is what is broken; a light showing it fail is not the
  fix. Worth revisiting only if a wake can still be missed after
  this (a killed task, a resumed session) — a separate plan, and it
  would need a real backstop, not a display.
- Unsticking the live penlock-experiment session, which is a
  different repo and the human's call.
