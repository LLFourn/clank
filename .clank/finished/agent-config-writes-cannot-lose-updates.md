# agent-config-writes-cannot-lose-updates
# Agent config writes cannot lose updates

## The bug

`agent_store::update_agent_config` is an unlocked read-modify-write:

```rust
let mut cfg = load_agent_config(repo, label)?.unwrap_or_default();
edit(&mut cfg);
save_agent_config(repo, label, &cfg)
```

About 70 production call sites write `.clank/agents/<label>/config.json`
through it (or through `save_agent_config` directly):
- monitor (18), wait (9), stop_hook (6);
- wake_receipt, events, source_status;
- the web bridge (claude/native questions, repo_session);
- `clank as`, `clank auto`, the TUI toggle, fork, doctor.

When two processes overlap, the later save writes back the earlier
process's stale view. That silently reverts any field the other process
changed: `auto_mode`, the session binding, `wait_events`, and so on.

Observed on 2026-10-21. At 21:05 the master ran `clank auto off`. At
21:10:20 a write that was neither the master nor a test put back
`auto_mode: "on"`, and the master resumed being nudged every minute. A
test-suite cause was ruled out: the file's mtime was watched across full
`--lib` and `--test it` runs. The TUI toggle was not ruled out, but the
race is real regardless.

## The fix

**One lock domain per label.** Every mutation of one label's config takes
the SAME stable lock, the agent dir's existing `binding.lock`. It is held
from before the authoritative load until after the atomic replacement
(`save_json` already writes and renames). That covers:
- `update_agent_config` / `set_auto_mode`;
- the existing binding transactions in `agent_store.rs` (`release_binding`,
  `record_transcript`, `bind_session_to_agent`), which already take
  `binding.lock` but call `save_agent_config` directly.

**Acquired exactly once.** One internal transaction,
`mutate_locked(&BindingGuard, edit)`, takes the held guard as proof. The
binding transactions take the lock and call it with their guard.
`update_agent_config` takes the lock and calls the same function. Nothing
under the guard re-acquires the flock, so there is no self-deadlock.
- The compare-and-clear and transcript session checks run inside that
  transaction, against the state loaded under the lock.
- Binding transfer keeps today's rule: release one label's lock before
  touching another's. Cross-label binding uniqueness is not redesigned
  here.

**No stale whole-config writes.** `save_agent_config` becomes private to
the store, and callers are migrated as field edits inside the
transaction, never as a whole `AgentConfig` loaded earlier and assigned
back.
- Fork initialization explicitly initializes or patches the
  destination's intended fields, keeping its new-fork semantics.
- A reader's parse or I/O error stays an error. The transaction never
  replaces unreadable state with a default, because overwriting a config
  it could not read is the same lost update.

**Fields.** After M2, `AgentConfig` holds `auto_mode`, `session` and
`wait_events`. Most of the earlier call-site count was test fixtures or
bookkeeping that M2 moved into monitor-owned `turn.json`. All three
fields stay in `config.json` under the one lock, and no state-file
migration is part of this plan.

## Acceptance

- **Contention test, deterministic, with no sleeps and no unbounded
  barrier:**
  1. writer A enters the transaction and pauses inside its edit, through
     an instrumented closure seam;
  2. a non-blocking lock probe (`try_lock` on the same domain) shows B
     cannot enter;
  3. A is released and commits;
  4. B then runs, loads A's result and applies its own edit;
  5. both edits are present.

  Run it twice:
  - generic `update_agent_config` against `set_auto_mode`;
  - a binding/transcript writer (`bind_session_to_agent` or
    `record_transcript`) against `set_auto_mode`. This proves the shared
    domain.

  Mutation checks:
  - pasting back the old unlocked `update_agent_config` body fails step 2
    and/or step 5;
  - giving the binding writers a separate lock fails the second case.
- **An unreadable config is left alone:** a mutation against an
  unparseable `config.json` errors and leaves the file's bytes unchanged.
- **Source gate in the `it` harness:** no `save_agent_config` call
  outside `agent_store`, and no direct write to an agent `config.json`
  (alongside `no_json_literal_config_writes`).
- **Suites:** `cargo test -p clank --lib` and the `it` modules touched
  pass, with the session env cleared.
- One complete implementation commit.
