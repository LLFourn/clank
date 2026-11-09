# all-background-work-is-attended
# Attending is explicit for every agent: clank never detects background work, and agents mark all of it

## Why

On 2026-11-09 the life repo's `assistent` (Codex, standby) sat "working" mid-turn on a background task. A message LLFourn sent it waited, because clank delivers between turns.

clank's attendance is built on detection: each harness's Stop hook reports `background_tasks` (`hook_io.rs:61-67`), and `turn_policy` reconciles that list with what the agent attends. That design has three problems:
- **It only covers some harnesses.** Claude and Grok send a list; Codex and OpenCode don't, so their background work is invisible.
- **It speaks as if it saw everything.** The `Uncovered` notice ("You have live background work you are not attending…") and the matched-no-live-work / ambiguous notices suggest complete knowledge.
- **It has a cost in this session.** It woke the master repeatedly over a subagent's own shells.

LLFourn: "there should be no 'background work' detecting in ANY kind of agent. Purge it all away. We do explicit attending now."

## Build

### 1. Remove detection

- **Hook payloads.** Stop reading `background_tasks` from every hook payload (`clank_core::hook_io`, `grok_hook.rs`), and delete `BackgroundTask` and its fixtures. Unknown fields stay tolerated, so newer harness payloads still parse.
- **`turn_policy`.** Remove everything that reasons from a task listing:
  - the `Uncovered` demand and its notice;
  - resolving attend intents against a listing ("until a listing shows exactly one live match");
  - the "known listing no longer has the task" ending;
  - the `Ambiguous` and matched-no-live-work notices;
  - the stored "latest report's listing".
- **The monitor and recovery.** Recovery and monitor code that reads the listing (`monitor.rs`, around 8672) goes too. Each removal must leave the state machine simpler, not patched: delete the listing-shaped states rather than leaving them unreachable.

### 2. Attending is only what the agent declares

**`clank run --desc "…" -- <cmd>`** runs shell work, and keeps today's single-process model (`run.rs:3-13`). It registers the run with the monitor, then `exec`s the command. The registered PID, with its birth token, is the work process itself. There is no wrapper, so a wrapper can't die while its child keeps going. stdio, exit status and signals stay the command's own, and the agent reads output and status where it already does, from whatever launched the run (its shell or background terminal). clank doesn't capture output and doesn't claim an exit status, because nothing waits on the process.

- **One terminal outcome.** A run ends exactly once, when its registered process is gone: the PID is dead, or now carries another birth. The monitor writes one durable terminal record for that run ID, `run_ended { desc, started, ended_seen }`, and delivers a single `run_ended` wake item from it through the existing at-least-once inbox until ack. There is no finish report, so there's no report-versus-disappearance race. Replace `forget_gone_requests`'s silent drop of a vanished, unresolved run with this record.
- **Restart.** Run registrations are already durable. A monitor that restarts re-checks each registered run's PID and birth, and writes the terminal record for any that are gone. Writing it is idempotent per run ID, so a run can never produce two different completions.
- **Cancellation.** Killing the command is its end, and gives the same `run_ended`.
- **No monitor.** When no monitor accepts the registration (`run.rs` `launch`, the `NotTaken` branch), `clank run` refuses to run the command and says why. Today it runs it unattended, and that can't satisfy the completion wake. The agent can retry, or run the work some other way knowing it isn't attended.
- **While it runs**, the agent is attending it, so already-shown work doesn't re-wake the agent.

**`clank attend "<name>" [--expect D]`** declares work clank can't observe, such as a subagent. It's a turn-scoped declaration with a check-in deadline, and it promises nothing about completion:
- It takes effect when declared. Nothing resolves it against a listing.
- It ends when the agent's next turn starts, whatever woke the agent (a message, a check-in or the harness), or at `--clear`. clank never claims the named work finished.
- At `--expect`, if the declaration is still standing, the agent is checked in on once.
- Whether the work's own completion wakes the agent is up to its harness. Claude re-fires Stop when a subagent finishes, and `completion_wake` already models this for Claude only. clank doesn't promise it for any tool.
- An agent woken for anything while named work is still running re-declares that work before ending the turn. The skill says so.

**Nothing else counts as background work, for any tool.**

### 2a. Grok's waiter evidence comes from the monitor, not a task list

`grok_hook.rs` (about lines 30 and 85-99) uses `background_tasks` to tell whether the agent's one bare background `clank wait` is running, and `Handoff.wait_running` decides whether to tell the agent to arm one. Replace that evidence with the monitor's own record of registered waiters, which already exists for delivery:
- **Running:** a waiter for this session counts as running while its connection to the monitor is live.
- **Gone:** a waiter that exits drops that connection, so the next Stop says to arm one.
- **Race:** a waiter that has started but not registered yet is the start/register race. Use the existing registration order: the wait registers before it parks. If that leaves a window, either the Stop verdict asks the monitor after the registration point or treats a just-armed (same-turn) wait as pending. Say which, and test it.
- **Duplicates:** duplicate arming stays refused by the existing one-wait-per-session holder.

The waiter stays a single bare background `clank wait`. It isn't wrapped in `clank run`, and handoff keys or generations aren't reintroduced. This is control-protocol bookkeeping, not detection of agent work.

### 3. The skill states the rule

In `setup_assets/skill_shared_core.md`, which every role and tool gets, replace the "Long background work" bullet with:

- All work that outlives a command (builds, test runs, servers, subagents) is started through `clank run` or declared with `clank attend`. There are no exceptions, for any tool.
- Never wait for background work inside a turn. Start it through clank and end the turn. A `clank run` wakes you once when its process ends; read its output and exit status where you launched it. A `clank attend` declaration only lasts until your next turn and checks in at `--expect`, so re-declare any named work that is still running before you end a turn. Don't poll it and don't start a wait.
- clank doesn't detect background work. What you don't mark isn't tracked, and messages wait until your turn ends.

Keep the `--expect` and check-in facts. For Codex and OpenCode, confirm how the agent runs `clank run` without blocking its own turn, and put that exact form in its text. If one has no non-blocking way, report it rather than writing an instruction it can't follow. Make sure `clank setup` (or the upgrade path) refreshes installed skills, and say how existing installs get the new text.

## Out of scope

Inferring background work from process trees or harness logs.

## Tests

- **Hook payloads:** a Stop hook payload that includes `background_tasks` parses, and the field has no effect on any decision.
- **The monitor:**
  - A turn end with no attends produces no background-work notice for any tool.
  - A `clank run` in flight keeps the agent attending.
  - When its registered process is gone (by PID death, or a reused PID with another birth), exactly one `run_ended` item is delivered. It isn't repeated after ack, and no task listing is involved.
  - A monitor that restarts after the run ended writes the record once and delivers it. A second restart writes no duplicate.
  - A killed run gives the same single `run_ended`.
  - When no monitor accepts the registration, `clank run` refuses and runs nothing.
  - A `clank attend` takes effect immediately and is checked in on once at `--expect`.
  - A message wake with two standing declarations ends both. Neither is reported as finished.
  - Re-declaring after that wake restores them.
- **Grok:** with `backgroundTasks` absent, a registered waiter alone gives the "wait is armed" verdict. A waiter that exits gives "arm one". Cover the start/register race chosen in 2a, and a duplicate arm being refused.
- **The skill:** rendered for each tool, it contains the rule, and no tool's text promises a wake-up for unmarked work.
- **Deleted tests:** tests that existed only to exercise listing reconciliation are deleted, not adapted. Tests of explicit attending are kept and adapted.

## Done when

- No code path reads a harness's background-task list.
- Attending comes only from `clank run` and `clank attend`.
- Every installed skill tells agents to mark all background work and never to wait on it in-turn.

## As landed

- **Detection removed.** `background_tasks` and `BackgroundTask` are gone from `hook_io`, along with the stop-hook fixtures, Grok's `backgroundTasks` reader and every listing-shaped `turn_policy` state and notice. That includes `Uncovered`, `Ambiguous`, NoMatch, ShellNamed, `resolve_requests`, `forget_gone_requests`, `completion_wake` and `DescInUse`. A payload carrying `background_tasks` still parses and has no effect.
- **`clank run`.**
  - It keeps its exec model, and is accepted only once its registration is durably written. A failed write rolls back and refuses, so the command never runs.
  - When its process is gone (verified death, or a different birth), the run writes one `RunEnded` item. Its key is pinned in the message log (`run-ended#`), so a replay after a crash can't write a second one past the identity horizon.
  - Unknown identity (a denied read, an unsupported platform, a malformed or unrecognised token) keeps the registration and writes nothing.
  - The owner beat looks for ended runs, so a restarted monitor finds runs that ended while it was down.
  - With no monitor, `clank run` refuses.
- **`clank attend`.** It's a turn-scoped declaration, ending at the agent's next turn start (a delivery, a later Stop or Grok's native turn end) or at `--clear`. It gets one check-in at `--expect`, and it never reports completion.
- **Grok.** Its armed-wait verdict comes from the monitor's registered waiter. A Stop that continues the turn that was just asked to arm treats the wait as pending.
- **Skill.** Every tool gets the rule, each with its own non-blocking run form:
  - Claude: `run_in_background`.
  - Codex: `exec_command` with a short `yield_time_ms` (this only exists on Codex builds offering that tool).
  - Grok: `block_until_ms: 0`.
  - OpenCode: `shell` with `background: true`.

  The Codex reviewer skill's size cap was raised to 6,150 bytes. Existing installs pick up the new text through `cargo install` and then `clank setup`.
- **Wire and state.** The wire protocol goes from 19 to 20. Old turn state migrates, fixtured in `turn-listed-attendances.json`.
