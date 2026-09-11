# opencode-wake-bootstraps-and-surfaces

## Why

The opencode wake loop works only once it is running. Its one arming
trigger is `session.idle` (setup_assets/opencode_plugin.js), which
fires only when a turn COMPLETES. A session that has not produced a
turn — resumed, restarted, never prompted — never idles, so no
`clank stop-hook` wait is ever spawned, no work is delivered, and no
turn is produced. It wedges by construction.

Observed end to end in `~/src/penlock-experiment`. kimi was swapped in
as a commit reviewer with work pending (`label-grid-pass`, gate waiting
on kimi). The fresh launch ran its bind prompt once (Tuesday 02:11Z,
session `ses_f813…`), then never turned again. Two resume launches the
following Friday (pids 5402, 96390) each booted, loaded all five
plugins (`init count=5` in their runs), and produced zero session
activity — no stream, no loop, no idle — while the gate sat waiting.
No `clank wait` process ever existed. Claude solved this same class
with SessionStart catch-up; the opencode plugin has no equivalent.

Two more holes on the same path:

- **Diagnostics are invisible AND, worse, would loop if injected.**
  The hook's contract is Diagnostic = exit 0 + a stderr line ("a
  non-zero exit from this binary is a clank bug"). The plugin reads
  only stdout on exit 0, so Diagnostics — including "cannot resolve
  role; arming no wait", which exists specifically to be seen — are
  swallowed. Injecting them through `promptAsync` is NOT the fix:
  that creates a paid model turn whose idle re-arms the same
  persistent diagnostic, a loop that bills.
- **Every failure reads as "no work".** The plugin has no logging
  (`nothrow()`, non-zero exits ignored, nothing anywhere). The
  penlock case took archaeology through a shared log file; one log
  line per arm would have answered it.

## Approach

Changes are in `crates/cli/src/cli/setup_assets/opencode_plugin.js`
(installed by `clank setup` to `~/.config/opencode/plugin/clank.js`)
and, for the bootstrap env var, the opencode launch composition in
`crates/cli/src/cli/agent.rs` (`compose_launch` /
`compose_fork_launch`). Tested headlessly through
`crates/cli/tests/opencode_plugin_lifecycle.mjs`, with the argv side
pinned in the existing compose tests.

1. **Bootstrap at plugin LOAD, for the ONE session this process
   owns.** No event-dependent fallback can cover the incident: a
   resumed session that emits nothing gives a handler nothing to
   sight (codex on 42a2242). But a directory-wide
   `client.session.list()` scan is the wrong fix for it (codex on
   9a558d1): the plugin cannot tell which session its own process
   hosts from the list, so every process would arm waits for EVERY
   bound session in the repo — each opencode agent running stop
   hooks and `promptAsync` into the others' sessions, and in the
   three-process repro all three bootstrapping the same binding.
   Missing wake becomes cross-session wake ownership.

   The launch environment already carries the one identity that is
   known: on a RESUMED opencode launch, `compose_launch` puts
   `--session <id>` in the argv, so clank knows the exact session id
   at composition time. Pass it to the process as a dedicated env var
   (`CLANK_BOOTSTRAP_SESSION_ID`) in the composed launch env, and
   have plugin init arm ONE guarded wait for that identity alone —
   the same inflight/stale/quiescent rules as the idle path. The
   invariant is one process, one owned session: no process arms or
   injects for any other.

   The var is an OWNERSHIP TOKEN, and it gets the same hygiene as the
   existing identity vars (codex on 0c2cf99): a resumed process
   exports it, its tool shells inherit it, and a nested fresh/fork
   launch would otherwise bootstrap the PARENT's session. Add it to
   `agent_env::SESSION_IDENTITY_VARS` and to the plugin's
   `FOREIGN_IDENTITY_VARS` blanking set, so a fresh/forked child
   never inherits a bootstrap identity.

   The load-time arm is DETACHED: plugin construction returns its
   hooks immediately, with the bootstrap wait spawned fire-and-forget
   — never awaited, since the stop-hook long-polls and opencode
   startup must not block on it.

   Fresh and forked launches set NO var: opencode mints the session
   id only when the session is created, so it cannot be known at
   composition. Those keep today's path — the bootstrap prompt drives
   the first turn, whose terminal idle arms the wait (proven
   working). The no-token fallback is IDLE-VERIFIED, not
   first-sighting: an arbitrary first event is not an idle signal
   and can arrive mid-turn (codex on 0c2cf99), so a session with no
   token and no armed wait arms only after an explicit
   `client.session.status()` check returns `{type:"idle"}` (the SDK
   carries the state). A zero-event hand resume has neither token nor
   sighting and stays a noted gap, not a wrong arm.

2. **Surface diagnostics on a NON-model channel.** Stop-hook exit 0
   with empty stdout and NON-empty stderr is a Diagnostic. Show it as
   a TUI toast (`/tui/show-toast` is in the SDK surface) and record it
   with `client.app.log` — never through `promptAsync`, which would
   spend a model turn and re-arm the same diagnostic on that turn's
   idle. `promptAsync` stays reserved for real continuation stdout.

3. **Log the loop.** One `client.app.log()` line per arm, per
   injection, per stale discard, per spawn failure / non-zero exit.
   Structured, one line each — the next wedge should be diagnosable
   from the log the plugin already has.

**ESC handling is NOT in this plan.** It shipped in fffc79f
(an-aborted-turn-is-not-a-finished-turn): the plugin latches the
interrupted turn's assistant-message `MessageAbortedError` until the
next user message, and it works — verified live twice this week
(aborts recorded in the shared store with exactly the matched shape,
suppressed). See Out of scope for the hole that remains.

## Required tests

Extended through the existing lifecycle harness (fake client/`$`, no
live opencode, no model), plus a fake `client.session.list()` and
`client.tui.showToast`:

- Plugin init with `CLANK_BOOTSTRAP_SESSION_ID` set arms ONE wait
  for that session, and pending work is delivered without any turn
  having completed — the penlock case.
- **Multi-session ownership:** with two bound sessions in the repo,
  plugin init with the var set for session A arms for A only — it
  never arms a wait for, and never calls `promptAsync` into, session
  B.
- Init with the var UNSET arms nothing blindly: a first sighting
  arms only after a faked `session.status()` returns
  `{type:"idle"}`; a busy status arms nothing.
- A pending (never-resolving) fake wait proves plugin CONSTRUCTION
  resolves immediately — the bootstrap arm is detached, never
  awaited.
- The opencode resume launch composition carries
  `CLANK_BOOTSTRAP_SESSION_ID=<id>`; fresh and fork compositions do
  not (the id is unknowable before opencode mints it) — asserted on
  the composed argv/env in the existing compose tests.
- Identity hygiene: the var is in `SESSION_IDENTITY_VARS` and the
  plugin's blanking set — a fresh/forked child process never
  inherits a parent's bootstrap id (composition + shell-env
  regression).
- Exit 0 + empty stdout + non-empty stderr produces a TOAST and a
  log entry, and NO `promptAsync` call. A repeated diagnostic still
  does not prompt the model and does not arm through its own
  presentation.
- The existing two-turn re-arm regression (codex d7c8908) and the
  ESC latch regression (fffc79f) still pass.
- `client.app.log` is called on arm, inject, discard.
- No live opencode, no agent binary.

## Out of scope

- **Duplicate processes on one session — the remaining ESC hole.**
  Reproduced live: `ses_f813…` had THREE opencode processes; the
  abort latch is per-process, so a sibling's armed wait injects
  after ESC no matter how the ESC'd process latches. Same root as
  the parked duplicate-pane bug. That needs cross-process ownership
  (generational, claude's answer to the same class) and its own
  plan — the repros above are recorded for it.
- **Why `opencode --session <id> --prompt …` did not run the prompt.**
  Fresh-launch `--prompt` runs; resume `--prompt` produced no turn.
  Upstream opencode behaviour; the load-time bootstrap makes wake
  delivery independent of it.
