# the-monitor-is-structs-in-submodules

## Why

`crates/cli/src/cli/monitor.rs` is 19.6k lines: about 6.3k of production
code and 13.3k of inline tests (`recovery_tests`, `tests`, `lease_tests`,
`build_tests`, `superseding_tests`, `user_command_tests`,
`producer_identity_tests`), beside the already split `monitor_keeper.rs`,
`monitor_outage.rs`, `monitor_watchdog.rs`, `monitor_upgrade.rs` and five
`monitor_*_tests.rs` files. The serve loop is one `async fn start` of about
1,200 lines (≈3138–4342) that holds every concern's state as loose locals
and calls free functions over them, which is how a blocking lock got into
the serve loop unnoticed: nothing gives a concern a boundary to be reviewed
at.

The production code, by concern (line ranges as of the promote):

- **client protocol** (≈141–1670): `Holder`, `Hello`, `Descriptor`,
  `Opening`, `greeting_of`, `Attached`, `connect_within`, `write_frame`,
  `attach`, `look_at`, socket dirs (`socket_dirs`, `vet_dir`,
  `ensure_dir`, `verify_private`, `sweep_dead_sockets`).
- **runtime paths and repair** (≈1670–1950): `runtime_dir`,
  `runtime_path`, `take_repair_permit`, `repo_tag`.
- **spawning and ownership** (≈1950–2620): `wait_for_ownership_by`,
  `supervise`, `attach_or_spawn_by`, `look_after`, `Hangup`,
  `reconcile_after`, `CoreWakes`, `monitor_command`.
- **lease and publication** (≈2620–2960): `FileId`, `LeaseError`,
  `acquire_lease`, `publish_in`, `published`.
- **serve loop** (≈2960–4450): `Served`, `Client`, `Admitted`,
  `Reporting`, `Heard`, `RunExits`, `start`, `Releases`.
- **user commands** (≈4450–4800): `unavailable_somewhere`,
  `user_command_round`, `user_command_look`.
- **turns and recovery dispatch** (≈4800–5650): `owed_items`, `project`,
  `recovery_gate_in`, `authorize`, `Panes`, `end_gone_runs`,
  `deadline_round`, `act_on_recovery`.
- **hosted sources** (≈5650–6270): `LeaseWatch`, `Detection`, `Built`,
  `host_sink`, `restart_source`, `spawn_hosted`, `Hosted`,
  `roster_authors`.

## Build

1. Move `monitor.rs` to `cli/monitor/mod.rs`, and the existing
   `monitor_*.rs` files under `cli/monitor/` (dropping their `#[path]`).
2. One submodule per concern above. Where a concern's state lives as
   locals of `start`, it becomes a struct owning that state with methods
   for the events it handles (clients and admission, run exits and
   releases, user commands, turns and recovery dispatch, hosted sources),
   so `start` owns those structs and dispatches each `Heard`/wake to one
   method. Concerns share no free functions over each other's state.
3. Tests move with their code, as each submodule's sibling `*_tests.rs`;
   no test is rewritten except for paths and the visibility the move
   needs. Source-reading tests (those that grep `monitor.rs` for a call)
   are repointed at the file that now holds the code.
4. A pure refactor: no behaviour, wire or format change. The gates that
   name monitor paths (`timed_wait_boundary`, `hook_reach`,
   `harness_adapter_boundary`, `stale_write_boundary`, and any other that
   lists `cli/src/cli/monitor.rs`) are updated to the new paths, with the
   same classifications.

## Acceptance

- `cli/monitor/mod.rs` holds only the module root and the serve loop's
  dispatch; `start` is a loop over owned structs, not 1,200 lines of
  locals.
- No production submodule exceeds about 1.5k lines (tests excluded).
- `cargo test -p clank --lib` and `cargo test -p clank --test it` pass
  with the same test count as before the move.
