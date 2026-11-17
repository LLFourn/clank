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
- Scope: on Claude and Grok clank sends nothing and relies on the harness
  to tell its agent: Grok's source says it does, and Claude's doing so has
  not been recorded (`docs/harness-support.toml`). Codex holds the turn in
  its synchronous Stop hook and was observed (0.167.2) never to continue the
  agent when a background command exits; nothing shows OpenCode doing so.
  On those, clank holds a run-ended notice for the session that started the
  run until its next Stop that may wake
  (`clank_core::harness::Behaviour::reports_background_completion`).
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
- Status: holds
- Scope: GitHub, command, timer and message events are re-presented until
  `clank events ack`, whichever session they were shown to; a rebind drops
  none of them. What the bound session was shown, and when, is kept only as
  that session's `bound.delivered` part of the agent's `state.json`: best
  effort, read for status and recovery's timing and never for what is
  presented, and gone with the session at a rebind, so it replays nothing.
- Terms: a transport attempt is a try to put an item in front of the model;
  a presentation is the model being shown it; an acknowledgement is the agent
  saying it is handled. Only inbox events have acknowledgements.
- Tests: `crates/cli/src/cli/monitor_turn_tests.rs::unacked_inbox_events_outlive_a_rebind_until_each_is_acked`,
  `crates/cli/src/agent_store/state.rs::a_rebind_drops_every_bound_part_and_keeps_the_kept_ones`,
  `crates/cli/src/cli/presentation_tests.rs::a_rebind_cannot_land_inside_an_old_sessions_receipt`

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
- Status: holds
- Scope: an agent's directory holds `config.json` (what the user sets),
  `state.json` (every runtime fact, its session once, in `binding`), its
  history (`nudges.jsonl`), its inbox (`events/`) and its reviews
  (`feedback/`). What is known of the bound session (`bound`) names no
  session of its own and goes with it at a rebind, and nothing in
  `state.json` repeats the roster. Each writer reloads the document under
  the agent's lock and patches its own part, so no copy of the document is
  ever written back. Other stores are not audited here.
- Tests: `crates/cli/src/cli/turn_custody_tests.rs::a_fresh_agent_keeps_exactly_its_files_and_names_its_session_once`,
  `crates/cli/src/format_fixtures.rs::the_state_document_reads_through_every_part_and_each_writer_keeps_the_rest`,
  `crates/cli/src/cli/turn_custody_tests.rs::a_stale_turn_record_restores_no_binding_and_no_old_state`,
  `crates/cli/src/cli/turn_custody_tests.rs::a_question_update_and_a_turn_record_landing_together_both_survive`

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
  hosted timers take: an agent's state lock, `.clank/blocks.lock`, event
  log sidecars, timer ingest leases, and the old file inbox's migration
  (`inbox/.lock` and `messages.writer.lock`), which is removed. The loop
  only tries an agent's state lock. One found held leaves its write
  undone: an outage notice stands, a control request is refused as busy,
  recovery claims and dispatches nothing, and a turn record or a Stop's
  question evidence is kept in memory. The release wakes the loop, which
  writes those and folds again, redoing recovery's claim, the ledger and the
  published state. It reads blocks without their lock (a read is checked against its
  question's generation, and one that cannot settle blocks and owes
  nothing), and takes each event-log sidecar and each timer acceptance's
  ingest lease once, without waiting. A held one leaves that source or
  acceptance unavailable for the pass (its items absent, the agent unknown
  to recovery and its ledger, the timer pending) until it is released: a
  thread blocked in a flock of it as the loop wanted it lets it go at once
  and wakes the loop, which retries the contended operations then. A holder may keep a lock
  across a side effect, such as signed user-command delivery, because the
  loop never waits on it. The ingest lease's patience (250 ms) remains only
  off the loop: in commands and wait clients; a hosted GitHub source takes
  it without waiting and follows until it is handed over. `messages.writer.lock` only serializes message writers.
  Corrupt or unreadable logs are skipped as before
  (`event_source::owed_from_sources`), which reads as no events.
- Tests: `crates/cli/src/cli/monitor_turn_tests.rs::a_held_state_lock_stalls_no_serve_and_its_release_writes_what_it_left`,
  `crates/cli/src/cli/github/supervisor.rs::a_held_lease_parks_no_runtime_thread_and_its_release_hands_it_over`,
  `crates/cli/src/agent_store/state.rs::a_held_lock_is_contended_for_a_take_that_does_not_wait`,
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
- Scope: a poll wakes again while what it observes may be unchanged, with no
  bound. Scheduled domain work, one-shot deadlines, bounded retries and
  throttles of an event's work are not polls. Every production timed wait is
  classified so in `timed_wait_boundary`; the gate sees forms by name, so
  the classes themselves are the reviewers' check.
- Today: besides GitHub's, one residual poll remains: recovery's
  `RETRY_LOOK`, which stands for three different waits (the gate, which
  only `Driver::due` asks; the record showing the failed turn; a turn it
  cannot watch) behind one `next_at`.
  A held event-log sidecar or ingest lease wakes the monitor on its release,
  and a remote's browser streams wake on a sessions-store write or their
  expiry. A client with no monitor (a parked wait, a turn child, the TUI's
  keeper) attempts again only when its failure's events move: the repo's
  lease or repair permit releasing, a monitor publishing, an install, a
  config edit. Its outage has one report deadline, which those events and
  the attempts they cause do not move; a failure none of them ends (a
  start that failed for want of resources) is reported then and not
  attempted again until one moves. A watch that cannot be armed or stops
  ends the wait at once. Other failed work waits for what can end its
  failure, and is never tried on a clock: a timer occurrence that will not
  write stays owed until a change at its log, its seen index or their
  directory leaves them other than the failure found them (its own staging
  never does), looked at once more as the owner's watch is armed, and one
  held off by a lock waits for the release; a failed
  status refresh, in the TUI or a status watch, until a wake moves its
  inputs from where the failure found them (or `s` in the TUI's log); a
  failed self-replacement until another build is installed or a watch
  hears the installation change, and a watch that cannot be had, covers
  less than the installation's directories, or fails ends that wait,
  saying so (a monitor and a status watch end; the TUI says so, and `w`
  watches again, trying the replacement once more); a deferred retirement until a member of its
  group exits or its producer record changes, every member watched or the
  source stopped, saying why. Restarts answer an end, with
  a budget counted per episode: a command source is restarted when its
  run ends, a pane subscription when it ends, and the web connector when
  its connection ends, each `FAILURES_BEFORE_SURFACING`, `RESTARTS` or
  `RECONNECTS` times in a row, and then reported and not tried again
  until a person asks (`r` on a source's page, a viewer's retry on a
  stopped pane, which the page shows, the remote switched off and on) or
  what it runs changes; routine verifications never restart a pane.
- Tests: `crates/cli/tests/it/timed_wait_boundary.rs::every_timed_wait_is_classified`,
  `crates/cli/tests/it/timed_wait_boundary.rs::invariant_status_matches_the_inventory`,
  `crates/cli/src/cli/monitor_turn_tests.rs::a_held_sidecar_wakes_the_loop_only_on_its_release`,
  `crates/cli/src/cli/monitor_turn_tests.rs::a_release_before_the_waiter_is_armed_still_wakes_the_loop`,
  `crates/web-cloudflare/src/host.rs::an_unchanged_store_is_not_read_again_while_the_session_lasts`,
  `crates/web-cloudflare/src/host.rs::a_revocation_before_the_watch_is_armed_is_not_lost`,
  `crates/cli/src/cli/monitor_outage.rs::a_monitor_published_before_its_watch_is_armed_is_found`,
  `crates/cli/src/cli/monitor_outage.rs::a_held_leases_release_leads_to_an_attach`,
  `crates/cli/src/cli/monitor_outage.rs::a_lease_held_forever_is_reported_at_its_deadline_with_no_attempt_between`,
  `crates/cli/src/cli/monitor_outage.rs::a_reported_outage_waits_on_its_events_alone`,
  `crates/cli/src/cli/monitor_outage.rs::a_watch_refused_or_stopped_ends_the_wait`,
  `crates/cli/src/cli/monitor_outage.rs::the_kernel_hears_a_held_lease_released`,
  `crates/cli/src/cli/monitor_keeper.rs::an_unattached_keeper_reports_once_at_its_deadline_and_attempts_nothing_more`,
  `crates/cli/src/cli/monitor_keeper.rs::a_watch_refused_or_stopped_is_shown_and_ends_the_attempts`,
  `crates/cli/src/cli/wait.rs::a_bound_wait_fails_once_for_an_outage_and_then_waits_on_its_events`,
  `crates/cli/src/cli/turn_child_tests.rs::a_stale_child_waits_quietly_and_tells_its_agent_nothing`,
  `crates/cli/src/cli/timer_source.rs::a_failed_entry_waits_for_its_files_to_change`,
  `crates/cli/src/cli/timer_source.rs::a_failed_attempts_own_writes_try_nothing_again`,
  `crates/cli/src/cli/timer_source.rs::a_staging_files_rewrite_is_not_a_change`,
  `crates/cli/src/repo_watch.rs::a_timer_index_repair_is_heard_and_tries_the_failed_timer`,
  `crates/cli/src/cli/monitor.rs::a_hosted_timer_that_will_not_write_waits_for_a_timer_log_to_change`,
  `crates/cli/src/cli/wait.rs::the_own_loop_tries_a_failed_timer_only_on_a_timer_log_change`,
  `crates/cli/src/cli/status_watch_tests.rs::a_failed_refresh_waits_for_its_inputs_to_move`,
  `crates/cli/src/cli/status_tui/mod.rs::a_failed_rebuild_is_owed_again_only_for_a_cause`,
  `crates/cli/src/upgrade.rs::a_failed_replacement_waits_for_the_installation_to_change`,
  `crates/cli/src/upgrade.rs::until_current_returns_when_its_build_is_back`,
  `crates/cli/src/upgrade.rs::an_installation_that_cannot_be_heard_ends_the_park`,
  `crates/cli/src/cli/wait.rs::a_wait_that_cannot_hear_its_installation_is_served_or_says_why`,
  `crates/cli/src/cli/wait.rs::a_failing_source_is_restarted_on_its_ends_until_its_budget`,
  `crates/cli/src/cli/wait.rs::a_deferred_retirement_waits_for_a_member_to_exit`,
  `crates/cli/src/cli/wait.rs::a_failed_retirement_watch_ends_the_wait`,
  `crates/cli/src/cli/wait.rs::a_group_that_cannot_be_covered_does_not_arm_its_retirement`,
  `crates/cli/src/cli/command_source.rs::a_retiring_group_is_covered_whole_or_not_at_all`,
  `crates/cli/src/cli/timer_source.rs::a_repair_before_the_watch_is_armed_is_found_by_the_look_after`,
  `crates/cli/src/cli/status_tui/mod.rs::a_lost_install_watch_is_shown_once_the_screen_is_free_and_w_arms_it_again`,
  `crates/cli/src/cli/status_tui/mod.rs::the_loop_reports_a_lost_install_watch_and_w_rearms_it`,
  `crates/cli/src/cli/status_watch_tests.rs::a_watch_that_cannot_hear_its_installation_ends`,
  `crates/cli/src/cli/monitor.rs::a_monitor_deaf_to_installs_ends`,
  `crates/cli/src/upgrade.rs::a_rearmed_watch_tries_a_failed_replacement_once_more`,
  `crates/web/src/request.rs::a_verify_without_retry_is_routine`,
  `crates/web/src/service.rs::a_verify_carries_the_pages_retry`,
  `crates/cli/src/cli/web/mod.rs::a_first_subscription_that_cannot_start_is_reported_and_retried`,
  `crates/cli/src/cli/wait.rs::a_held_timer_lock_is_heard_released`,
  `crates/cli/src/cli/wait.rs::a_deferred_retirement_withholds_the_launch_and_keeps_the_record`,
  `crates/cli/src/cli/web/mod.rs::a_dead_subscription_is_restarted_at_its_end_until_its_budget`,
  `crates/cli/src/cli/web/bridge_instance.rs::host_crash_reports_disconnection_without_stopping_local_panes`
- Plans: recovery-looks-when-something-moves

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
