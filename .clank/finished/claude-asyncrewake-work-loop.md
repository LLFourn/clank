# claude-asyncrewake-work-loop
# Claude work loop on asyncRewake: kill the armed wait, kill the nudges

## Why

Claude Code's background-task manager kills tracked background tasks
(exit 144, silent, zero output) on its own lifecycle boundaries —
session resume, restarts, the daily auto-update storm — and clank's
claude loop is built ON tracked background tasks (the armed `clank
wait`). Every kill costs a nudge→re-arm cycle: a full model turn per
auto-on claude session, multiplied across sessions = rate-limit
burn ("too many requests"), observed live through 2.1.221–223.

Claude Code hooks now provide a channel the reaper cannot touch:
hooks are UNTRACKED separate processes, and Stop hooks support
`asyncRewake` — run in background WITHOUT blocking the UI; on exit
code 2 the session WAKES with the hook's stderr as a system
reminder. That is the codex-shaped in-hook long-poll, non-blocking,
with a native wake channel. The armed wait and the entire nudge
protocol become unnecessary.

## M0 — SPIKE: verify asyncRewake live (gates everything)

Probe with a scratch hook + scratch session before building:
- Minimum Claude Code version carrying `asyncRewake` (docs list it;
  find where it landed) and behavior on versions without it
  (unknown field ignored → hook silently sync? MUST fail safe).
- The wake: exit 2 + stderr → system reminder text, verbatim? Size
  limits? Does the model reliably act on it?
- Non-blocking claim: turn ends cleanly while the hook parks; the
  statusMessage spinner shows; user input during the park is
  unaffected.
- Parallel spawning: a second turn-end while a prior asyncRewake
  hook still parks — does a second instance spawn? (Expect yes:
  hooks fire per turn-end.) Measure, then design the guard.
- Lifecycle: does the parked hook survive session restart? (Expect
  orphaning — fine, see SessionStart catch-up; verify it doesn't
  double-wake a resumed session.)
- **Waiter ownership across restart** (intro 0d826d6): a parked
  waiter is untracked, so it can SURVIVE the session it belongs to.
  The spike must answer: does a surviving waiter's exit-2 wake reach
  a RESUMED session incarnation at all? This BRANCHES the design:
  - If yes (claude accepts wakes from pre-restart hook processes),
    the surviving waiter may keep ownership and the per-session
    guard is just its lock.
  - If no, ownership is GENERATIONAL: each session incarnation mints
    a generation (SessionStart writes it beside the agent skeleton);
    a waiter records its generation; a hook from a newer generation
    revokes the stale waiter (takeover), and a stale waiter checks
    the generation before exiting-2, exiting 0 silently instead — no
    wake aimed at a dead incarnation, no lock wedged shut against
    the resumed session, and SessionStart's peek stays able to
    deliver later work.
- **NO-GO: if asyncRewake is unreliable (wakes lost, UI blocked,
  hooks reaped like tasks), block the plan and stay on the armed
  wait.**

## M0 FINDINGS (recorded 2026-08-08 — live on Claude Code 2.1.223)

The core wire is GO; the ownership branch is decided by anatomy:

- **The wake wire, end to end (headless `-p`)**: turn completes →
  the asyncRewake Stop hook parks WITHOUT blocking the turn → on
  exit 2 its stderr reaches the model as a system reminder — the
  model ACTED on it verbatim (replied the requested marker) — and
  the wake turn's end fires a FRESH Stop hook whose quiet exit 0
  ends the cycle cleanly.
- **Empty-is-quiescent is load-bearing**: the unconditional-exit-2
  variant looped forever (wake → turn → hook → wake …), the same
  idle-loop failure the opencode plugin guards against. The
  production hook's no-work arm MUST exit 0 silent.
- **Ownership branch RESOLVED without a restart probe**: the wake
  transport is the hook's stderr pipe to ITS OWN claude process. An
  orphaned waiter's exit-2 goes to a dead parent's pipe — it cannot
  wake a resumed incarnation BY CONSTRUCTION. The generational
  design is selected: SessionStart mints, newer generations take
  over the lock, stale waiters check generation and exit 0.
- Schema notes: `asyncRewake` implies `async`; internal fields
  exist for a custom reminder prefix and a user-facing wake summary
  (marked @internal — do not depend on them). 2.1.223 is the
  known-good floor for setup's capability probe; the exact landing
  version can refine it later.
- `-p` holds the process open while an async hook runs (fine —
  production panes are long-lived interactive sessions).
- Not probed live (accepted residual risk, M1 keeps them visible):
  TUI keystroke responsiveness during a park (docs assert
  non-blocking; statusMessage makes a stuck park visible), and true
  concurrent-spawn overlap (certain by design; the per-session
  generation lock is the guard regardless).

## M1 — the loop

- stop_hook's claude arm: `LoopPolicy::BackgroundArm` →
  a new `AsyncRewakeWait`: run the SHARED codex/opencode long-poll
  (`compute_wait_outcome`); work → exit 2, items on stderr
  (formatted as today's wake text); quiet timeout → exit 0 silent.
  The nudge machinery (arming reminders, chained-wait detection)
  is deleted for claude, not bypassed.
- ONE PARKED WAIT PER SESSION: a per-session flock (keyed by
  session id, beside the agent skeleton) — a second hook instance
  exits 0 immediately when the lock is held. The ingest lease
  already makes overlapping waits WAL-safe; this guard is about not
  stacking N parked processes and N duplicate wakes.
- **The delivery mode is ONE durable setup-time decision** (intro
  0d826d6): claude interprets `asyncRewake` from the static hook
  entry while the hook binary could otherwise re-decide at runtime —
  drift turns the 86400s poll into a SYNCHRONOUS Stop hook (a
  blocked UI for a day). So setup probes the installed Claude Code
  once and writes EITHER the async entry (`asyncRewake: true`,
  `statusMessage`, timeout 86400, command
  `clank stop-hook --tool claude --loop asyncrewake`) OR the legacy
  entry (today's command, no marker). The hook keys its behavior off
  ITS OWN argv — never off a runtime version probe — so the entry
  and the behavior cannot disagree. doctor compares the installed
  entry's mode against the CURRENT claude capability and says "run
  clank setup" when stale (upgrade or downgrade); it never silently
  flips.
- SessionStart hook (matcher: startup/resume/clear): non-blocking
  peek (`clank wait --peek` equivalent) → pending items via
  `additionalContext`, closing the restart stranding gap codex
  has. No long-poll here — SessionStart must be fast.

## M2 FINDINGS (recorded 2026-08-08 — live e2e on 2.1.223)

The PRODUCTION loop verified end to end in a scratch clank repo
(project-scoped async entries, debug binary, headless session):
bind mid-turn → the park takes the lease (wait.holder/wait.lock on
disk) → the staged promote item arrives as the exit-2 wake
("Clank wait returned work for `tester` (master). Items: …") → the
model acts on the reminder → an unhandled item RE-PARKS and
re-presents on the next idle (at-least-once held; a real agent's
action clears it). Zero armed background tasks anywhere.

One gap surfaced and closed: SessionStart fires BEFORE the session
is bound, so its fail-open identity check bails and it cannot mint
for a FRESH session — a dead predecessor's waiter would have held
the lease at the same generation forever. Binding is the ownership
claim: `clank as` now mints the generation too (bind covers fresh
sessions, SessionStart covers resumes).

## M2 — skills, docs, teardown

- WORK_LOOP_CLAUDE rewrites to the opencode shape: work finds you
  (the hook wakes you; NEVER arm `clank wait` yourself, never
  poll). The arm/re-arm teaching and the Stop-hook-nudge language
  go. Reviewer + master skills, README loop table, doctor section.
- Remove the nudge arm from the hook only behind the version gate;
  the fallback path keeps the old skill text via the same
  substitution the per-tool work loops already use.

## Acceptance

- With asyncRewake active: an auto-on claude agent receives work
  as a system-reminder wake with ZERO armed background tasks and
  ZERO nudge turns (the stop-hook unit surface pins the exit-2 +
  stderr wire; the parked-lock discipline has deterministic
  tests).
- Restart takeover is deterministic: with a stale-generation waiter
  parked, a new incarnation's hook takes ownership (or the proven
  surviving-waiter branch holds it legitimately), the stale waiter
  never emits a wake (suppression pinned at its generation-check
  seam), and no scenario double-wakes one session for one item.
- Mode drift is impossible by construction (the hook obeys its
  argv) and VISIBLE when installed: doctor flags an entry whose
  mode mismatches the current claude capability in both directions.
- Old Claude Code → byte-identical current behavior, doctor names
  the version gate.
- SessionStart catch-up delivers pending items on resume (pinned
  at the hook-output seam).
- Live spike findings recorded in the plan before M1 lands.
