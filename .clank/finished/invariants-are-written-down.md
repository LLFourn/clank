# invariants-are-written-down

## Why

Plans keep undoing each other because the invariants they rely on live only in plan prose and chat:

- `grok-handoff-survives-a-slow-monitor` was undone by the next plan, `the-monitor-resolves-grok-continuations`.
- `clank open` on an unreadable ancestry went refuse → refuse-in-zellij-only → never refuse in three commits.
- The run-ended wake was added, removed on the premise "the harness tells you", and that premise turned out false for Codex.

Each plan was reviewed against its own goal, so nothing caught the reversal.

This plan writes the registry down and gates its test references. It changes no runtime behaviour. Where an invariant does not hold today, the registry says so and names the queued plan that makes it hold.

## Build

1. `docs/invariants.md`. One entry per invariant, each with:
   - a stable id (`I1`…)
   - one sentence
   - why, in one line
   - a status:
     - **holds**: true of the code today, naming the test(s) that fail when it breaks, by path::name;
     - **holds, untested**: true today, naming the queued plan that adds its test;
     - **target**: not true today, naming the queued plan that makes it hold, and saying what is true today instead.

   A target is never written as present behaviour.
2. The initial list. Each entry's status is determined from the code when the doc is written; the expected statuses are given here.
   - **I1, target** (`codex-wakes-when-its-declared-work-ends`). Clank never parks an agent while its harness says work is in flight. Where the harness cannot say, work the agent declared (`clank run`/`clank attend`) wakes it when it ends.
     - Today: the yield on a listed background task holds for Claude and Grok (`turn_policy` task snapshot).
     - Today: a declared run ending only retires its attendance (`turn_policy.rs::end_gone_runs`). It wakes nobody, so a parked Codex agent is not told.
   - **I2, target** (`blocks-have-no-migration`). Reading durable state has no side effects.
     - Today: `block::lock()` runs the legacy migration on every read.
   - **I3, target** (`block-answers-are-best-effort`, `one-state-file-per-agent`). Inbox events are at-least-once until acked; everything else is best effort, with no per-session delivery record. Inbox events here means GitHub, command, timer and message sources, re-presented until `clank events ack`.
     - Today: `presentation::Receipts` (`delivered.json`) records per-session delivery and the monitor reads it (`delivered_to`).
     - Today: block answers carry a per-session `received` record.
     - The doc distinguishes three things: a transport attempt, presentation to a session, and acknowledgement.
   - **I4, holds**. A hook acts on its stdin, the binding it routes by, and the evidence the hook audit allows: the agent's transcript, process identity and upgrade state. It never reads monitor-owned policy or configuration.
     - Today's enforcement is a substring gate (`hook_is_a_messenger`) that a sibling function evades. `hooks-read-only-what-they-are-given` replaces it with an allowlist.
   - **I5**. Attendance is only what an agent declares.
   - **I6, target** (`one-state-file-per-agent`, `a-watch-is-a-typed-producer`, `an-unreadable-ancestor-is-outside-zellij`). Each fact has one stored copy, and nothing reconciles two copies. The doc names today's known duplicates.
   - **I7, target** (`only-the-master-raises-blocks`). Only the master raises blocks.
   - **I8, holds, untested** (`wire-changes-bump-the-protocol`). Any change to a monitor wire message bumps the protocol version. The doc notes the current violation (`Control::Questions` at 23).
   - **I9, target** (`the-monitor-loop-never-waits-on-an-agent-lock`). The monitor's serve loop never blocks on a lock an agent process can hold.
   - **I10**. No polling, except GitHub.
   - **I11, target** (`an-unreadable-ancestor-is-outside-zellij`). `clank open` works when no zellij session exists anywhere, and inside a pane it never nests a session.
   - **I12**. The zellij session a process belongs to is read from its process ancestry, never from inherited environment.
   - **I13**. Every invariant has a test that fails when it breaks. Weakening or deleting such a test needs the user's sign-off.
3. A gate module in the `it` harness. It parses `docs/invariants.md` and fails if any test named under a **holds** entry does not exist, or a named queued plan is neither queued, active nor finished.
   - The doc states the gate's limit: it proves a referenced test exists, not that the test is enabled or that its assertions protect the invariant.
   - Behavioural relevance is the reviewers' job (step 4).
4. Skills:
   - The master skill's plan-authoring section requires each plan to name the invariants it touches, and to update the registry entry when it changes one's status.
   - The reviewer skill tells reviewers to check a commit against the invariants its plan names and any it plainly touches. They also check that each test the registry names really fails when its invariant breaks.

## Acceptance

- `docs/invariants.md` exists with the list above. Each status is verified against the code; any whose status differs from the expectation above is noted in the commit message.
- The gate fails when a listed test is renamed, and when a named plan does not exist. Mutation-check both.
- Both skill texts are updated and their README tests pass.
