# a-mod-delivers-work-in-process
# clank — deliver work from inside Claude Code, not by exiting a process

## Why

Work reaches a Claude session today by clank spawning `clank stop-hook` at every
turn end, which spawns `clank wait`, which parks until work appears and then exits 2
so that Claude reads its stderr. Three things are wrong with that, and the third is
what makes this a plan rather than a bugfix.

**1. Parked waits leak, one per conversational exchange.** A park is cancelled only
by its own exit. When the next turn starts for any other reason — you typed, you
interrupted, a background task completed — the park is orphaned and keeps watching.
The next turn end arms another. Measured on this machine: 54 live `clank wait`
processes across 42 sessions; 6 sessions had duplicates, and every one of those is a
session a human talks to (`clank`, `clank-wr`, `fs-war-room` x2, `esphal-v1-restack`,
`nonce-cap`). Purely autonomous sessions sit at exactly one, because there the park
IS the waker and exiting is how it wakes you. In `~/src/fswt/genuine-check` thirty had
accumulated over six days; when a commit finally moved the gate all thirty exited
within 548ms and the session got thirty notifications, spaced 3-12ms
(`~/.claude/projects/-Users-llfourn-src-fswt-genuine-check/34ce157e-*.jsonl`,
lines 3142-3173).

**2. The mechanism is already Claude-version-coupled, and goes stale silently.**
`claude_asyncrewake_capable` (`setup.rs:896`) parses `claude --version` against a
hardcoded `2.1.223` floor, and the chosen mode is frozen into `settings.json` at setup
time. A Claude upgrade does not re-probe. There is no stable contract here to defend.

**3. Every safe repair is worse than the disease.** The obvious fix — a new park
supersedes the old — requires killing another process. Two hooks arming at once each
see the other and kill it: zero waiters, session silent until a human types. A strict
`(start_time, pid)` order removes that particular race, but the trade stays bad: the
upside is less noise, the downside is a dead agent, and clank's own code already says
"silencing a turn with no provable wake is how an agent goes to sleep for good".
Duplicate wakers are noise. No waker is fatal. We do not trade the second for the first.

## The direction

Claude Code ships MODS (`anthropics/claude-code:mods/`): plugins whose behaviour is a
hooks module with one `register(on, options)` entry, handlers shaped `($, e, next)`,
running IN-PROCESS. If clank's delivery lives there, the leak cannot exist — there is
no second process to orphan, and no `--deliver-to-session` guard needed because there
is no second process whose identity can drift.

## What the research already found

Read from `mods/types/claude-code.d.ts` (13,044 lines) and `mods/README.md`. Recorded
so implementation starts here rather than at zero. ALL OF IT NEEDS RE-CHECKING — see
below.

- **`$.session.receive`** is "one inbound delivery (a relay's event, a peer's message,
  a Remote Control prompt), sanitized, before it is queued", and one of its
  `SessionReceiveOrigin.kind` values is **`task-notification`**. clank's wakes ALREADY
  arrive through this path: the transcript records each one as a
  `queue-operation: enqueue` whose content opens with `<task-notification>`. A mod
  would call that channel directly instead of exiting a process to reach it.
- Engine events are callable on `$` — "The engine raises its events through these same
  calls; a plugin's call runs the same chain with the calling hook alone skipped."
- **`turn.start`** fires when a turn begins. In-process, that is the signal the current
  design cannot see: the mod can cancel its OWN outstanding watch when a new turn
  starts, which is a cancellation, not a kill, and therefore has none of the race.
- **`classic.Stop`** exists as a mod event — every classic hook event is exposed as
  `classic.<Name>` — returning `additionalContext` plus
  `block`/`preventContinuation`/`stopReason`.
- **`turn.complete`** carries `reason: 'answer' | 'aborted' | 'refusal' | 'error'`.
- **Budget: `HookBudget.ms = 10_000`** per dispatch — but it counts the hook's OWN code
  only: "the clock stops while a `next(e)` call or any `$` call of the hook's is in
  flight (a `$.clock` wait excepted)". `$.process.run` caps at ten minutes.
- Mods load via `claude --plugin-dir <path>`; structure is `.claude-plugin/plugin.json`
  + `hooks/hooks.json` + TypeScript under `hooks/`.
- **"Early access: hooks modules load only where function hooks are enabled, and the API
  these mods are written against may change between releases without notice."**
- This machine: claude `2.1.280`; no `functionHooks` key in `~/.claude/settings.json`.

## Research this properly before designing

The instruction is deep research, and it applies to the REVIEWER as much as the
implementer: review this plan against the type declarations, not against this summary.
Fetch `mods/types/claude-code.d.ts` and read it. Every bullet above is a claim to
verify, and the ones below are unanswered.

1. **Can a mod deliver work while NO turn is running?** This decides everything. If
   `$.session.receive` can only be called from inside a dispatch, there is no
   out-of-turn injection and the design must fall back to something else. If it can be
   called from a watcher the mod holds, the Stop hook disappears entirely.
2. **What owns a long watch?** The park lasts hours. A hook dispatch cannot. Options to
   evaluate: a module-scope watcher started at load/`session.start`; repeated bounded
   `$.process.run` calls (<=10m each, clock stopped); or something else the API offers.
   Say which, and why the others were rejected.
3. **Is the mod's lifetime the session's?** What happens on `/clear`, on compact, on
   resume, on reload. An orphan here would be the same bug in a new house.
4. **Are function hooks enabled on our machines, and how is one turned on?** If they
   are not, this plan stops at a spike and says so.
5. **Persistent install.** `--plugin-dir` is a launch flag. Is there a settings-level
   install, and can `clank setup` write it the way it writes hook entries today?
6. **What does the mod do with clank's logic?** clank is Rust; mods are TypeScript. The
   mod shelling to `clank wait`, versus talking to a clank process, versus something
   else — pick one and say why. Do NOT reimplement clank's decision logic in TypeScript.
7. **The API may change without notice.** What is the failure mode when it does, and
   can we make it loud rather than silent? A delivery path that fails quietly is the
   thing we are trying to stop building.

## Findings, round 1 (read from the declarations and the shipped mods)

Pinned: `mods/types/claude-code.d.ts` last changed at `cbab6f4598e9` (2026-09-24);
`anthropics/claude-code` HEAD `dec92bc87ab6` (2026-09-29). Re-pin before trusting any
of this — the README says the API may change between releases without notice.

**Q1, out-of-turn delivery — SUPERSEDED by round 2 below; the conclusion here is
wrong for an installed plugin.** Kept as written so the correction is legible.

**Q1, out-of-turn delivery — strongly supported, NOT yet proven for `session.receive`.**
The shipped `telemetry` mod establishes the pattern end to end. Its `engine.create`
hook captures `beneath` (the `$` built so far) into closure state:

```ts
on('engine.create', async (_$, e, next) => {
  const beneath = await next(e)
  sender = telemetryOf({ authorize: () => beneath.session.authorize(),
                         id: () => beneath.session.id(), ... })
```

and its flush is scheduled on an ordinary timer —
`timer ??= deps.after(Batching.BATCH_WINDOW_MS, () => void flush())` — where `flush`
calls those captured nouns and posts over the network. `deps.after` is
`(ms: number, fn: () => void) => Timer`, injected only for testability. So a
first-party mod DOES call engine nouns from a timer callback outside any hook
dispatch, and holds engine references across dispatches to do it.

What that does not settle: `telemetry` only calls READ-ish session nouns
(`authorize`, `id`, `model`). `session.receive` mutates the session's delivery queue.
Whether the engine permits that from an idle watcher is the spike, and it stays the
gate. If it is refused, record the refusal verbatim — it will say who refused and why.

**Q2, what owns the long watch — answered.** Module-scope state plus ordinary JS
timers. `HookBudget.ms` (10s) bounds a DISPATCH, and only the hook's own code within
it; a watcher living in closure state is not in a dispatch at all, so the budget and
the ten-minute `$.process.run` cap bound neither the watch nor its lifetime. The
`$.process.run` cap still bounds any single child invocation, which matters only if
the watch is implemented by shelling `clank wait` (see Q6).

**Q3, lifetime — partly answered.** `session.start` and `session.end` both exist as
mod events, and `telemetry` flushes on `session.end`, so there is a defined teardown
point. Behaviour across `/clear`, compact and resume is still unverified and an orphan
there would be this same bug in a new house.

**Q4 enablement, Q5 persistent install, Q6 the Rust/TypeScript boundary, and Q7 the
drift failure mode remain open.** This machine: claude `2.1.280`, and no
`functionHooks` key in `~/.claude/settings.json`.

## Findings, round 2 (spiked against the live binary, claude 2.1.280)

A throwaway mod was built and run headlessly — `claude -p ... --plugin-dir`. Every
refusal below is the engine's own text.

**Q4, enablement — ANSWERED, and it is a switch not a build.**

> `hooks modules are not turned on for installed plugins in this process (early
> access: set CLAUDE_CODE_ENABLE_FUNCTION_HOOKS=1 in its environment to load them);
> built-in plugins load regardless`

**The sandbox — a constraint nothing in the declarations told us.**

> `cannot import "node:fs": a hooks module imports its own files by relative path and
> "claude-code", nothing else`

No Node builtins. A mod CANNOT shell out directly; every effect goes through `$`.
That answers Q6's mechanism by removing the alternatives: reaching `clank` means
`$.process.run`.

**Round 1's conclusion is WRONG for an installed plugin.** The engine statically
refuses to let one capture the engine:

> `the value of next(e) at engine.create is assigned; $ is always spelled
> $.noun.event(...) at the call site`

`telemetry` does exactly what round 1 described, but `telemetry` is BUILT IN and
"built-in plugins load regardless". Reading a shipped mod as a template for an
installed one was the mistake. So there is no captured `$`, no timer holding it, and
therefore no out-of-turn delivery by that route — and `session.receive` from an idle
watcher is not merely unproven, it is unreachable this way.

**It turns out not to be needed. The design below was proven end to end:**

```ts
on('classic.Stop', async ($, e, next) => {
  if (e?.stop_hook_active) return next(e)              // the loop guard, free
  const r = await $.process.run(['clank','wait',...])   // budget clock stopped
  return { additionalContext: [render(r.stdout)] }      // string[], not string
})
```

Observed, in one headless run: `classic.Stop` fires for an installed mod;
`e.stop_hook_active` is readable (`false` on the first fire, `true` on the second);
`$.process.run` returned `exitCode 0` with stdout captured; `$.fs.write` works; and
the model received the text — it replied *"The stop hook reported 'CLANK-SPIKE
delivered: work-from-clank'"*. The second fire stood down on `stop_hook_active` and
the run ended with no loop.

Three things fall out. ~~There is no second process, so nothing can be orphaned — the
leak this plan exists for cannot occur in this shape.~~ **THIS CLAIM IS FALSE; see
round 3.** `$.process.run` starts a child like any other. `HookBudget.ms` does not bite,
because the clock stops during any `$` call; the real bound is `$.process.run`'s ten
minutes, so a long park is a loop of bounded waits. And `stop_hook_active`, the field
the current hook deliberately ignores, is right there as the loop guard.

**Q7, drift failure mode — ANSWERED, and it is reassuring.** Every failure was LOUD,
named the file and the rule, and degraded to "hook skipped" with the session
continuing: a missing switch, a forbidden import, a refused static shape, and a
wrong return type each printed a specific sentence. A silently-dropped delivery was
not among the failure modes observed.

**Still open:** persistent install (`--plugin-dir` is per-session; is there a settings
path `clank setup` could write?), lifetime across `/clear`, compact and resume, and
what `block` / `preventContinuation` do on `classic.Stop` versus `additionalContext`.

## Findings, round 3 (the correction codex asked for)

**The "no second process" claim was wrong.** `$.process.run(['clank','wait',...])`
starts a child. What changes versus today is who OWNS it, and ownership was the thing
I had not tested. `ProcessRunInit` is `{ cwd, env, stdin, timeoutMs }` — no signal,
no abort — and the only documented kill is the timeout.

**Tested: the child outlives the engine.** A `classic.Stop` hook started
`/bin/sleep 417` with `timeoutMs: 600000`; the engine was killed mid-dispatch with
SIGTERM; three seconds later `96650 /bin/sleep 417` was still running. So the engine
does not reap `$.process.run` children when it dies, and nothing in the type says it
reaps them when a dispatch is abandoned either.

**clank's existing self-reap cannot be used here.** `--die-with-owner` works by stdin
pipe EOF — "the spawner must hand this process a pipe as stdin and hold the write
end". `$.process.run`'s `stdin` is "text written to the child's standard input, then
closed". The pipe closes at once, so a `clank wait --die-with-owner` under
`$.process.run` would exit immediately instead of parking. The two mechanisms are
structurally incompatible, and that is a clank-side problem with a clank-side answer:
a self-reap that watches the parent pid rather than a pipe.

**What the route actually buys — asserted in round 3, MEASURED in round 4.** The
leak becomes BOUNDED rather than eliminated: an abandoned child lives at most
`timeoutMs` (ten minutes, capped by the engine) instead of forever, so concurrent
children are bounded by `timeoutMs / exchange-interval` instead of growing without
limit. That is a real improvement over today and it is NOT what the plan's done
condition asks for ("delivery without a parked process"), so the condition is not met.

Round 4, in full, because codex was right that round 3 had not earned this. The
timeout is enforced OUTSIDE the engine process:

| test | timeoutMs | engine killed | child at deadline |
| --- | --- | --- | --- |
| 1 | 600 000 | SIGTERM | alive at T+3s (deadline far away — proves nothing) |
| 2 | 5 000 | SIGTERM | gone by T+20s (ambiguous: a graceful shutdown could have enforced it) |
| 3 | 5 000 | SIGKILL | alive T+0..T+4s, **GONE at exactly T+5s** |

Test 3 is the one that settles it: SIGKILL leaves no engine to enforce anything, the
child was still alive four seconds later, and it died precisely on its deadline. So
`timeoutMs` binds an abandoned child whether or not the engine still exists, and
test 1's survivor was simply a child with nine and a half minutes left to run.

**Not yet proven, and it is the thing that decides whether this is good enough:**
whether an abandoned dispatch's child can still DELIVER. Today's orphans each exit 2
and each produce a notification — that is the visible bug. If an abandoned dispatch
discards its result, the visible bug is gone even with a bounded process leak. If it
does not, this route reproduces the bug with extra steps. Testing it needs a live
interactive session — interrupt a turn mid-dispatch, then see whether the abandoned
child's output ever reaches the model — and a headless single-turn run cannot show it.

**Still to test, per the review:** the count across repeated exchanges in one live
session, and cancellation on a user-started turn rather than on engine death.

## Findings, round 5 — scoped to ENGINE DEATH (see round 6 for the live case)

**A dispatch whose ENGINE WAS KILLED does not deliver into the resumed process.**
This says nothing about a still-live session, which is where the real leak happens;
round 6 tests that separately. The claim of "no duplicate wakes at all" made here
first time round was too broad and is withdrawn.

Method: a `classic.Stop` hook read a token from a file, slept 20s inside
`$.process.run`, then returned `additionalContext: ["STOPHOOK-SAYS <token>"]`. Run C
was SIGKILLed mid-sleep with the token `CHARLIE`; the token was then changed to
`DELTA` and the conversation resumed with `--continue`.

Result: the resumed session saw `DELTA` only. `CHARLIE` never arrived, though its
dispatch had begun and its hook later ran to completion.

**A first attempt at this was INVALID and is recorded so nobody repeats it.** The kill
hit the shell wrapper rather than the engine, run A completed normally, and the
resumed session saw the earlier token — which looks exactly like a late delivery and
is not. The tell is the run's stdout: 159 bytes (a completed answer) versus 0 bytes
when the dispatch is genuinely abandoned. Check the byte count, not the kill's exit
status, and beware `pgrep -f` matching the checking command's own argv.

**Consequence, scoped.** For the engine-death case only: the orphan's result is
discarded, so a child that outlives a dead engine cannot wake anyone. Whether a
LIVE session can end up with two dispatches delivering is round 6's question.

**One thing to design around.** The abandoned hook KEPT RUNNING after the engine was
killed — it completed its sleep and wrote its log — so an abandoned `clank wait` will
observe work, return it, and have the answer thrown away. clank must therefore never
treat "the wait returned items" as "the items were presented". Today's WAL keeps items
unhandled until an explicit ack, so this is believed safe, but it is now a stated
requirement rather than an accident.

Marked as inference, not fact: that the hook survived a SIGKILLed engine, and that
`timeoutMs` is still enforced afterwards (round 4), together suggest the hooks module
runs in a host process separate from the CLI process being killed. Nothing read so far
says so, and the design does not depend on it.

## Findings, round 6 — the live session, and a new problem

Driven through `--input-format stream-json`, which feeds several user messages to ONE
live engine. Turn 1 was sent, its `classic.Stop` dispatch began a 25-second child, and
turn 2 was sent two seconds into it — the exact shape that orphans a park today.

The hook log is unambiguous about what the engine did:

```
BEGUN TOKEN-ECHO          <- turn 1's Stop dispatch
COMPLETED TOKEN-ECHO      <- 25s later
BEGUN TOKEN-FOXTROT       <- turn 2's dispatch starts only now
```

**A QUEUED message waits.** The engine did not begin turn 2 while turn 1's Stop
dispatch was in flight; the user's message waited, and both tokens were delivered
once each, in order. This covers the non-aborting path ONLY — an interrupt is
round 7.

**But that is also the problem.** If a Stop dispatch blocks the next turn, then a
clank park of any length delays the user by that length. A ten-minute
`$.process.run` would make the session unresponsive for ten minutes after every turn
end. That would be far worse than the bug being fixed.

Stated as inference, not measurement: the ordering above shows turn 2 waited, but the
delay was not timed directly, and it is not established whether the engine signalled
`next.signal` (the hook ignored any signal and awaited its child regardless, which is
what a naive clank port would do). The next measurement is timestamps on both events
plus a hook that honours `next.signal` — if a well-behaved hook can return promptly on
abort, the design survives with short polls; if not, `classic.Stop` cannot hold a long
park and the route needs a different event or a different shape entirely.

## Findings, round 7 — the interrupt path, and what it settles

Driven as round 6, but instead of letting the message queue, a real in-session
interrupt was sent mid-dispatch:
`{"type":"control_request","request":{"subtype":"interrupt"}}`.

```
1790668350 BEGUN TOKEN-ECHO
1790668352 ABORT-SIGNAL-FIRED TOKEN-ECHO     <- 2s later
1790668358 BEGUN TOKEN-FOXTROT
1790668383 COMPLETED TOKEN-FOXTROT
```

Four facts, each measured:

- **`next.signal` fires.** The hook's abort listener ran two seconds after the
  dispatch began.
- **The `$.process.run` child is reaped on abort** — live children went 1 to 0 —
  even though this hook deliberately IGNORED the signal and kept awaiting. So
  cleanup does not depend on the hook behaving well.
- **The aborted dispatch delivered nothing.** `TOKEN-ECHO` appears 0 times in the
  output stream.
- **The engine survived** and went on to the next turn, with one child alive at a
  time, never two.

Taken with round 6, both live paths are now covered: a queued message waits for the
dispatch, an interrupt aborts it and reaps its child, and in neither case does an old
dispatch deliver beside a new one. The duplicate-delivery incident this plan exists
for cannot be reproduced in this design.

**A trap in my own instrumentation, recorded because it nearly became a finding.**
The driver computed `OVERLAP = BEGUN(second) && !COMPLETED(first)` — which an ABORTED
first dispatch satisfies trivially, since an aborted dispatch never completes. It
printed `OVERLAP = True` where nothing overlapped. The honest measure is the
concurrent child count (1, never 2). An assertion a degenerate case satisfies is not
evidence.

**What stays open from round 6:** a queued message waits for the running dispatch, so
a long park would delay a user who TYPES rather than interrupts. Whether an
interactive session turns typing into an interrupt (which round 7 shows is handled
cleanly) or into a queued message (which round 6 shows waits) is not established, and
it decides how long a park may safely be.

## Findings, round 8 — persistent install, and round 9 — the leak is gone

**Q5, persistent install — ANSWERED.** `claude plugin init` scaffolds at
`~/.claude/skills/<name>/` and "auto-loads next session as `<name>@skills-dir`".
Verified directly: a plugin placed there loaded its hooks module with NO
`--plugin-dir` flag (a harmless `session.start` probe wrote its marker; the probe was
removed immediately). That is the same directory `clank setup` already writes
`clank-master/SKILL.md` and friends into, so shipping the mod is the mechanism setup
already has.

One deployment constraint comes with it: `CLAUDE_CODE_ENABLE_FUNCTION_HOOKS=1` must be
in the ENVIRONMENT of the claude process. clank can set that for sessions it launches
itself; a hand-launched `claude` needs it from the user's profile, and without it the
mod loads as a plugin but its hooks module is skipped — loudly, with the reason. That
is the fallback behaving correctly rather than a failure, but setup has to say so.

**Round 9 — repeated exchanges, but measured AFTER each park had finished, which
proves nothing.** It counted children 22 seconds after a 3-second park, when a
leaking implementation would also read zero. The condition the bug needs — a new
exchange while the previous park is STILL LIVE — was never created. Superseded by
round 10; kept for the method's sake. Five exchanges through one live engine, with a
`classic.Stop` hook that parks 3s per turn end:

```
baseline children: 0
after exchange 1..5: sleep-children = 0   (every one)
final children: 0
hook log: 5 DISPATCH, 5 DONE
```

Flat at zero, and the hook demonstrably ran on all five. Today the same five
conversational exchanges leave five parked waits, every one of which later delivers.

**The first attempt at this measurement was worthless and is recorded as such.** It
reported the same flat zero, but the mod had no logging, so "0 children" was equally
satisfied by "no leak" and by "the hook never ran" — the same
degenerate-satisfaction trap as round 7's OVERLAP predicate, twice in one plan. The
count only became evidence once `5 DISPATCH / 5 DONE` ruled out the second reading.

## Findings, round 10 — the causal sequence, measured

The test round 9 should have been. Park raised to 20s so it is comfortably live; four
times, the next exchange was sent WHILE the park was running; children sampled every
second throughout.

```
park 1 live: children during=1, just after next send=1
park 2 live: children during=1, just after next send=1
park 3 live: children during=1, just after next send=1
park 4 live: children during=1, just after next send=1

MAX CONCURRENT CHILDREN across run: 1      (131 samples, 0 of them above 1)
```

and the hook log alternates strictly, five parks, none overlapping:

```
954 BEGUN   974 DONE
979 BEGUN   999 DONE
1002 BEGUN  1022 DONE
1026 BEGUN  1046 DONE
1049 BEGUN  1069 DONE
```

So an exchange arriving during a live park does not create a second park. Today the
same four early turns leave four more parked waits, each of which later delivers.
With round 7 (an interrupt aborts the dispatch and reaps its child, delivering
nothing) both live paths are now measured, not inferred.

**Latency, no longer an inference.** Each park runs a full 20s and the next BEGUN
follows its DONE by about 5s — the next turn waits out the park. So the delay a
typed message suffers is the park's length. A ten-minute `$.process.run` would mean a
ten-minute wait. **The park must therefore be SHORT and re-entered** — seconds, not
minutes — and clank's wait needs a bounded-poll mode rather than one long block. That
is a design requirement, and it is the one thing these rounds have added to the
build rather than removed from it.

Unmeasured still: whether an interactive session turns TYPING into the queued path
(waits, above) or the interrupt path (aborts cleanly, round 7). Short parks make the
question much less sharp either way.

## Findings, round 11 — the reentry EXISTS; delivering from it does not (yet)

codex named the gate: a short poll that returns empty leaves an idle session with no
watcher and nothing to call `classic.Stop` again, while a loop inside the dispatch
recreates the latency. So `classic.Stop` alone cannot give both prompt input and a
continuously armed wake. Correct, and the search for an engine-supported reentry
found one.

**`watchPaths` + `classic.FileChanged` IS the reentry, and it fires while idle.**
A `classic.SessionStart` hook returning `{ watchPaths: ['/tmp/watchdir'] }` was
accepted, and 25 seconds after the session went quiet, touching a watched file fired:

```
1790669462 SESSIONSTART registering watchPaths
1790669487 FILECHANGED add /tmp/watchdir/work.txt
```

No turn was running. That is exactly what the design needs, and clank's work is
already signalled by files under `.clank/`, so the watch has something real to watch.
It also means NO park and NO child at all — the engine does the waiting.

**But nothing found so far can DELIVER from that dispatch.** Two candidates, both
refused, both loudly:

- `$.session.receive` is **undefined at runtime** for an installed plugin, whatever
  the declarations show: *"'$.session.receive' is undefined"*.
- `$.prompt.submit` is refused with an explanation and a pointer:
  *"called from a classic.FileChanged hook, it would wait on the turn this hook may
  be holding; submit from a later event (turn.complete)"*.

`FileChanged` is absent from `ClassicResultFields`, so it cannot return
`additionalContext` either. Deferring the submit past the dispatch would mean holding
`$` in a closure, which is the pattern the static checker already refuses (round 2).

**So the gate is now split in two, and only one half is open.** Reentry while idle:
SOLVED, engine-supported, no process. Delivery from that reentry: unsolved. The next
thing to try is what the engine's own error suggests — whether `$.prompt.submit` is
permitted from `turn.complete` or another event, and whether a FileChanged hook can
record work in module state (not `$`, which is allowed) for a later event to submit.
If nothing can deliver while idle, the honest conclusion is that this route cannot
satisfy both prompt input and a continuously armed wake, and the plan says so.

**A fourth static rule, learned the hard way.** A helper that takes `$` must be a
TOP-LEVEL function declaration: a `const` arrow inside `register` is refused —
*"`$` is passed to `mark`, which is not a function declared at the top of this
file"*. Every probe in these rounds now follows that shape.

## Findings, round 12 — an idle session IS woken, by a chain

codex: FileChanged is not a wake until it delivers, and module state for a later
`turn.complete` is circular since no turn completes until something starts one. Both
correct. Here is a chain that closes it, every step engine-supported, demonstrated end
to end.

**Every delivery call, probed from an idle `classic.FileChanged`:**

| call | result |
| --- | --- |
| `$.prompt.submit` | refused — host check, "submit from a later event (turn.complete)" |
| `$.session.receive` | **undefined at runtime** |
| `$.prompt.attachment` | **undefined** |
| `$.prompt.context` | **undefined** |
| `$.session.attach` | **undefined** |
| `$.agent.spawn` | **OK** |

`agent.spawn` is the one thing permitted from that dispatch — and the declarations say
a spawned agent "is its own `turn.complete`", which is the event the engine's own
refusal points at. So the chain is:

```
idle  ->  file changes  ->  classic.FileChanged  ->  $.agent.spawn
      ->  turn.complete ->  $.prompt.submit      ->  main agent turn
```

Measured, twice, with the session idle 23 seconds before the file appeared:

```
1790669880 FILECHANGED add
1790669880 spawn issued
1790669885 TURNCOMPLETE agentId=main pending=true
1790669885 SUBMIT-OK
1790669887 TURNCOMPLETE agentId=main
GOTWORK seen in stream: 2
```

`GOTWORK` is the word the delivered prompt asked the model to reply with, so the work
reached the model and it acted. **Work arriving after the session went idle caused an
agent turn with no user input** — with no parked process and no child, because the
engine does the waiting.

**Three things this costs, stated plainly.**

1. It needs a throwaway subagent spawn purely to manufacture a `turn.complete`. That
   costs tokens and latency on every wake, and it is a workaround, not a design.
   Whether a cheaper trigger exists is unknown.
2. Delivery is a PROMPT, not a system reminder. clank's wakes would appear in the
   transcript as user input rather than as the `<system-reminder>` they are today.
   That is a visible behaviour change and arguably a wrong one.
3. The clean API is missing. `$.session.receive` is exactly the right call, is in the
   declarations with a `task-notification` origin kind that matches clank's wakes
   precisely, and is UNDEFINED at runtime for an installed plugin. If it were
   available the chain collapses to `FileChanged -> session.receive`, with no spawn,
   no prompt, and no workaround.

## Findings, round 13 — `session.receive` is WITHHELD, not missing

Round 12 said the clean API was "missing". It is present and gated. Probing the
`session` noun from an installed plugin:

| `$.session.*` | installed plugin |
| --- | --- |
| `id` | OK — returned the session UUID |
| `model` | OK |
| `version` | undefined |
| `measure` | undefined |
| `receive` | undefined |

The noun exists with a SUBSET of its methods. The read-only ones are there; the ones
that act on the session are not. And built-in `telemetry` calls `session.version`
— undefined here — so the subset is by TIER, which is what the declarations meant by
"judge by `tier` and `uses`". (The `session.version` call is named in the commit that
last touched the declarations, `cbab6f4598e9`: "rows carry the engine's version, base
version and build time from `$.session.version()`". Round 1 quoted `authorize`, `id`
and `model`, not `version`; an earlier draft of this section miscited it.)

`uses` cannot unlock anything: `claude plugin validate` shows it is derived by static
scan of the source, tracing helpers —
`./register.ts calls: $.agent.spawn (via spawnNudge), $.process.run (via mark),
$.prompt.submit (via submitWork)`. It is the host's reading, not the author's request.

So the position is: the chain in round 12 is the INSTALLED-PLUGIN path and it works;
`FileChanged -> $.session.receive` is the clean path and it is built-in-only. That is
worth raising upstream, because the gap is one method on an already-present noun, its
`task-notification` origin kind already matches clank's wakes exactly, and granting it
would delete the subagent spawn, the prompt-versus-reminder semantics problem, and the
whole workaround in one step.

## The production design (the decisions the review asked for)

**Two paths, because the expensive one is rare.** Nearly every clank wake happens at
a TURN END — the agent finishes, work is already waiting. Only work that arrives while
the session is genuinely idle needs the chain.

- **Turn end — `classic.Stop`.** Run `clank wait --peek --json` through
  `$.process.run` with a SHORT bound (seconds). Work present: return it as
  `additionalContext`, which is the closest thing to today's `<system-reminder>` and
  costs nothing extra. No work: return immediately, so the user is never delayed
  (round 10 measured the delay as the park's length).
- **Idle — `classic.FileChanged`.** Only here pay for the chain: `$.agent.spawn` to
  manufacture a `turn.complete`, then `$.prompt.submit` from it (round 12).

This answers the two semantic questions together. The subagent cost is paid only on
idle arrivals, not on every wake; and the common path keeps reminder-like semantics,
so only idle wakes appear as prompts. If `$.session.receive` is ever granted to
installed plugins, the second path collapses to one call and both costs vanish.

**Duplicates are ACCEPTABLE; a spawn per file write is not.** An earlier draft of
this section said both that duplicate delivery is harmless and that it must never
happen. The first is right and the second was wrong. The event WAL is at-least-once
and an item stays unhandled until an explicit ack — the property that already makes
today's re-presentation correct — so a wake arriving twice costs a repeated
`<system-reminder>` and nothing else. `.clank/` changes on every event append, so what
`FileChanged` must avoid is a SPAWN per write: debounce, ask `clank wait --peek`
whether anything is presentable, and do nothing when it is not.

**Real clank integration, with no clank logic in TypeScript.** The mod shells
`clank` through `$.process.run` and renders nothing itself: `clank wait --peek --json`
for the items, and a `clank` call at `SessionStart` to learn this repo's agent and
return its `watchPaths`. The one clank-side change is a bounded-poll mode for the
short turn-end wait (round 10).

**Fallback — and this is where the plan's premise breaks.**

Two attempts failed for the same reason. `CLAUDE_CODE_ENABLE_FUNCTION_HOOKS` proves
function hooks are enabled, not that this mod loaded or is dispatching. A liveness
marker written on each dispatch proves the mod was alive THEN: if it dies a second
later while the session is idle, no Stop dispatch ever occurs to recheck the marker,
and nothing is left to wake the agent. A timestamp is evidence about the past, and the
guarantee needed is about the future.

Generalised: **to promise a wake you need something LIVE and INDEPENDENT of the thing
it is backing up.** Any check that runs only at a turn end cannot cover a session that
is already idle, because a turn end is itself a wake. So under the never-zero-wakers
constraint the external park must stay armed, and the mod cannot replace it.

**Which means the mod cannot be the sole waker.** That is this plan's blocker, and
it is a finding rather than a failure: the research answered every feasibility
question it set out with, and the last one came back no.

**Two problems were being conflated, and only one has an answer.** The Why above names
both: thirty notifications at once (a delivery symptom) and fifty-four live processes
(a resource leak). They are not the same bug and they do not have the same fix.

*Duplicate notifications* have a fix needing no mod and no killing: a park about to
deliver checks for a strictly newer LIVE park for the same `(repo, author, session)`
and exits silently if it finds one. Nothing is killed, so there is no
mutual-destruction race — the objection that sank the first proposal. If the newer
park has died it is not in the process table and the older one delivers. The failure
direction is a duplicate wake, never silence.

*The parked-process leak is UNRESOLVED and belongs to its own plan.* The rule above
leaves every process alive and still accumulating one per exchange, so it must not be
described as a leak fix, and "bounded by the session's exchanges" is not a bound — a
long-lived session has no limit on exchanges. Repairing lifetime needs either a
succession that actually ends the older park (killing, which was rejected for its
mutual-destruction race; a strict total order over `(start_time, pid)` removes that
specific race and is the obvious thing for that plan to examine first) or an
independent waker that is bounded by construction. Neither is settled here, and this
plan should not pretend otherwise.

**What would actually satisfy never-zero-wakers.** Not `$.session.receive`. An
earlier draft said granting it would make a mod trustworthy enough to stand alone;
that is wrong, and the error is worth naming because it repeats a confusion between
CHANNEL and LIVENESS. The failure is the mod ceasing to dispatch after the last turn,
and that is the same failure whichever delivery API it holds. A better channel does
not make a dead process deliver.

Two things could satisfy the guarantee, and only one is in clank's hands:

1. **A waker that is independent of the mod AND bounded by construction.** Today's
   park is independent but unbounded, because its lifetime is tied to the TURN — one
   armed per turn end, none retired. A waker scoped to the SESSION instead would be
   exactly one per session by construction: armed once (clank already has a
   `SessionStart` hook), never re-armed, so nothing accumulates and nothing needs
   superseding. That is the shape the separate leak plan should examine first; it
   reframes the leak as a LIFETIME bug — the waker is per-turn when the thing it
   serves is per-session — rather than as a cleanup problem.
2. **An explicit relaxation of the guarantee**, accepting that a mod failure leaves
   the agent silent until its human types, on the grounds that mod load failures are
   loud and visible. That is a real option and it is the user's call, not this plan's.

**So this plan finishes at its blocker.** Established: the mod route works end to
end, installs persistently from `~/.claude/skills/`, and wakes an idle session with no
user input. Not established, and now known to be unreachable this way: that a mod can
be the ONLY waker, because nothing it offers proves a FUTURE delivery. Not solved: the
process leak.

**What the prototype's clean behaviour does and does not cover.** No duplicate and no
orphan was observed across one idle wake, five sequential exchanges, and four sends
made into a live park. That is a prototype under a driver, not production event
traffic: `.clank/` churns on every event append, the debounce described above was
never built, and `FileChanged` under real churn is untested. The measurements bound
what a quiet session does, not what a busy repo does.

## Constraints

- **Never risk zero wakers.** Any design whose failure mode is a session that hears
  nothing is rejected, however rare. Duplicates are acceptable; silence is not.
- **The other three tools keep the external hook.** codex, grok and opencode are not in
  scope and must not regress. codex leaks too (`2x fs-war-room codex`); that is a
  separate plan, not this one.
- Do not delete the existing Stop-hook path. Claude mods are early access; the process
  path stays until the mod is proven, and a machine without function hooks must keep
  working exactly as it does today.

## Done when

- The questions above are answered from the type declarations, in writing, with the
  answers in the repo rather than in a commit message.
- Either a working mod that delivers clank work to a live Claude session without a
  parked process, with the existing path intact as the fallback — or a recorded finding
  that says which question defeated it and what would have to change.
- If it works: evidence that the leak is gone. Count parked waits, converse for several
  exchanges, count again, and show it flat.

## Out of scope

Removing the Stop hook, the other three tools, the `--force` squash request (queued
separately), and rewriting clank's wait/decision logic. This plan changes how work
REACHES a Claude session and nothing about what that work is.
