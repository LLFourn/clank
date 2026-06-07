# agents-declaration-is-user-local

The `agents` array in `.clank/config.json` is a per-USER
choice, not a team-shared decision. Today the file is
tracked, which forces all collaborators on a repo to use the
same agent set (claude/codex/ruthless or whatever).
Different users may want different agent panels — one
runs claude+codex, another runs claude+ruthless, etc.

This plan pivots to the **skeleton-is-the-declaration**
model: `.clank/agents/<label>/config.json` becomes the
single source of truth for an agent's existence,
declaration (role/tool/launch/initial_prompt), AND state
(session/auto_mode/wfw_timeout). No separate per-user
config file. The tracked `.clank/config.json#/agents`
block is removed.

## Architectural framing

`agent-add-cli-and-repo-scope` introduced the explicit
`.clank/config.json#/agents` block on the premise that
declarations are repo-policy. That premise is wrong:
declarations ARE per-user, just like per-machine state.
The "centralize the declaration" move added a second
source of truth that drifts from the skeleton dirs,
which is the root of several follow-on bugs (the
`tool=None when session=None` synthesis weirdness; the
legacy-fallback path at `config.rs:502-506` that I keep
tripping on in adjacent plans).

The right architecture: the per-agent dir IS the agent.
Presence = registered. File contents = full declaration
+ state. Already gitignored. Already per-user-per-repo.
The fix completes the model that was already half-built;
it doesn't introduce a new one.

## Approach

### 1. Extend `AgentConfig` with declaration fields

In `crates/core/src/agent_config.rs`, add two fields:

```rust
pub struct AgentConfig {
    // ── existing per-machine state ──────────────
    pub auto_mode: AutoMode,
    pub role: Role,
    pub wfw_timeout: Option<String>,
    pub session: Option<Session>,
    pub launch: Option<LaunchConfig>,
    // ── NEW: declaration fields ─────────────────
    pub tool: Option<Tool>,
    pub initial_prompt: Option<String>,
}
```

`tool` becomes a first-class field instead of being
derived from `session.tool`. This resolves the
synthesis weirdness where `tool=None` for unbound
agents — now `tool` is set at registration time and
persists across sessions.

`initial_prompt` mirrors today's `DefaultAgent.initial_prompt`.

`role` already exists on `AgentConfig` — no change.

### 2. `load_merged_agents` rewires to use skeletons directly

In `crates/cli/src/cli/config.rs`, the merged-agent
resolution becomes:

1. Scan `<repo>/.clank/agents/*/config.json` —
   declared agents in this repo.
2. If empty, fall back to user-scope
   `~/.clank/config.json#/default_agents`.

REPLACE semantics still hold: if ANY skeleton exists,
user-scope is ignored entirely for THIS repo. Same shape
as today's repo-vs-user override.

The current legacy-synthesis path at `config.rs:502-506`
(which derives `DefaultAgent.tool` from
`cfg.session.tool`) becomes the PRIMARY path, now
reading the new explicit `tool` field instead of
inferring from session.

### 3. `.clank/config.json#/agents` removed

The `agents` key on `RepoConfigFile` goes away. The
struct keeps its other fields (hooks, diff editor,
whatever else is team-policy).

This is a breaking change to the on-disk format. Mitigated
by the migration step below.

### 4. `clank init` migration

When `clank init` runs and detects
`.clank/config.json#/agents` is non-None:

1. For each entry in the block, write the declaration
   fields (role, tool, launch, initial_prompt) into
   `.clank/agents/<label>/config.json`, MERGING with any
   existing skeleton (preserving
   session/auto_mode/wfw_timeout).
2. Remove the `agents` key from `.clank/config.json`.
3. Print:
   `migrated <N> agents declaration(s) to per-agent
   skeletons (now per-user)`.

Idempotent: re-running `clank init` after migration is a
no-op (the `agents` key is already None).

### 5. `clank agent add / remove / promote`

- `add`: writes the per-agent skeleton directly. Today's
  `--global` flag (write to user-scope) is unchanged.
  The default target is repo-scope per-agent skeleton.
- `remove`: deletes
  `.clank/agents/<label>/config.json`. The `feedback/`
  subdir is preserved (review history stays).
- `promote`: mutates `role` in the target skeleton(s)
  via the existing `ensure_unique_master` helper. Same
  flag handling as today.

The `ensure_unique_master` helper operates on a slice
of `DefaultAgent`-shaped entries today. It needs minor
rework: instead of mutating an in-memory `Vec`, it
mutates per-agent skeleton files (one write per demote
+ the promote). The "atomic" property changes from
"one JSON file written once" to "N skeleton files
written".

**Open question (decide in implementation)**: should
the multi-skeleton write be transactional? Options:
- Accept N writes — atomicity per-file. If a crash
  happens mid-loop, the post-state has SOME demotes
  applied.
- Stage all writes, then `fsync` + rename together.
  More code but truly atomic.

Recommendation: accept N writes. The window is small
(one process, milliseconds) and `promote` is
idempotent — re-running fixes any partial state.

## Surfaces touched

- `crates/core/src/agent_config.rs`:
  - Add `tool: Option<Tool>` and
    `initial_prompt: Option<String>` fields to
    `AgentConfig`.
- `crates/cli/src/cli/config.rs`:
  - `load_merged_agents` rewires to skeleton-first.
  - `load_repo_agents` removed (no more repo-scope
    block).
  - `load_declared_agents` collapses to the same path
    as `load_merged_agents`.
  - The `RepoConfigFile.agents` field is removed.
- `crates/cli/src/cli/agent.rs`:
  - `add` writes per-agent skeleton, not the repo block.
  - `remove` deletes the skeleton.
  - `promote` writes per-agent skeletons (loop).
  - `ensure_unique_master` returns the labels to update;
    the caller does the writes.
- `crates/cli/src/cli/init.rs`:
  - Add migration step that moves
    `.clank/config.json#/agents` → per-agent skeletons.
- `crates/cli/src/cli/agent.rs`'s `compose_bootstrap_launch`
  (`agent-start-bootstraps-missing-skeleton`):
  - Read `tool` directly from `AgentConfig` instead of
    deriving from session. The no-tool error path keeps
    the same hint but now points at
    `.clank/agents/<label>/config.json` (the right file)
    as the edit target — closing the loop that codex
    9c431de caught on the prior plan.

## Tests

- `agent_config_round_trip_with_tool_and_initial_prompt`:
  serde tests the new fields round-trip cleanly. `None`
  values get `skip_serializing_if`.
- `load_merged_agents_reads_skeleton_tool_directly`:
  skeleton with `tool: Some(Codex)`, no session; assert
  merged entry has `tool: Some(Codex)` (not None from
  the old session-derived path).
- `clank_init_migrates_repo_scope_agents_block_to_skeletons`:
  fixture has `.clank/config.json#/agents` set; run
  `clank init`; assert each entry materialized to
  `.clank/agents/<label>/config.json` AND the `agents`
  key is removed from `.clank/config.json`.
- `clank_init_migration_is_idempotent`: run `clank init`
  twice; second run is a no-op (no warnings, no second
  migration).
- `clank_init_migration_preserves_existing_skeleton_session`:
  declaration block has `codex` AND a skeleton at
  `.clank/agents/codex/config.json` ALREADY exists with
  `session: Some(...)`. Migration merges in declaration
  fields without clobbering session.
- `clank_agent_add_writes_per_agent_skeleton`: replaces
  today's `add` integration tests that assert
  `.clank/config.json#/agents`. New assertion: the
  skeleton file exists with the right fields.
- `clank_agent_promote_writes_multiple_skeleton_files_for_repair_case`:
  pre-state: alice/bob/carol all marked master in their
  skeletons (multi-master). `promote dave`; assert all
  three skeleton files are mutated (role=reviewer) AND
  dave's skeleton has role=master. One mtime change per
  file.
- `agent_start_bootstrap_uses_skeleton_tool_field`: the
  bootstrap path reads `tool` from skeleton AgentConfig,
  not from session. The previously-renamed test
  `agent_start_session_none_with_no_tool_errors_with_hint`
  is updated: setup writes a skeleton WITH `tool: None`
  explicitly (not "no session implies no tool").

## Out of scope

- Moving other repo-wide settings (hooks, diff editor)
  to per-user. They stay tracked in `.clank/config.json`.
  If the user later decides editor preferences should be
  per-user, that's a separate plan.
- Multi-user merging — if two users want to see each
  other's agent declarations, they don't. Repo-scope
  agents are PER-USER-PER-REPO. The user-scope
  `~/.clank/config.json#/default_agents` is the only
  cross-machine sharing point.
- Versioning the `AgentConfig` schema. Adding optional
  fields (`tool`, `initial_prompt`) is backward-compat
  via `#[serde(default)]`. Older clank reading a newer
  file simply ignores the new fields.

## Acceptance

- `.clank/config.json` no longer has an `agents` key.
  The struct field is removed.
- `.clank/agents/<label>/config.json` holds all of:
  role, tool, launch, initial_prompt, session,
  auto_mode, wfw_timeout.
- `load_merged_agents` returns the same shape as today,
  sourced from skeleton scan first + user-scope
  fallback.
- `clank init` migrates an existing
  `.clank/config.json#/agents` block to per-agent
  skeletons in one pass, idempotently.
- `clank agent add` writes the per-agent skeleton; the
  repo `.clank/config.json` file is unchanged by this
  operation.
- `clank agent promote` mutates per-agent skeletons
  (one write per affected agent). The unique-master
  invariant still holds via `ensure_unique_master`.
- The bootstrap path (just shipped in
  `agent-start-bootstraps-missing-skeleton`) now reads
  `tool` from the skeleton directly. The no-tool error
  hint points at `.clank/agents/<label>/config.json`,
  the actual edit target — closing the loop that
  codex 9c431de caught.
- `cargo test --workspace` passes.

## Why this matters

Three layered wins:

1. **Removes a class of bugs.** Declaration drift
   between `.clank/config.json#/agents` and
   `.clank/agents/<label>/` was already biting (the
   `tool=None` synthesis case; the legacy-fallback path
   I trip on every plan). Merging the two sources
   eliminates the drift surface.
2. **Matches the actual ownership.** Agents are
   per-user-per-repo; the data should live in
   per-user-per-repo files (already gitignored, already
   per-machine). The tracked
   `.clank/config.json#/agents` was a mis-modeling.
3. **Smaller code surface.** `load_repo_agents`,
   `load_declared_agents`, `load_merged_agents` all
   collapse to one function. `RepoConfigFile.agents`
   field deleted. The legacy synthesis path becomes the
   only path. Net negative line count.
