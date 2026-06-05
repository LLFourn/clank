# agent-add-cli-and-repo-scope
# Repo-scope `<repo>/.clank/config.json` `agents` field + `clank agent add/remove/set-role` CLI. Deferred from agent-config-and-start.

## Problem

Two related gaps left over from `agent-config-and-start`'s rescope (FINISHED `18a5e78`):

1. **No repo-scope override of the agent set.** `~/.clank/config.json` `default_agents` exists (user-scope, seeds skeletons at `clank init`). But `<repo>/.clank/config.json` has no `agents` field for per-repo overrides. Customizing the agent set per-repo means hand-editing `<repo>/.clank/agents/<label>/config.json` files. No CLI surface.

2. **No `clank agent add/remove/set-role` CLI.** Adding an agent today is `clank as <label>` (binds session) + manually editing `<repo>/.clank/agents/<label>/config.json` for role. The deferred plan trimmed these subcommands out of scope to keep that plan focused; they belong here.

3. **The agent-declaration schema is too thin and inconsistently sourced** (codex review of 55f14f2 surfaced this). Today:
   - `DefaultAgent` (in `crates/cli/src/cli/config.rs:102-107`) is just `{label, role}`. No `tool`, no `launch`.
   - Downstream consumers (`agent_store::load_expected_reviewers`, `clank agent list`, `clank agent start`) read role from the **per-agent skeleton** (`<repo>/.clank/agents/<label>/config.json`), NOT from the declaration. And `clank agent start` reads `launch` from the skeleton too — Phase A of `agent-config-and-start` put it there.
   - Consequence: this plan can't ship `clank agent remove` while preserving feedback history. Removing the declaration entry doesn't remove the agent from gate input because the gate reads the skeleton; preserving the skeleton means the "removed" agent is still a registered reviewer.
   - Consequence: this plan can't ship `--tool` on `clank agent add` without somewhere to store the tool preference; the skeleton's `session.tool` is for BOUND state, not pre-binding metadata.

This plan unifies the schema by making the merged declaration the source of truth for role + tool + launch, demoting the per-agent skeleton to per-machine state (`auto_mode`, `wfw_timeout`, `session`).

## Approach

### Phase 1: Schema unification — declaration becomes the source of truth

Extend `DefaultAgent`:

```rust
pub struct DefaultAgent {
    pub label: AgentLabel,
    #[serde(default)]
    pub role: Role,
    /// Tool this agent runs (claude / codex). Used by spawners
    /// (zellij layouts, `clank agent start` fallback) to compose
    /// the launch line. `None` means "no preferred tool yet";
    /// `clank agent start` will fall back to the bound session's
    /// tool, OR error if neither is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<Tool>,
    /// Launch profile (command override + args + env). Same shape
    /// as the old `AgentConfig.launch`; moved here so the
    /// declaration carries everything a spawner needs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch: Option<LaunchConfig>,
}
```

Demote `AgentConfig` (skeleton) to per-machine state:
- KEEP: `auto_mode`, `wfw_timeout`, `session`.
- REMOVE: `role` (declaration is authoritative), `launch` (moved to declaration).

The skeleton's `role` field stays in the schema for backwards-compat with existing on-disk configs (Serde `#[serde(default)]` already; nothing breaks reading old files). But all readers stop consulting it. A future cleanup plan can drop it from the struct entirely.

Migration: `AgentConfig.launch` was just shipped in `agent-config-and-start`. The only on-disk caller is this codebase's `<repo>/.clank/agents/codex/config.json` (the codex agent's `launch` is currently `None` AFAIK). Plan implementer: grep for any non-None `launch` in `.clank/agents/*/config.json` post-implementation and move them to repo-scope `agents` entries.

### Phase 2: Repo-scope `agents` field

- Add `agents: Vec<DefaultAgent>` to `<repo>/.clank/config.json` schema.
- Merge semantics: repo-scope, if present, **REPLACES** the user-scope `default_agents` set for the repo (per "the local project can modify it"). If absent, fall back to user-scope.
- Strict-fail at both layers (matches current user-scope `load_default_agents` semantics — silently dropping a multi-reviewer set converts repo to auto-finalize).
- `clank init` consumes the merged set when seeding skeletons.

### Phase 3: Migrate gate / start consumers to the declaration

- `agent_store::load_expected_reviewers(repo)`: change implementation to read the merged declaration list (filter by `role == Reviewers`) instead of scanning `<repo>/.clank/agents/*/config.json`. Same return shape.
- `agent_store::load_all_agent_configs(repo)` (used by `clank agent list`): now needs to JOIN declaration + per-agent skeleton. Return shape stays `Vec<(AgentLabel, AgentConfig)>` for compat; the merged record carries `role` from declaration AND per-machine state from skeleton.
- `clank agent start <label>`: read `launch` from declaration, NOT skeleton. Session still comes from skeleton.
- `clank agent list`: source-of-truth for "registered agents" is the merged declaration. List items missing a skeleton render as "unbound" (existing behavior, just relocated).

**This is the load-bearing migration.** Without it, `clank agent remove` can't actually remove the agent from gate input. With it, the declaration's `Vec<DefaultAgent>` IS the registered-agent set; removal from the declaration removes from gate.

### Phase 4: CLI surface

- `clank agent add <label> [--role master|reviewers] [--global] [--tool claude|codex] [--launch-cmd <cmd>] [--launch-arg <arg> ...] [--launch-env KEY=VAL ...]`:
  - Default scope: repo. Writes a `DefaultAgent` entry to `<repo>/.clank/config.json`'s `agents` AND creates the per-agent skeleton at `<repo>/.clank/agents/<label>/config.json` with default per-machine state (no role, no launch — those live in the declaration).
  - `--global`: writes to `~/.clank/config.json`'s `default_agents`. No per-agent skeleton is created (user-scope skeletons don't exist today; `clank init` in each repo seeds local skeletons).
  - Refuses if `<label>` already exists at the chosen scope.
  - `--tool` defaults to `claude` if absent.
- `clank agent remove <label> [--global]`: removes the declaration entry. Per-agent dir + its feedback history stay (gate no longer consults the dir for "is this a reviewer", so the dir is just dormant history). A separate `--purge-history` flag for full removal is a follow-up.
- `clank agent set-role <label> <master|reviewers> [--global]`: edits the declaration's `role`.

### Phase 5: Transactional ordering for `clank agent add`

Two filesystem writes per add: the list entry in the chosen config.json AND the per-agent skeleton at `<repo>/.clank/agents/<label>/config.json` (or `~/.clank/agents/<label>/config.json` under `--global` if we ever ship per-user skeletons — out of scope today; user-scope `--global` writes only the list entry).

Decision (ruthless review 55f14f2): **"doctor as recovery surface" pattern**, not in-process rollback.

1. **Pre-checks (in-memory)**: label not in the list at the chosen scope; if `--global`, label also not in repo-scope's list (cross-direction collision → REFUSE). If repo-scope add and label already in user-scope's list, ALLOW + stderr notice ("repo-scope `<label>` shadows user-scope default") — REPLACE semantics naturally permit shadowing.
2. **Skeleton write first** (when target = repo): `mkdir -p` + `write_atomic` (tempfile + rename) for the per-machine skeleton. Per Phase 1, the skeleton holds ONLY per-machine state (`auto_mode`, `wfw_timeout`, `session`) — role and launch live in the declaration. If a prior `clank as <label>` binding already created the skeleton, **preserve existing `session` + `auto_mode` + `wfw_timeout` values**; only write a skeleton when none exists (or backfill missing fields with defaults). Never clobber session state from this command. Codex caught the contradiction with the old "overwrite with role + launch" instruction on b13122d.
3. **List entry write** (tempfile + atomic rename on the parent config.json) — writes the FULL `DefaultAgent` (label, role, tool, launch) per Phase 4.

If step 3 fails, the skeleton dir is left in place. **Doctor catches both inconsistency directions**:
- Existing check (already shipped): "merged declaration set non-empty but `<repo>/.clank/agents/` is empty" → Warn.
- New check (this plan): per-agent dir present but label NOT in the **merged declaration** (NOT just repo-scope `agents`) → Warn with diagnostic "found `.clank/agents/<label>/config.json` but `<label>` not in the merged agent declaration (neither repo-scope `agents` nor user-scope `default_agents`); run `clank agent add <label>` to register, or remove the directory." Codex caught the wrong-scope check on b13122d.

This accepts that single-process atomicity over two fs entries isn't free and lets doctor surface inconsistency. The alternative (best-effort rollback on step-3 failure) has its own failure modes (rollback can itself fail) and adds code complexity for a rare path.

### Phase 6: Default `--launch-cmd` behavior

When NO `--launch-*` flag is passed: declaration's `launch` field is `None` (not `Some(LaunchConfig::default())`). `agent start` already falls back to the bare tool when `launch.is_none()`. Cleaner on-disk shape; matches the existing default.

### Phase 7: Doctor checks (additions)

- **Per-agent Warn** (not aggregated) so each missing/orphan entry is actionable.
- **Missing skeleton diagnostic**: "agent `<label>` in merged declaration but `.clank/agents/<label>/config.json` missing; run `clank init` to seed the skeleton." The check walks the merged declaration (repo-scope `agents` if present, else user-scope `default_agents`), NOT just repo-scope.
- **Orphan skeleton diagnostic**: "found `.clank/agents/<label>/config.json` but `<label>` not in the merged declaration (neither repo-scope `agents` nor user-scope `default_agents`); run `clank agent add <label>` to register, or `rm -rf` the directory to remove it."

## Verified before promotion (resolved 2026-06-05)

- **Deferral line confirmed**: `crates/cli/src/cli/config.rs:120-122` says "A future plan that adds repo-scope agent management owns that policy." This plan owns it.
- **load_default_agents is user-scope-only with strict-fail semantics**: missing file = empty list, malformed = error. Rationale (per the docstring at config.rs:115-133): silently dropping a multi-reviewer `default_agents` would convert the repo to auto-finalize. Plan extends this function in place rather than adding a parallel loader — single source of truth for the merged agent set, strict-fail preserved at both layers.
- **Config.load is already layered** (`load_with_home` at config.rs:157-165: user via `apply_layer` then repo via `apply_layer`). But layering is for the lossy review/hooks settings; `default_agents` lives outside the lossy path. The fix adds a per-loader merge step inside `load_default_agents`, NOT a new field on `Config`.
- **Merge semantics PINNED to override** (repo replaces user). Rationale: per-repo "the local project can modify it" intent; an additive policy would force users to enumerate every user-scope agent again to REMOVE one. Override matches mental model. Same strict-fail semantics apply at the repo layer.
- **`--skill` / `--profile` resolved**: tool-aware sugar is rejected; this plan ships `--launch-cmd <cmd>` + `--launch-arg <arg>` (repeatable) + `--launch-env KEY=VAL` (repeatable). Tool-aware sugar (e.g. `--skill <s>` translating to `["--skill", "<s>"]` for claude / no-op for codex) couples CLI to specific tool flag conventions; cleaner to keep the CLI tool-agnostic and let users hand-edit if they want the exact ergonomic shorthand. Sugar can come as a follow-up plan if users want it.

## Out of scope

- `clank agent rename`. Edit by hand for now.
- Per-agent feedback-style customization (review cadence, etc.). Separate plan.
- Migrating existing in-flight repos. New `agents` field is purely additive; existing repos keep working via user-scope fallback.
- Dropping `AgentConfig.role` and `AgentConfig.launch` from the struct definition. Phase 3 makes readers stop consulting them; an explicit schema-cleanup plan can remove the fields entirely once we're confident nothing reads them.
- `--purge-history` flag on `clank agent remove` (full removal including feedback dirs).
- User-scope skeletons. `--global` add writes only the declaration; users still bind per-repo via `clank as <label>`.

## Acceptance

**Schema unification (Phase 1 + 3)**

- `DefaultAgent` carries `tool: Option<Tool>` + `launch: Option<LaunchConfig>` alongside `label` and `role`.
- The merged declaration (user-scope + repo-scope) is the source of truth for: which agents exist, their role, their tool, their launch profile.
- `agent_store::load_expected_reviewers` reads the declaration → gate input is driven by declarations, not skeleton presence.
- `clank agent start` reads launch from the declaration; session still comes from the skeleton.

**Repo-scope merge (Phase 2)**

- `<repo>/.clank/config.json` `agents` field overrides user-scope `default_agents` for that repo (full replace, not append).
- `clank init` in a repo with repo-scope `agents` seeds the override set, NOT the user-scope set.
- Malformed repo-scope `agents` fails closed at load time (same strict-fail semantics as user-scope `default_agents`).

**CLI surface (Phase 4)**

- `clank agent add codex --tool codex --launch-cmd codex --launch-arg --profile --launch-arg deep` writes a FULL `DefaultAgent` (label, role, tool, launch) to `<repo>/.clank/config.json`'s `agents` AND creates a per-machine skeleton at `<repo>/.clank/agents/codex/config.json` with defaults (no role, no launch).
- `clank agent add lloyd --global --role master` writes to `~/.clank/config.json` `default_agents`; no per-agent skeleton.
- `clank agent add codex` (repo-scope) when `codex` already in user-scope `default_agents`: ALLOW + stderr "repo-scope `codex` shadows user-scope default."
- `clank agent add codex --global` when `codex` already in repo-scope `agents`: REFUSE.
- `clank agent add` with no `--launch-*` flag produces declaration entry with `launch: None`.
- `clank agent remove codex` removes from the declaration, preserves the per-agent directory (and its feedback history). Critically: `load_expected_reviewers` no longer returns `codex` after removal.
- `clank agent set-role codex master` flips the role in the declaration entry.

**Doctor (Phase 7)**

- Doctor surfaces both missing-skeleton AND orphan-skeleton diagnostics, one Warn per affected label.

**Workspace**

- `cargo test --workspace` passes after all phases land.

## Tests

### Phase 1 (schema)

- `default_agent_serde_roundtrips_with_tool_and_launch`: serialize + deserialize a full `DefaultAgent { label, role, tool: Some(Claude), launch: Some(LaunchConfig{...}) }`; assert round-trip equality.
- `default_agent_deserializes_minimal_form`: `{"label": "codex", "role": "reviewers"}` parses (tool + launch default to None) — backwards compat for existing user-scope `default_agents`.
- `agent_config_skeleton_drops_launch_at_read`: deserialize an existing `<repo>/.clank/agents/<label>/config.json` that has a `launch` field; assert read succeeds (forward-compat with the old schema during migration), but `clank agent start` ignores it and reads from declaration.

### Phase 2 (repo-scope agents loader)

- `repo_scope_agents_replaces_user_scope_when_present`: write `~/.clank/config.json` `default_agents: [user_a, user_b]` + `<repo>/.clank/config.json` `agents: [repo_x]`; merged loader returns `[repo_x]` (REPLACE, not append).
- `repo_scope_agents_absent_falls_back_to_user_scope`: only user-scope set; merged loader returns user-scope.
- `malformed_repo_scope_agents_fails_closed`: invalid JSON in `<repo>/.clank/config.json`'s `agents`; load errors.
- `clank_init_with_repo_scope_agents_seeds_override_set`: temp HOME with user-scope `[user_a, user_b]` + repo with repo-scope `[repo_x]`; run `clank init`; only `repo_x`'s skeleton dir is created.

### Phase 3 (consumer migration)

- `load_expected_reviewers_reads_declaration_not_skeleton`: write a declaration with `[alice: reviewers, bob: reviewers]` but ONLY create skeleton dir for `alice`; `load_expected_reviewers` returns both `alice` AND `bob` (declaration is authoritative for gate input).
- `load_expected_reviewers_excludes_removed_label_even_if_skeleton_remains`: declaration has `[alice: reviewers]`; `<repo>/.clank/agents/bob/config.json` exists from a prior bind; `load_expected_reviewers` returns `[alice]` only (bob is NOT a registered reviewer despite the skeleton).
- `clank_agent_start_reads_launch_from_declaration`: declaration entry has `launch = { command: "claude", args: ["--skill", "ruthless"] }`; skeleton has NO launch field; `clank agent start <label> --print` composes `claude --skill ruthless --resume <id>`.

### Phase 4 (CLI surface)

- `clank_agent_add_writes_declaration_entry_and_skeleton`: `clank agent add codex --tool codex --launch-cmd codex --launch-arg --profile --launch-arg deep`; assert `<repo>/.clank/config.json`'s `agents` contains the FULL declaration (label, role, tool, launch) AND `<repo>/.clank/agents/codex/config.json` skeleton exists with per-machine defaults (no role, no launch).
- `clank_agent_add_no_launch_flags_leaves_launch_none`: `clank agent add codex --tool codex` (no `--launch-*`); declaration entry has `launch: None`.
- `clank_agent_add_global_writes_to_user_scope`: `--global` lands in `~/.clank/config.json` `default_agents`; no per-agent skeleton created at user-scope (separate concern).
- `clank_agent_add_refuses_duplicate_at_same_scope`: two adds at same scope with same label → second errors, filesystem unchanged.
- `clank_agent_add_repo_scope_shadows_user_scope_with_notice`: user-scope has `codex`; repo-scope `add codex` succeeds + stderr contains "shadows user-scope default."
- `clank_agent_add_global_refuses_when_repo_scope_has_label`: repo-scope has `codex`; `add codex --global` → REFUSE.
- `clank_agent_remove_removes_from_declaration_preserves_feedback`: pre-populate `agents/codex/feedback/<sha>.md`; `clank agent remove codex`; declaration entry gone, feedback file preserved. Critically: `load_expected_reviewers` no longer returns `codex`.
- `clank_agent_set_role_flips_role_in_declaration`: existing entry with role=reviewers; `clank agent set-role codex master`; declaration's role is master, skeleton untouched.

### Phase 7 (doctor)

- `doctor_warns_on_missing_skeleton`: `agents` lists `codex` (repo-scope) but `<repo>/.clank/agents/codex/` absent; Warn with "agent: codex" name + diagnostic mentioning `clank init`.
- `doctor_warns_on_orphan_skeleton`: `<repo>/.clank/agents/orphan/config.json` exists but `orphan` NOT in the merged declaration; Warn naming the label + diagnostic mentioning `clank agent add` AND `rm -rf`.

## Related history

- `clank-init-seeds-default-agents` (FINISHED): user-scope `default_agents` + init seeding.
- `manage-clank-agents` (FINISHED, trimmed): `clank agent list` + doctor unbound-reviewer warning.
- `agent-config-and-start` (FINISHED `18a5e78`): shipped `clank agent start`; explicitly deferred Phase 1 + Phase 3 to this plan.
