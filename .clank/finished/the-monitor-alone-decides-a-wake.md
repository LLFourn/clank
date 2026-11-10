# the-monitor-alone-decides-a-wake
# Only the monitor decides whether an agent is woken; a waiter that loses its monitor gets another one and never decides for itself

First of four plans from one diagnosis (LLFourn, 2026-11-10: "I don't want temp fixes. What is the problem with our architecture … No refactor is too ambitious"). The others, in order: `undelivered-work-is-noticed`, `no-process-outlives-its-build`, `reachability-is-evidence-not-inference`.

## Why

On 2026-11-10, `artist4` (Codex, standby) in `~/src/safepay/.clank/worktrees/story-short` never received a message sent at 11:10 (inbox seq 50 in its `messages.jsonl`, with no presentation record after it). Traced live:

- **Its waiter had no monitor.** Its Stop hook's child (`clank wait … --turn-end`, pid 64025) had been parked since 9 Nov 16:30. Each `cargo install` since then retired the repo's monitor. The child follows a lost monitor once (`turn_child.rs:139`, `for _ in 0..2`), then falls to `alone()` and decides locally for the rest of its life. `lsof` showed no monitor socket, and `sample` showed it in the local watcher loop (`wait::Beat::bridge`).
- **The local decision is a second, different policy.** `alone()` accepts items only when `alone_verdict` (`turn_policy.rs:1714`) is not `Park`, and that function parks on `facts.asking || facts.blocked || facts.owed.is_empty()`. `facts.blocked` is `wait::check_blocks(..).suppress_all`, which is true when any agent in the repo has an unanswered block. The master's `stork-retakes` block had been pending since 9 Nov 13:30, so the waiter parked on an owed message indefinitely.
- **The monitor's own policy would have delivered it.** `present_with_blocks` (`wait.rs`) withholds only review-cycle items behind a block and still presents events such as messages.

So "should this agent be woken" has two owners with two answers. The fallback exists for a good reason, stated at `wait.rs` ("Sharing is an optimisation, never a dependency … a LOST WAKE"). But the cure for a lost monitor is another monitor, which any clank process can start, not a private copy of the policy.

## Build

1. **A turn-end child never decides.**
   - In `turn_child.rs`, replace the two-attempt loop and `alone()` with attach-or-spawn that repeats, with backoff, until it holds a monitor connection, the hook goes away (`Said::Gone`), or the report's deadline passes.
   - A monitor that dies mid-park is followed every time, not once.
   - Delete `alone()`, `turn_custody::Gathered::alone_verdict`, `turn_policy::alone_verdict`, `Notice::MonitorUnavailable` and `wait::items_alone` with its `Local.accept`, plus their tests.
   - Grok's handoff answer in `alone()` (`handoff_verdict` with no monitor) goes the same way: it's answered by a monitor.
2. **When no monitor can be had, wake the agent with the reason.** If attach-or-spawn keeps failing past a named, finite outage budget, the child emits a **waking notice that carries no work**.
   - **Framing:** use the existing `Notify` framing with a new notice that names the connection or startup failure ("clank cannot start its monitor: …"). It is not a `Diagnostic`: that verdict is non-waking by contract (`turn_wire.rs::Frame::outcome` maps it to `HookOutcome::Diagnostic`, which `stop_hook.rs::emit` writes to stderr with `HOOK_OK_EXIT` and `presented=false`, and `grok_hook.rs::wire` emits stderr only). Ordinary `Diagnostic` behaviour, for parse, setup and adapter failures, is unchanged.
   - **What it carries:** no presentation id, no work content, and it earns no receipt. Only a monitor can present work.
   - **Choosing the outcome:** use the typed `monitor::Attempt` taxonomy (`Newer`, `Theirs`, `Busy`, `SpawnFailed`, …) to tell a protocol refusal from a reconnectable failure. Don't parse `Attached::Alone`'s string.
   - **Once per outage.** The notice wakes an agent once for one outage, not at every Stop: a monitor that can't start would otherwise spend a model turn per agent per minute for as long as it lasts. The child records that this session was told (a typed marker in the agent's config, written under the binding lock, like `resume_pending`). While the marker stands, later children keep retrying the attach quietly for their whole park and end with the ordinary re-arm at the report deadline. The first monitor that answers clears the marker, so the next outage is announced again.
   - **A short deadline, for reports that don't park.** A report whose hook gives up at its deadline (Grok's handoff) uses the shorter of the outage budget and its own deadline less a margin, so the notice gets out before the hook frames a non-waking deadline. A report that parks doesn't need this: at its deadline it ends with the ordinary re-arm, with no notice and no marker, and the next child follows the monitor with a fresh budget. Otherwise a monitor retiring for an install seconds before a park's deadline would be announced as an outage. The spent-budget path notifies both kinds.
   - **Grok's own wait shares the marker.** Its background `clank wait` ends when the budget is spent, which wakes the agent, and the skill then re-arms it. A session already told keeps retrying quietly instead of ending.
   - **Composition:** the outage budget, the attach backoff and the shared wait all run inside the report's deadline and the owner's cancellation. When the hook is gone, or its binding has been superseded, the child still exits silently.
3. **A monitor of another build.** Until `no-process-outlives-its-build` lands, a child whose attach is refused because the monitor speaks another protocol can't follow it. It ends the park with one waking notice of the same no-work kind ("clank was upgraded; ending this wait so the next one uses the new build"), so the agent's next Stop runs the installed binary. That plan replaces this with a re-exec.
4. **Other waits.** Audit every caller of `items_with_policy` and of the local watcher loop, and list each in the report:
   - Grok's waiter already reconnects (`wait.rs`, "reconnecting Grok wait"); make sure it never folds locally either.
   - The audit covers the initial local fold that runs before `share()`, as well as the watcher fallback after a lost monitor.
   - A person's `clank wait` with invocation sources or `--for`, and `--peek`, may keep the local path: they serve no agent's wake. Make those exceptions explicit in the code, so an agent delivery can't select them by accident.
   - No agent wake may come from a local fold.

## Tests

No test starts a monitor, clank or a harness. Use the existing fake monitor boundary and a paused clock.

- **Following:** a monitor lost three times mid-park is followed three times; the child never folds locally.
- **The artist4 case:** an owed message, a pending block held by another agent, the monitor lost and replaced. The replacement delivers the message.
- **No monitor:** attach-or-spawn failing past the outage budget yields the no-work waking notice, not a park, not a `Diagnostic` and not a presentation.
- **Another protocol:** the refusal yields the one waking notice.
- **The notices really wake, through each adapter's own framing:** Claude's continuation exit, Codex's block JSON, Grok's block decision, and OpenCode's continuation or renewal contract. Each carries no presentation id and no work content, and earns no receipt. Asserting that the child returned a verdict isn't enough.
- **Once per outage:** a second and third Stop during one outage wake nobody and keep retrying. After a monitor answers, a new outage notifies again. The marker's old-format absence and its bytes are fixtured.
- **A short deadline:** a Grok handoff with no monitor gets its notice before the hook's own deadline.
- **A park's deadline is not an outage:** a parked report whose connection is lost just before its deadline re-arms, prints no notice and claims no marker.
- **Grok's wait in a long outage:** it ends once with the reason, and a session already told keeps retrying.
- **Ordinary diagnostics are unchanged:** a parse or setup failure still doesn't wake.
- **Hook gone or binding superseded:** the child exits without output.
- **Deadline and cancellation:** the report's deadline ends a retry loop that is still failing, and the owner's EOF ends it at once.
- **Mutation:** the old body (two attempts, then `alone`) pasted back fails the following test and the artist4 test.
- **Source gate:** nothing under the agent wake path calls a local fold (extend an existing boundary gate if cheap).

## Done when

A parked agent's wake is decided by a monitor or not at all: no code path lets a waiter choose to stay parked on owed work, and a waiter with no monitor either gets one or wakes its agent with the reason.

## As landed

- **No agent wake folds locally.**
  - `turn_child::carry` attaches or spawns until a monitor answers.
  - `wait::monitored` does the same for a plain agent `clank wait` and for Grok's waiter. A plain wait with no `--event`, `--for` or `--peek` is now monitor-only.
  - Deleted: `alone()`, `alone_verdict` (core and custody), `Notice::MonitorUnavailable`, `items_alone`, `Local`/`Accept`, `share`, `shared_wait`.
  - `OwnFold { Peek, InvocationSources }` is the only way into the local fold, and source gates pin that.
- **Typed outcomes.** `monitor::Attempt::served()` gives `Upgraded` for `Newer` and `Retry` for the rest. Nothing parses a string.
- **Notices.** Each is a `Notify` with no presentation and no receipt:
  - `MonitorOutage { why }`: "clank cannot start its monitor: …";
  - `MonitorUnanswered`: "clank's monitor did not answer in time";
  - `Upgraded`.

  Ordinary `Diagnostic` behaviour is unchanged.
- **Budgets.**
  - `monitor::OUTAGE_BUDGET` is 60s. The first loss after a monitor answered retries at once, then backs off from 0.5s to 8s.
  - A report that never parks gives its last word at its deadline less `turn_child::DEADLINE_MARGIN` (2s).
  - A parked report at its deadline re-arms with no notice and no marker.
  - The report's deadline covers the connected exchange as well as the attach.
- **Once per outage.** `agent_store` owns the marker (`AgentConfig.outage_notified`, absent from a config never told):
  - `announce_outage` checks the binding and the marker, delivers and records, all under the binding lock, and records only a delivered notice;
  - `outage_served` compares and clears under the same lock.

  A hook report is served by a verdict. A background subscription is served by its first note after admission.
- **Tests deleted.** Two integration tests are gone with the alone mode and the zero-source local wait. Others add `common::idle_source()` to stay on the retained invocation-source path.
- **Known gap,** quoted into `reachability-is-evidence-not-inference`: a Grok agent told of an outage has no wait armed, and nothing re-arms it when the monitor returns.
