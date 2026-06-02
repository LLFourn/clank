# investigate-open-false-gitwithoutclank

`clank open` reports `GitWithoutClank` for repos whose `.clank/`
is fully set up but lacks a `.clank/config.json`. Reproduced
against `~/src/fswt/frostsnap_nostr-taipei`: linked worktree
correctly detected (`is_linked_worktree: true`), `.clank/`
contains `agents/`, `cache/`, `finished/`, `plans/`, `queue/` —
but no `config.json`, so `clank open` recommends `clank_init`.

## Root cause

`crates/cli/src/cli/open.rs` decides `ClankInitialized` vs
`GitWithoutClank` from a single file check:

```rust
let clank_config_path = repo_root.join(".clank/config.json");
if !clank_config_path.is_file() {
    ... GitWithoutClank ...
}
```

`config.json` became optional once master became a per-agent
role claim (`crates/core/src/agent_config.rs`). A repo can be
fully clank-initialized with only `.clank/plans/` and per-agent
state under `.clank/agents/` — no shared file required.

## Fix

Treat the repo as `ClankInitialized` if `repo_root/.clank/` is
a directory. `clank init` always creates that directory plus
`plans/` and `.gitignore`, so its presence is the actual marker.

Keep `repo_root/.clank/config.json` as the data source for the
optional fields (e.g. hooks) but stop using it to gate the
state.

## Tests

- `open_clank_initialized_without_config_json` — fresh git
  repo, create `.clank/plans/` only, run `clank open .`,
  assert state == `clank_initialized` (not
  `git_without_clank`).
- `open_clank_initialized_linked_worktree_without_config_json`
  — linked worktree with `.clank/plans/` but no `config.json`,
  assert state == `clank_initialized`. Pins the original
  bug report.

## Out of scope

- Migrating any other code path that keys off
  `.clank/config.json` (status, wfw, etc. probably handle
  missing config fine via defaults — verify but don't change).
- Re-adding a required `config.json`. The whole point of
  per-agent master is that the repo-shared file is optional.
