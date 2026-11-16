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
  started without `clank run` never wakes a Codex or OpenCode agent. A Grok
  Stop the monitor does not answer in time still arms Grok's wait, which
  asks the monitor when it attaches.
- Tests: `crates/core/src/turn_policy/tests.rs::a_codex_run_ending_wakes_its_parked_agent_once`,
  `crates/core/src/turn_policy/tests.rs::a_codex_run_ending_mid_turn_is_told_at_the_next_stop`,
  `crates/core/src/turn_policy/tests.rs::a_claude_run_ending_is_left_to_the_harness`,
  `crates/core/src/turn_policy/tests.rs::a_run_going_mid_retirement_is_never_retired_without_its_notice`,
  `crates/cli/src/cli/monitor_turn_tests.rs::a_codex_run_ending_wakes_its_parked_agent`,
  `crates/cli/src/cli/monitor_turn_tests.rs::a_codex_run_ending_is_never_told_to_a_rebound_session`,
  `crates/cli/src/cli/monitor_grok_tests.rs::a_grok_stop_the_monitor_answers_late_still_arms_its_wait`

## I2. Reading blocks never changes them.

- Why: a read that writes can wedge every reader on one bad file.
- Status: holds
- Scope: a block read takes the repository's block lock and nothing else; it
  never migrates, repairs or rewrites block files. Other stores are not yet
  audited for reads with side effects.
- Tests: `crates/cli/src/cli/block/repo_tests.rs::per_agent_block_files_are_ignored_and_left_alone`

## I3. Inbox events are at-least-once until acked; everything else is best effort.

- Why: per-session delivery records replay work after every rebind.
- Status: target
- Today: GitHub, command, timer and message events are re-presented until
  `clank events ack`, which is intended. Separately,
  `presentation::Receipts` (`delivered.json`) records which items reached
  which session, and the monitor reads it (`delivered_to`).
- Terms: a transport attempt is a try to put an item in front of the model;
  a presentation is the model being shown it; an acknowledgement is the agent
  saying it is handled. Only inbox events have acknowledgements.
- Plans: one-state-file-per-agent

## I4. A hook acts only on what it is given.

- Why: hooks must not read configuration owned by other components; the
  monitor decides.
- Status: holds
- Scope: a hook uses its stdin, the binding it routes by, and the evidence the
  hook audit allows (the agent's transcript, process identity and upgrade
  state). It never reads monitor-owned policy or configuration.
- Tests: `crates/cli/tests/it/hook_reach.rs::nothing_a_hook_reaches_reads_policy_or_decision_state`,
  `crates/cli/tests/it/hook_reach.rs::every_root_reader_and_exception_names_a_function`,
  `crates/cli/tests/it/hook_is_a_messenger.rs::hook_modules_touch_no_decision_state`,
  `crates/cli/tests/it/hook_is_a_messenger.rs::shared_hook_identity_paths_cannot_parse_monitor_policy`,
  `crates/cli/src/agent_store/identity.rs::the_binding_reader_and_the_roster_parser_agree_on_every_tool`

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
  identity is re-parsed from strings.
- Plans: one-state-file-per-agent, a-watch-is-a-typed-producer

## I7. Only the master raises blocks.

- Why: a block pauses the whole repo; the master decides when to ask a person.
- Status: holds
- Scope: `clank block ask`, plain and `--permission`, admits the repo's
  unique master and a caller with no harness session variable and no
  `CLANK_AGENT`. Any other agent is refused before a block file or the
  `blocked` hook is touched, and so is a caller whose identity fails to
  resolve. Only the master's wakes offer `block ask`: a reviewer's name
  REQUEST_CHANGES, and other agents are pointed at `clank send --master`.
- Tests: `crates/cli/src/cli/block/repo_tests.rs::a_reviewer_is_refused_a_block_and_told_to_request_changes`,
  `crates/cli/src/cli/block/repo_tests.rs::a_standby_agent_is_refused_a_block_and_told_to_send_the_master`,
  `crates/cli/src/cli/block/repo_tests.rs::the_master_raises_a_block`,
  `crates/cli/src/cli/block/repo_tests.rs::a_caller_with_no_identity_raises_a_block`,
  `crates/cli/src/cli/block/repo_tests.rs::an_unresolved_identity_is_refused_and_creates_nothing`,
  `crates/cli/src/cli/wake_render.rs::only_the_master_is_offered_a_block`,
  `crates/cli/src/cli/turn_custody_tests.rs::only_the_masters_escalated_wake_offers_a_block`

## I8. Any change to a monitor wire message bumps the protocol version.

- Why: a peer on the old protocol drops what it does not know, silently.
- Status: holds
- Scope: drift is detected by the test: every message the monitor and its
  clients exchange, and every type nested in them, has a hand-written sample
  the compiler keeps complete, and the samples' JSON is pinned to `PROTO` in
  `PINS`. The bump is enforced by review of `PINS`: editing the last pair's
  hash in place, without a bump, passes the test, so reviewers check that
  `PINS` only ever grows.
- Tests: `crates/cli/src/cli/monitor_wire_golden_tests.rs::the_wire_shape_is_pinned_to_the_protocol`,
  `crates/cli/src/cli/monitor_wire_golden_tests.rs::every_variant_has_a_sample`,
  `crates/cli/src/cli/monitor_wire_golden_tests.rs::pins_are_a_history`

## I9. The monitor's serve loop never blocks on a lock an agent process can hold.

- Why: one agent holding a lock would stall serving for every agent.
- Status: holds
- Scope: the sweeps covered the locks serving, folding, recovery and
  hosted timers take: an agent's binding lock, `.clank/blocks.lock`, event
  log sidecars, timer ingest leases, and the old file inbox's migration
  (`inbox/.lock` and `messages.writer.lock`), which is removed. The loop
  only tries the binding lock (an outage notice stands until a serve finds
  it free), reads blocks without their lock (a read is checked against its
  question's generation, and one that cannot settle blocks and owes
  nothing), and takes each event-log sidecar and each timer acceptance's
  ingest lease once, without waiting. A held one leaves that source or
  acceptance unavailable for the pass (its items absent, the agent unknown
  to recovery and its ledger, the timer pending) until a bounded contention
  retry (250 ms doubling to 5 s) finds it free. A holder may keep a lock
  across a side effect, such as signed user-command delivery, because the
  loop never waits on it. The ingest lease's patience (250 ms) remains only
  off the loop: in commands, wait clients, and a hosted GitHub source's own
  task. `messages.writer.lock` only serializes message writers.
  Corrupt or unreadable logs are skipped as before
  (`event_source::owed_from_sources`), which reads as no events.
- Tests: `crates/cli/src/cli/monitor_turn_tests.rs::a_held_binding_lock_stalls_no_serve_and_leaves_the_notice`,
  `crates/cli/src/cli/monitor_turn_tests.rs::a_held_blocks_lock_stalls_no_serve`,
  `crates/cli/src/cli/monitor_turn_tests.rs::a_held_event_log_stalls_no_serve_and_the_retry_presents_it`,
  `crates/cli/src/cli/monitor_turn_tests.rs::a_held_ingest_lease_stalls_no_serve_and_the_retry_accepts_each_once`,
  `crates/cli/src/cli/monitor_turn_tests.rs::a_held_legacy_inbox_and_message_writer_lock_stall_no_serve_and_leave_the_old_inbox`,
  `crates/cli/src/cli/timer_source.rs::held_sidecars_leave_timers_pending_until_the_contention_retry`,
  `crates/cli/src/cli/timer_source.rs::a_held_ingest_lease_leaves_acceptances_pending_until_the_contention_retry`,
  `crates/cli/src/cli/timer_source.rs::each_loop_timer_operation_takes_its_sidecar_once`

## I10. Nothing polls except GitHub.

- Why: many clank processes each polling cost real CPU; work happens only
  when something happens.
- Status: target
- Today: production code has timed sleeps nobody has classified as polls or
  bounded retries.
- Plans: no-polls-but-github

## I11. `clank open` works when no zellij session exists.

- Why: `clank open` is how a workspace starts.
- Status: holds
- Scope: an ancestry that cannot be read never stops it. Outside a pane
  (`ZELLIJ` unset) every caller of this process's endpoint takes that as not
  in zellij. Inside a pane `open` still falls back to opening normally, by
  the user's rule, so there it can start a nested session.
- Tests: `crates/cli/src/cli/zellij_endpoint.rs::an_unreadable_ancestry_outside_a_pane_is_not_in_zellij`,
  `crates/cli/src/cli/zellij_endpoint.rs::a_server_the_walk_found_wins_over_the_environment`,
  `crates/cli/src/cli/open_zellij.rs::unreadable_ancestry_falls_back_to_normal_opening`

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
