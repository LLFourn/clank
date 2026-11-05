# durable-formats-survive-upgrade
# Durable formats carry a version and survive an upgrade

## Why

RELEASE-CHECKLIST.md §1 `durable-formats-survive-upgrade`. After the
first release, every upgrade reads state an older clank wrote:
- `.clank/config.json` and `~/.clank/config.json`;
- the `.clank/` tree: plans, finished, agents' feedback, inbox, events
  and blocks;
- installed hooks and skills;
- running monitors.

Monitors already retire on a new build, and the monitor protocol and the
state cache are versioned. What is still missing:
- the config files have no schema version;
- a config layer that fails to parse is skipped with a warning and the
  command carries on (`cli/config.rs`), so after an upgrade the wrong
  settings silently apply;
- migrations are one-offs (`inbox::migrate`, `block migrate`,
  `migrate_claude_perms`, setup's hook migration).

## Inventory and scope

Every durable on-disk format, with what this plan does to it. Paths are
under `crates/`.

**In scope: the three config documents.** Each gets a version and one
validated boundary:
- **A1. Repo config `.clank/config.json`.**
  - Read by: `config::load` layers (`cli/config.rs:395-430`),
    `agent_store::load_repo_config` (485) and `try_resolve_via_team`,
    `agent.rs` `read_repo_config`, and `init.rs` `repo_has_team` (258)
    and `register_repo_team` (544).
  - Written by: `config::set_key` (`config.rs:930`, neither atomic nor
    locked), `agent_store::record_web_port` (413),
    `agent.rs::write_repo_config_raw` (1887), and `init.rs` (239).
- **A2. User config `~/.clank/config.json`.**
  - Owner: `clank-user-config` (`user-config/src/lib.rs` `read`,
    `transact`, `replace`), shared by the CLI and the web door.
  - Bypassing readers: `team::read_user_config` (`team.rs:55`) and
    `agent.rs:2610`, `init.rs:195/589`, and `config::apply_layer`.
- **A3. Per-agent config `.clank/agents/<label>/config.json`.**
  - Owner: `agent_store` (`load_json`, and `try_mutate_locked` under
    `binding.lock`).
  - It has no `extra`, so unknown keys are dropped on rewrite.

**Already versioned, or governed by their own contract.** Unchanged, and
no second version axis is added:
- the event WAL `events/<key>.jsonl` and `messages.jsonl`: the per-record
  `t`/`v`/`seq` envelope, with `CURRENT_V=1` and
  `REPLY_REQUIRED_MESSAGE_V=2` (`github/log.rs:137-168`). Records it does
  not understand are kept as `Line::Foreign`;
- `events/<key>.producer` (`OnDisk.version`);
- `pane-owners.json` (`version==1`);
- the state cache `*.v15.bin` (`CACHE_FORMAT_VERSION`);
- the monitor descriptor (`PROTO`), and the runtime namespace files
  stamped by the monitor;
- the html builder (`BUILDER_VERSION`);
- the user-command envelope `v`;
- `ProcToken` `v`.

**Durable state files without a version.** Kept as they are, since their
serde shapes already accept older bytes, and pinned by fixtures (below):
- `turn.json`, which carries a `legacy::Stored` conversion;
- `question.json`;
- `recovery.json`, `recovery-issued.json` and `nudge-pending.json`;
- `.clank/stash/<stem>.json`, with legacy `.clank/shelved/`;
- `fork.json`, both the agent's and the clone's;
- the block `.received` markers;
- the remote door stores `remote-sessions.json` and `remote-links.json`;
- the `user-commands` ledger.

**Disposable projections.** Rebuilt when they fail to read; unchanged:
- `state.json`;
- the events sidecars `*.state.json` and `*.ci.json`, and `.meta.json`;
- `.clank/html/`;
- `.clank/zellij/layout.kdl`;
- `~/.clank/setup-manifest.json`, which already refuses when corrupt.

**Text and git artifacts.** Unchanged:
- plans and finished plans: tracked, read from git trees, so old bytes
  are history and cannot be migrated;
- queue and drafts, including the legacy `stubs/`;
- `feedback/<sha>.md`, with its verdict line and the
  `roster-stand-in:v1` marker;
- blocks and unblocks, with the optional `occurrence:` stamp;
- the legacy inbox `*.md`;
- `refs/clank/{stash,shelved,stash-pop}`;
- the post-rewrite hook marker.

**Installed harness files.** Unchanged: their cross-version contract is
the hook argv `clank stop-hook …`, which `setup` and `doctor` already
recognise.

## What changes

### One validated boundary per config document

`crates/user-config` grows a small generic module, `versioned`, that
owns how a config document is read:
- **Missing file:** absent. Each caller keeps its intentional
  first-run behaviour.
- **Malformed JSON, or a `version` that is not a positive integer:** an
  error naming the file and the parse error.
- **`version` higher than this build knows:** an error, "written by a
  newer clank (version N; this build reads up to M)".
- **No `version`:** today's shape. It is read as version 1 with no
  transformation, because today's files are version 1. The next write
  stores `"version": 1`.

Each document has exactly one module that every reader and every
read-modify-write uses:
- **A1:** `agent_store` gains `repo_config::{read, transact}`, under a new
  `.clank/config.lock`, written atomically.
  - `config::set_key`, `record_web_port`, `write_repo_config_raw`,
    `init.rs` `write_repo_config` and `register_repo_team` all go through
    `transact`.
  - The duplicate writers are deleted.
  - `config::load`'s repo layer, `load_repo_config`, `read_repo_config`,
    `repo_has_team` and `try_resolve_via_team` read through `read`.
- **A2:** `clank_user_config::{read, transact}` become the only path.
  `team::read_user_config` and the duplicate in `agent.rs`, `init.rs` and
  `apply_layer` call `clank_user_config::read`.
- **A3:** `agent_store::load_json`/`try_mutate_locked` validate through
  `versioned`. `AgentConfig` gains `#[serde(flatten)] extra`, so keys a
  newer clank wrote survive an older clank's rewrite.

**Fail closed before anything can overwrite.**
- A malformed or newer-version file stops the command before defaults,
  initialization, migration or mutation is applied. The file's bytes are
  left exactly as they were.
- The two `init` paths that today turn a parse failure into
  "no team" or `unwrap_or_default` and overwrite the file
  (`init.rs:258-265`, `544-552`, and `repo_config_if_valid` in
  `agent_store.rs:462-476`) now propagate the error.
- `load_repo_config` stops calling every serde error a legacy schema. The
  legacy-shape detector stays for the genuine legacy shape
  (`team`/`promoted`/array `agents`), with its existing remedy.

**Lossy readers become loud.** These sites must surface the error
instead of quietly defaulting:
- `config::apply_layer` skipping a layer;
- `reviewer_tiers_for_render_with`;
- `roster_labels`;
- `status.rs:622`;
- `team.rs:91` (auto-mode silently Off);
- `doctor.rs:142` and `335`.

Status and the TUI show it as a failed config line rather than crashing
the TUI. `clank doctor` reports each config document as ok, missing,
malformed or too new.

### Fixtures freeze the bytes that exist today

`crates/cli/tests/fixtures/formats/` holds real pre-change files: the
bytes this repo and `~` have now, scrubbed of secrets and session ids,
plus hand-kept older shapes the code already accepts. Each is read
through its production reader, and, where the format has one, its
production read-modify-write. The tests assert the meaning survived.

**Config fixtures:**
- an unversioned repo config with roster, review and finish settings,
  web port and `extra` keys;
- an unversioned user config with teams, remote, `user_command` with a
  legacy `site_key`, and `extra`;
- an unversioned agent config with a binding, `resume_pending`,
  `wait_events` (a github watch with prompt, a timer, a command) and old
  `role`/`tool` keys;
- the legacy repo shape, which still gets its legacy error.

**State fixtures:**
- a `turn.json` with stacked attendances, plus the older single
  `attendance` form;
- `question.json`;
- `recovery-issued.json`;
- a stash record, and a `.clank/shelved/` record;
- `fork.json`, both kinds;
- the remote door stores, with tokens replaced by placeholders.

Each must load with the same meaning, and its next write keeps it.

**The event WAL** already has old-byte tests (`events.rs:1020-1098`,
`log.rs:2537`, `log/message.rs:529`). This plan adds a fixture log mixing
v1 records, a v2 reply-required message, a foreign record and a torn
tail, and asserts:
- dedup keys and acks are unchanged;
- the reply obligation survives;
- the foreign record and its seq are preserved through a compaction.

### Migrations

The only new migration step is "no `version` → 1", and it is an identity.
The mutation requirement applies to real migration steps only. Those
that exist are each pinned by a fixture whose mutation fails it:
- `UserCommandSection.site_key` folded into `keys`;
- the `force_review_on_*`, `reviewers`, `hint`/`wait` and `passkey`
  serde aliases;
- `turn.json`'s `legacy::Stored`;
- `state.json`'s `one_or_many`.

**Migrations that stay separate, and why:**
- `inbox::migrate`: a data move from markdown files into the WAL, run on
  read under its own lock;
- `clank block migrate`: a manual layout flatten;
- `migrate_claude_perms`, setup's retired assets and hooks, and Claude's
  Stop-hook mode: these touch harness files, not clank formats.

### README

An "Upgrading" section:
- running monitors retire themselves on a new build, and panes keep
  running;
- a config written by a newer clank is refused, not silently ignored;
- `clank doctor` checks each config document.

## Acceptance

- **`versioned` unit tests:** missing, malformed, version absent (read
  as 1), version 1, a future version, `"version": "x"` and
  `"version": 0`.
- **Overwrite protection.** For A1, A2 and A3, a malformed file and a
  future-version file each make the real write paths fail with the
  file's bytes unchanged, byte-compared:
  - `config::set_key`, `init` with `--team`, and `agent add`'s roster
    insert;
  - `record_web_port`;
  - `clank_user_config::transact` from the CLI and the web door;
  - `try_mutate_locked`.

  A missing file still initializes as today.
- **Loud readers.** Status and doctor tests: a malformed repo config
  shows a failed config line in status, and doctor reports it as a
  failure. A newer-version user config does the same.
- **Unknown keys.** An agent config with an unknown key keeps it through
  `update_agent_config`.
- **Fixtures.** Every fixture above passes through its production reader
  and writer. The real migration steps each have a mutation that fails
  their fixture.
- **Gates.** `no_json_literal_config_writes` passes. Grepping for
  `serde_json::from_str` on a config path outside the three boundary
  modules finds nothing, which an `it` gate enforces.

## As landed

- **549ff543:** the plan as written.
  - `clank-user-config::versioned` (`read`, `read_stored`, `write`, `Refusal`).
  - `agent_store::repo_config`:
    - `read` / `transact` / `transact_document` / `replace`, under
      `.clank/config.lock`, writing atomically;
    - every roster edit goes through one `edit_roster`;
    - the duplicate writers are deleted.
  - The user config is read and written only through
    `clank_user_config`.
  - `AgentConfig` gains `extra`.
  - Readers that used to be lossy now report the failure:
    - `config_errors` in status, shown as a red `conf` line in the TUI;
    - doctor's `config_checks`.
  - Fixtures live under `tests/fixtures/formats/`:
    - user and remote fixtures are hand-built from the types, with no
      real data;
    - the rest are modelled on this repo's `.clank`, with ids and paths
      replaced.
  - The `config_read_boundary` gate is added.
  - README: Upgrading.
- **a9fbff79:**
  - A versioned document is never legacy. Only an unversioned document
    in the old shape may be recreated, and a mixed roster is a damaged
    one.
  - `config::check_layer` validates every layered setting, before an
    edit and on its result. The user config does this through a required
    check on `transact`, which the CLI and the web door both pass.
  - Status records an unreadable agent config in `config_errors` with
    its path, and marks the row unreadable. `source_status` propagates
    the error.
- **7e0732df / 303d00af:** the publication check moved onto the shared
  write path, and it runs the reader's own `shape()` on the outgoing
  document as a versioned one. Every typed or raw edit therefore leaves a
  document the reader accepts, legacy markers included.
- **Departures:**
  - The legacy detector also treats a non-empty `agents` map whose
    entries all lack a role as legacy.
  - A bad recovery duration is now an error.
  - The `reviewers` role alias has no on-disk reader to pin, so it is
    covered by its core unit test only.
  - There is no fixture for the user-command ledger.
- **Known flake, not from this plan:**
  `wake_receipt_acceptance::shared_monitor_recurring_timer_watches_config_and_ack_then_rearms_native_deadline`
  fails intermittently, 1 run in 4 here. Its timer repeats every 10s,
  so a run that crosses a slot boundary appends a new slot before the
  "pending replay appends nothing" assertion.
