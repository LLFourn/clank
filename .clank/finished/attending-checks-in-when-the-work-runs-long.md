# attending-checks-in-when-the-work-runs-long

> Attending should have some default timeout — this timeout is not a
> failure case and it doesn't cancel the process, it just wakes the
> agent with the hint that the thing has been taking longer than
> expected and they might want to look into it. The agent may pass an
> upper bound (default 5m). — lloyd

## The hole

An attended turn-end hands its wake channel to ONE thing: the
harness's task-completion notification. That is by design
([[a-silenced-turn-needs-a-wake-channel]]): the marker may only silence
when a completion wake is provably outstanding.

But a completion wake fires when the work COMPLETES. A build that
hangs, a test that deadlocks, a `cargo install` stuck behind a lock —
none of those complete, so the one channel never fires and the agent
sleeps until something unrelated wakes it. The proof rule closed the
"marker outlives the work" hole; it did nothing for "the work outlives
everyone's patience". Today that is a human noticing an hourglass in
the TUI that has read `2h` for a while.

## The rule

> Every attendance carries an expectation of how long the work should
> take. If the work is still running past it, the agent is woken ONCE
> with a check-in. Nothing is cancelled, nothing has failed, and the
> completion wake is still coming.

Three things this is NOT, because each was tempting and each is wrong:

- **Not a timeout.** The process is never signalled. The bound is on
  the agent's SLEEP, not the work's life. `--expect` (below) is named
  to say that: it is an estimate the agent gives, not a limit clank
  enforces.
- **Not a failure.** The wake text must not read as an error or as
  work. An agent woken with "X has been running 12m" and no framing
  will go looking for something to fix. It says what happened, that
  it is a check-in, and what the two sane responses are.
- **Not a second channel that replaces the first.** The completion
  notification stays exactly as it is. The check-in is an extra wake
  in the noise direction — the same direction every other attendance
  decision fails toward.

## The flag: `--expect <DURATION>`, default `5m`

On both `clank run` and `clank attending`:

    clank run --desc "cargo build" --expect 10m -- cargo build
    clank attending b72qah60w --desc "cargo build" --pid 4242 --expect 10m

Help text, in substance: "How long you expect this to take (default
5m). Not a timeout — nothing is cancelled. If the work is still running
past it you are woken once with a check-in, so a hang cannot hold you
asleep for hours."

- **Grammar** is clank's one duration grammar (`parse_duration_str`:
  `30s`, `5m`, `1h`). Nothing new to learn.
- **`0` is refused**, not "no bound". The shared grammar reads `0` as
  none, and there is no such attendance: an agent that wants to sleep
  through a hang can say `--expect 24h` and be honest about it. One
  shape for every marker; the hook has no "unbounded" branch to get
  wrong.
- **Why not `--duration`/`-d`** (lloyd's floated name, considered):
  "duration" does not say WHOSE — the command's, the sleep's, the
  hook's — and `-d` sits one keystroke from `--desc`, which is the flag
  right next to it on every invocation. `--expect` says the number is
  the agent's estimate, which is precisely its status. No short form,
  matching `--desc`.

## Where the clock starts: at the marker

The expectation is about the WORK, so the clock starts when the marker
is written — `clank run` writes it immediately before the exec, so
that is the work's start; a hand `clank attending --pid` starts it at
the attend. The marker records `since` (RFC3339) and `expect_secs`.

Consequence, stated so nobody is surprised: an agent that launches a
build, works alongside it for eight minutes, and ends its turn with a
`5m` expectation is checked in on IMMEDIATELY — the work is past its
bound and still running, which is exactly the condition. The check-in
names the elapsed time, so the agent sees why. It may re-attend with a
larger `--expect` if it meant "more time from now".

Measuring from turn-end instead would make `--expect` a lie (it would
bound the sleep, which the agent cannot estimate at launch, rather
than the work, which it can). Rejected.

## Where the timer lives: in the hook process, not in the marker

The marker stays one-shot and consumed at hook entry. That is the
whole safety argument of [[attending-suppresses-standing-wakes]] and
this plan does not touch it. The check-in is a TIMER held by the
asyncRewake hook process that consumed the marker:

    entry: consume marker → provably live → record `attended` (as today)
      write `attending.holder` = this attendance's identity
      then, instead of exiting 0 at once:
        wait until the earliest of
          · the work ends  (pid dead, or token no longer holds it)   → Silent
          · `attending.holder` no longer reads this identity          → Silent
          · the session generation changes                            → Silent
          · the poll ceiling                                          → Silent
          · `since + expect`                                          → final check → Continue

- **Liveness is polled**, every couple of seconds, with the same
  `pid_is_alive` + `token.still_holds` the proof rule uses. A process
  that ended before its bound exits the timer quietly; the completion
  notification is the wake, as today. The poll is `kill(pid, 0)` and a
  proc-table read — nothing spawns.
- **The final check is load-bearing.** When the bound arrives the
  predicate is evaluated once more before emitting. A process that
  ended in the last poll interval gets Silent, not a check-in for work
  that just finished and whose completion wake is already in flight.
  The residual window between that check and the exit is inherent and
  lands on noise.
- **Generation.** Same guard as the park: a timer from a dead session
  incarnation emits nothing. Its stderr pipe is dead anyway; this
  keeps the model uniform rather than relying on that.
- **Ceiling.** The timer runs under `poll_deadline(started)` like every
  other in-hook wait. At ~24 days it never wins in practice, but the
  hook must expire below the runner's ceiling rather than be killed at
  it, and when it does the outcome is Silent: the completion wake is
  still outstanding, so silence is safe there.
- **No lease.** The timer is not the park and takes no `wait.lock`. A
  park from an earlier turn-end stays parked; both can wake the agent,
  each for its own reason.

The seam is the same shape as `await_wait_under_deadline`: the
predicate ("still worth waking") and the clock are what tests inject,
so the timer is driven under `start_paused` and nothing sleeps or
spawns for real.

### The timer's control identity: `attending.holder`

The first draft had the timer read the `attended` projection to learn
whether it had been superseded, and treated a missing record as "keep
going". Codex on ddb12fc: that makes `--clear` unable to cancel an
armed check-in — it removes the marker, which the hook consumed at
entry, and leaves the timer to fire — and it makes an explicit clear
indistinguishable from the projection being swept by an ordinary
no-marker turn-end. A status projection cannot double as a control
protocol.

So the timer gets its own, on the pattern `wait.holder` already sets:

- **Identity.** Every marker carries `since`, an RFC3339 instant at
  nanosecond precision, written once by `clank run`/`clank attending`
  and consumed at most once. That is the attendance's identity; the
  check-in armed for it has the same one.
- **`attending.holder`** names the attendance whose check-in is
  currently armed. Written by the hook at the silence decision, with
  the consumed marker's `since`. The timer polls it alongside liveness
  and yields the moment it reads anything but its own identity —
  missing included.
- **Re-attend supersedes.** A newer attendance decision overwrites the
  holder; the older timer reads a foreign identity and yields. An
  agent woken early that re-attends the same work with `--expect 20m`
  has replaced its earlier expectation, and the 5m timer does not fire.
- **`--clear` cancels.** `clank attending --clear` removes the marker
  AND the holder. The timer reads nothing and yields. "No longer
  attending" then means exactly that: no marker for the next turn-end,
  and no check-in armed. The confirmation says which of the two it
  found (`no longer attending; the armed check-in is cancelled` /
  `was not attending anything`).
- **An unrelated turn-end leaves the timer alive.** `consume_attending`
  sweeps the marker and the `attended` projection, as today, and does
  NOT touch the holder: a turn-end that attended nothing is not a
  withdrawal of the expectation. The work is still running, and the
  check-in the agent asked for is still wanted.
- **The timer never deletes the holder.** On firing or on the work
  ending it leaves the file as it is. A compare-then-delete has a
  window in which it deletes a NEWER attendance's holder and silences
  that timer — the wrong direction. A stale holder costs nothing:
  only a live timer ever compares against it, and `--clear` removes it
  whenever a human or agent wants the slate clean.

The three transitions — supersede, clear, leave alone — are each a
distinct file state the timer can observe, so each is specified and
tested through the production composition below rather than inferred
from what happens to be on disk.

## Only the asyncRewake hook can hold a timer

Codex's hook cannot hold a marker at all — its input carries no
`background_tasks`, so no marker is ever provably live there — and
nothing changes for it. Legacy claude (`LoopPolicy::BackgroundArm`)
runs its Stop hook synchronously; parking five minutes inside it would
freeze the session. Outside `async_loop`, attendance silences at once,
exactly as today. This is a limitation, named, not a branch anyone
should widen: the check-in exists where the hook can outlive the turn.

## The check-in

Written for an agent with no context, because that is who reads it:

    clank: "cargo build" (pid 4242) has been running for 12m — you
    expected 5m. It is still running; nothing has been cancelled and
    nothing has failed. This is a check-in, not a task.

    Look at its output so far and decide whether it is stuck. If it just
    needs longer, re-attend it with a bigger expectation and end your
    turn:

        clank attending b72qah60w --desc "cargo build" --pid 4242 --expect 20m

    Otherwise end your turn; its completion still wakes you when it ends.

Every value in the recipe is filled in — the agent must be able to
paste it. The pid, description, elapsed and expectation are the
record's own. The task id is the harness's, and for a `clank run`
marker it is NOT in the record: the marker stores the description as
`task`, because no id existed when it was written. It does exist at
the proof point. The hook proves a run marker by finding EXACTLY ONE
live background task that is recognisably its run, and that task
carries the id — so the proof yields the id: `live_clank_run_matches`
becomes a lookup returning the matched tasks, and `provably_live`
returns the harness id the evidence named, or nothing. A unique match
the tool listed WITHOUT an id is not proven at all (codex on 134f354):
it could never be re-attended, so the turn parks as any unproven
attendance does. For a hand `clank attending` marker the id is the
marker's `task`. The recipe is built from those typed fields by one
function beside the parser that reads it back, and every word of it is
shell-quoted — a description is user text, and `$(…)`, backticks and
quotes in it must paste as the same letters, not run. The crate had
three private copies of a POSIX quoter; there is now one.

After the check-in the agent's next turn-end has no marker and parks
normally. That is the pre-existing behaviour for an unattended live
task: the completion notification still wakes, and review work wakes
via the park. Re-attending is optional and buys a fresh check-in.

## What the human sees

`Attended` — the decision record `clank status` reads — carries
`since` and `expect_secs` too, so the age it renders is the WORK's
age, against its bound:

    attending: claude → 4242 (b72qah60w) · 3m/5m

and after the bound, `· 12m/5m`, which says "overdue" without a word.
The TUI marker row shows the same form through the same accessor; its
width tables already drop fields from the right, and three more
characters on the age field are within them (verify, do not assume).
Records without the fields — none should exist, since the marker and
its decision are swept every entry, but the reader stays lenient as
`desc` did — render as they do today.

`clank attending`'s confirmation names the bound, since it is the
caller's only sight of what was stored:

    attending "cargo build" (`b72qah60w`, pid 4242) — your next turn-end
    stays silent while the tool still reports this work running; if it is
    still going after 5m you will be checked in on

## Tests

Flag and marker:

- `--expect` parses the shared grammar and defaults to 5m on both
  commands; `--expect 0` and garbage are refused with an error naming
  the flag, and no marker is written.
- Both writers record `since` and `expect_secs`; a legacy marker
  without them still parses.

The timer, through the production composition (the function that
holds the entry decision, the wait and both exits — not its parts
reassembled in the test, which is the trap the deadline tests already
document):

- The work ends before the bound → Silent(AttendingBackgroundTask),
  and the timer is gone: no check-in follows.
- The bound arrives with the work still live → Continue, and the
  reason names the description, pid, elapsed and expected values, says
  nothing was cancelled, and carries the re-attend recipe with the
  pid and description filled in.
- The bound is already past at entry with the work live → Continue at
  once, after the final check.
- The work ends inside the last poll interval → the final check yields
  Silent, not a check-in.
- The three holder transitions, each driven through the composition
  with the timer already armed: a re-attend (a newer identity in the
  holder) → Silent, no check-in; `clank attending --clear` (holder
  removed) → Silent, no check-in, and the marker is gone too; an
  unrelated no-marker turn-end (marker and projection swept, holder
  untouched) → the check-in still fires at the bound. Both directions
  are asserted so none can be "fixed" by ignoring the file.
- The check-in for a `clank run` marker carries the harness id of the
  uniquely matched task, yielded by the proof; for a task-id marker it
  carries the marker's `task`. A unique match without an id is not
  proven: nothing recorded, no timer armed. The recipe in the reason is
  asserted as a complete command line, and as one shell-safe line for a
  description and id carrying `$(…)`, backticks, both quote kinds and
  backslashes; the quoter itself is tested on each metacharacter and
  on the empty word.
- The timer does not delete the holder on either exit.
- Generation change → Silent(StaleGeneration). Ceiling before bound →
  Silent, never a check-in for work that may have finished.
- `async_loop = false` → Silent immediately, no waiting.

Display:

- `Attended` with `since`/`expect_secs` renders `3m/5m` on the status
  line; past the bound it renders `12m/5m`; a record without the
  fields renders the plain age it does today.

Every claim is mutation-checked with production-only edits (a bound
that never fires, a final check removed, supersession inverted), and
each check must FAIL the test — a mutation that does not compile
proves nothing.

## Out of scope

- Cancelling, signalling or otherwise touching the attended process.
  The TUI's kill control is the human's, and stays so.
- A check-in for codex or legacy claude hooks, for the reason above.
- Repeating check-ins (every N minutes). One wake, then the agent
  decides; re-attending is how it asks for another.
