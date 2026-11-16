# Invariants

The rules clank's design rests on. A plan names the invariants it touches,
and a plan that changes one's status updates its entry here in the same
commit.

Each entry has a status:

- **holds**: true of the code today. `Tests` names tests that fail when it
  breaks.
- **holds untested**: true today. `Plans` names the queued plan that adds its
  test.
- **target**: not true today. `Today` says what is true instead, and `Plans`
  names the queued plans that make it hold.

The `invariants_registry` gate (`crates/cli/tests/it/invariants_registry.rs`)
checks that every named test exists and that every named plan is active or
queued. A plan that has finished must no longer be named: its entry's status
changed when it landed. The queue is the master's private, untracked state:
a checkout without one (CI) cannot see queued plans, so there the gate refuses
only finished ones, and a mistyped plan is caught where the registry is
edited. The gate cannot tell whether a test is enabled or
whether its assertions protect the invariant. Reviewers check that.

## I1. A parked agent is told when a `clank run` it started ends.

- Why: an agent parked on a run it is never told about is stranded.
- Status: holds
- Scope: Claude and Grok tell their agent themselves, so clank sends nothing.
  Codex holds the turn in its synchronous Stop hook and never continues the
  agent when a background command exits; OpenCode shows no sign of doing so.
  On those, clank holds a run-ended notice for the session that started the
  run until its next Stop that may wake (`Tool::reports_background_completion`).
  The notice claims no exit status. A bare `clank attend` has no process to
  watch: it stands until cleared and checks in at `--expect`. Background work
  started without `clank run` never wakes a Codex or OpenCode agent.
- Tests: `crates/core/src/turn_policy/tests.rs::a_codex_run_ending_wakes_its_parked_agent_once`,
  `crates/core/src/turn_policy/tests.rs::a_codex_run_ending_mid_turn_is_told_at_the_next_stop`,
  `crates/core/src/turn_policy/tests.rs::a_claude_run_ending_is_left_to_the_harness`,
  `crates/core/src/turn_policy/tests.rs::a_run_going_mid_retirement_is_never_retired_without_its_notice`,
  `crates/cli/src/cli/monitor_turn_tests.rs::a_codex_run_ending_wakes_its_parked_agent`,
  `crates/cli/src/cli/monitor_turn_tests.rs::a_codex_run_ending_is_never_told_to_a_rebound_session`

## I2. Reading durable state has no side effects.

- Why: a read that writes can wedge every reader on one bad file.
- Status: target
- Today: `block::lock()` runs the legacy block migration on every read.
- Plans: blocks-have-no-migration

## I3. Inbox events are at-least-once until acked; everything else is best effort.

- Why: per-session delivery records replay work after every rebind.
- Status: target
- Today: GitHub, command, timer and message events are re-presented until
  `clank events ack`, which is intended. Separately,
  `presentation::Receipts` (`delivered.json`) records which items reached
  which session, the monitor reads it (`delivered_to`), and a block answer is
  recorded as received per session.
- Terms: a transport attempt is a try to put an item in front of the model;
  a presentation is the model being shown it; an acknowledgement is the agent
  saying it is handled. Only inbox events have acknowledgements.
- Plans: block-answers-are-best-effort, one-state-file-per-agent

## I4. A hook acts only on what it is given.

- Why: hooks must not read configuration owned by other components; the
  monitor decides.
- Status: holds
- Scope: a hook uses its stdin, the binding it routes by, and the evidence the
  hook audit allows (the agent's transcript, process identity and upgrade
  state). It never reads monitor-owned policy or configuration.
- Tests: `crates/cli/tests/it/hook_is_a_messenger.rs::hook_modules_touch_no_decision_state`,
  `crates/cli/tests/it/hook_is_a_messenger.rs::shared_hook_identity_paths_cannot_parse_monitor_policy`
- Today: the gate is a substring list that a sibling function can evade.
- Plans: hooks-read-only-what-they-are-given

## I5. Attendance is only what an agent declares.

- Why: clank detects no background work; guessing at it brought back wakes
  nobody asked for.
- Status: holds
- Tests: `crates/core/src/turn_policy/tests.rs::a_listed_task_nobody_declared_is_ignored`

## I6. Each fact has one stored copy.

- Why: copies drift, and the code that reconciles them is where the bugs
  live.
- Status: target
- Today: an agent's session is stored in `config.json`, `turn.json`,
  `state.json` and `delivered.json`; `state.json` repeats its role; watch
  identity is re-parsed from strings; reopen availability is kept beside the
  zellij reach it should derive from.
- Plans: one-state-file-per-agent, a-watch-is-a-typed-producer,
  an-unreadable-ancestor-is-outside-zellij

## I7. Only the master raises blocks.

- Why: a block pauses the whole repo; the master decides when to ask a person.
- Status: target
- Today: reviewer wake text suggests `clank block ask`, and `block ask`
  accepts any caller.
- Plans: only-the-master-raises-blocks

## I8. Any change to a monitor wire message bumps the protocol version.

- Why: a peer on the old protocol drops what it does not know, silently.
- Status: target
- Today: violated. 5410fb62 (tagged v0.2.0) added `Control::Questions`
  without bumping protocol 23, so development builds 1ef4886b..9d130109
  speak 23 without it and drop it silently. v0.2.0 itself carries it. The monitor
  still declares 23, so it takes those builds for peers.
- Plans: wire-changes-bump-the-protocol

## I9. The monitor's serve loop never blocks on a lock an agent process can hold.

- Why: one agent holding a lock would stall serving for every agent.
- Status: target
- Today: serving a report takes the agent's binding lock
  (`agent_store::outage_served`) on the loop.
- Plans: the-monitor-loop-never-waits-on-an-agent-lock

## I10. Nothing polls except GitHub.

- Why: many clank processes each polling cost real CPU; work happens only
  when something happens.
- Status: target
- Today: production code has timed sleeps nobody has classified as polls or
  bounded retries.
- Plans: no-polls-but-github

## I11. `clank open` works when no zellij session exists, and never nests one.

- Why: `clank open` is how a workspace starts.
- Status: target
- Today: outside zellij it works; inside a pane whose ancestry cannot be read
  it can start a nested session.
- Plans: an-unreadable-ancestor-is-outside-zellij

## I12. A process's zellij session is read from its ancestry.

- Why: inherited `ZELLIJ_SESSION_NAME` goes stale on a rename or a lost index
  row; a process cannot change sessions.
- Status: holds
- Tests: `crates/cli/src/cli/zellij_endpoint.rs::a_process_two_parents_below_a_server_resolves_to_its_full_socket_path`,
  `crates/cli/src/cli/zellij_endpoint.rs::a_stale_caller_environment_does_not_reach_the_client`

## I13. Every invariant names its enforcement.

- Why: without it, the next plan undoes the invariant. An entry names the
  tests that fail when it breaks, or the queued plan that makes it hold.
  Weakening or deleting a test named here needs the user's sign-off.
- Scope: the gate proves the names resolve. That each named test protects its
  invariant is the reviewers' check.
- Status: holds
- Tests: `crates/cli/tests/it/invariants_registry.rs::every_entry_resolves`
