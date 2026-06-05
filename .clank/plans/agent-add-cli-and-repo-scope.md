# agent-add-cli-and-repo-scope
# Repo-scope `<repo>/.clank/config.json` `agents` field + `clank agent add/remove/set-role` CLI. Deferred from agent-config-and-start.

## Problem

Two related gaps left over from `agent-config-and-start`'s rescope (FINISHED `18a5e78`):

1. **No repo-scope override of the agent set.** `~/.clank/config.json` `default_agents` exists (user-scope, seeds skeletons at `clank init`). But `<repo>/.clank/config.json` has no `agents` field for per-repo overrides. Customizing the agent set per-repo means hand-editing `<repo>/.clank/agents/<label>/config.json` files. No CLI surface.

2. **No `clank agent add/remove/set-role` CLI.** Adding an agent today is `clank as <label>` (binds session) + manually editing `<repo>/.clank/agents/<label>/config.json` for role. The deferred plan trimmed these subcommands out of scope to keep that plan focused; they belong here.

## Approach

### Phase 1: Repo-scope `agents` field

- Extend `<repo>/.clank/config.json` schema with `agents: Vec<DefaultAgent>` (same shape as `~/.clank/config.json`'s `default_agents`).
- Merge semantics: repo-scope, if present, **REPLACES** the user-scope `default_agents` set for the repo (override, not append; per lloyd's "the local project can modify it"). If absent, fall back to user-scope.
- `clank init` consumes the merged set when seeding skeletons.
- `clank doctor` warns if the merged set is non-empty but `<repo>/.clank/agents/` is empty.

### Phase 2: CLI surface

- `clank agent add <label> [--role master|reviewers] [--global] [--tool claude|codex] [--launch-cmd <cmd>] [--launch-arg <arg> ...] [--launch-env KEY=VAL ...]`:
  - Default scope: repo (writes to `<repo>/.clank/config.json` `agents` + creates `<repo>/.clank/agents/<label>/config.json` skeleton; `--launch-*` args populate `LaunchConfig.command`, `args`, `env`).
  - `--global`: writes to `~/.clank/config.json` `default_agents` instead.
  - Refuses if `<label>` already exists at the chosen scope.
  - `--tool` defaults to `claude` if absent; bound via the skeleton's session shape.
- `clank agent remove <label> [--global]`: removes from the agents list. Does NOT delete `<repo>/.clank/agents/<label>/` (preserves feedback history; `--purge-history` flag is a separate plan if needed).
- `clank agent set-role <label> <master|reviewers> [--global]`: in-place role edit.

### Phase 3: Transactional ordering for `clank agent add`

Two filesystem writes per add: the list entry in the chosen config.json AND the per-agent skeleton at `<repo>/.clank/agents/<label>/config.json` (or `~/.clank/agents/<label>/config.json` under `--global` if we ever ship per-user skeletons — out of scope today; user-scope `--global` writes only the list entry).

Decision (ruthless review 55f14f2): **"doctor as recovery surface" pattern**, not in-process rollback.

1. **Pre-checks (in-memory)**: label not in the list at the chosen scope; if `--global`, label also not in repo-scope's list (cross-direction collision → REFUSE). If repo-scope add and label already in user-scope's list, ALLOW + stderr notice ("repo-scope `<label>` shadows user-scope default") — REPLACE semantics naturally permit shadowing.
2. **Skeleton write first** (when target = repo): `mkdir -p` + `write_atomic` (tempfile + rename). Idempotent at-rest if the dir already exists from a prior `clank as <label>` binding — overwrite the config.json with the new role + launch fields.
3. **List entry write** (tempfile + atomic rename on the parent config.json).

If step 3 fails, the skeleton dir is left in place. **Doctor catches both inconsistency directions**:
- Existing check (already shipped): "merged set non-empty but `<repo>/.clank/agents/` is empty" → Warn.
- New check (this plan): per-agent dir present but label NOT in repo-scope `agents` list → Warn with diagnostic "found `.clank/agents/<label>/config.json` but `<label>` not in repo-scope `agents` list; run `clank agent add <label>` to register, or remove the directory."

This accepts that single-process atomicity over two fs entries isn't free and lets doctor surface inconsistency. The alternative (best-effort rollback on step-3 failure) has its own failure modes (rollback can itself fail) and adds code complexity for a rare path.

### Phase 4: Default `--launch-cmd` behavior

When NO `--launch-*` flag is passed: skeleton's `launch` field is `None` (not `Some(LaunchConfig::default())`). `agent start` already falls back to the bare tool when `launch.is_none()`. Cleaner on-disk shape; matches the existing default.

### Phase 5: Doctor checks (additions)

- **Per-agent Warn** (not aggregated) so each missing/orphan entry is actionable.
- **Missing skeleton diagnostic**: "agent `<label>` in repo-scope `agents` list but `.clank/agents/<label>/config.json` missing; run `clank init` to seed the skeleton."
- **Orphan skeleton diagnostic**: "found `.clank/agents/<label>/config.json` but `<label>` not registered in repo-scope `agents` list; run `clank agent add <label>` to register, or `rm -rf` the directory to remove it."

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

## Acceptance

- `<repo>/.clank/config.json` with `agents` field overrides user-scope `default_agents` for that repo (full replace, not append).
- `clank init` in a repo with repo-scope `agents` seeds the override set, NOT the user-scope set.
- `clank agent add codex --tool codex --launch-cmd codex --launch-arg --profile --launch-arg deep` writes the repo agents entry AND `<repo>/.clank/agents/codex/config.json` with `launch = { command: "codex", args: ["--profile", "deep"] }`.
- `clank agent add lloyd --global --role master` writes to `~/.clank/config.json`.
- `clank agent remove codex` removes from the agents list, preserves the per-agent directory (and its feedback history).
- `clank agent set-role codex master` flips the role.
- Malformed repo-scope `agents` field fails closed at load time (same strict-fail semantics as user-scope `default_agents`).
- `clank agent add codex` (repo-scope) when `codex` already in user-scope `default_agents`: ALLOW + stderr "repo-scope `codex` shadows user-scope default."
- `clank agent add codex --global` when `codex` already in repo-scope `agents`: REFUSE.
- `clank agent add` with no `--launch-*` flag produces skeleton with `launch: None`.
- Doctor surfaces both missing-skeleton AND orphan-skeleton diagnostics, one Warn per affected label.
- `cargo test --workspace` passes.

## Tests

### Phase 1 (repo-scope agents loader)

- `repo_scope_agents_replaces_user_scope_when_present`: write `~/.clank/config.json` with `default_agents: [user_a, user_b]` + `<repo>/.clank/config.json` with `agents: [repo_x]`; assert the merged loader returns `[repo_x]` (REPLACE, not append).
- `repo_scope_agents_absent_falls_back_to_user_scope`: only user-scope set; assert merged loader returns user-scope set.
- `malformed_repo_scope_agents_fails_closed`: invalid JSON in `<repo>/.clank/config.json`'s `agents`; assert error (matches user-scope strict-fail semantics).
- `clank_init_with_repo_scope_agents_seeds_override_set`: temp HOME with user-scope `[user_a, user_b]` + repo with repo-scope `[repo_x]`; run `clank init`; assert only `repo_x`'s skeleton dir is created.

### Phase 2 (CLI surface)

- `clank_agent_add_writes_list_entry_and_skeleton`: `clank agent add codex --tool codex --launch-cmd codex --launch-arg --profile --launch-arg deep`; assert `<repo>/.clank/config.json`'s `agents` array contains the entry AND `<repo>/.clank/agents/codex/config.json` contains `launch = { command: "codex", args: ["--profile", "deep"] }`.
- `clank_agent_add_no_launch_flags_leaves_launch_none`: `clank agent add codex --tool codex` (no `--launch-*`); assert skeleton has `launch: None` (key absent from JSON).
- `clank_agent_add_global_writes_to_user_scope`: `--global` lands in `~/.clank/config.json` `default_agents`, not in `<repo>/.clank/config.json`.
- `clank_agent_add_refuses_duplicate_at_same_scope`: two adds at same scope with same label → second errors, filesystem unchanged after the error.
- `clank_agent_add_repo_scope_shadows_user_scope_with_notice`: user-scope already has `codex`; repo-scope `add codex` succeeds + stderr contains "shadows user-scope default."
- `clank_agent_add_global_refuses_when_repo_scope_has_label`: repo-scope has `codex`; `clank agent add codex --global` → REFUSE.
- `clank_agent_remove_preserves_per_agent_directory`: pre-populate `agents/codex/feedback/<sha>.md`; `clank agent remove codex`; assert list entry gone, feedback file preserved.
- `clank_agent_set_role_flips_role_in_place`: existing entry with role=reviewers; `clank agent set-role codex master`; assert role updated in both the list entry AND the per-agent config.

### Phase 5 (doctor)

- `doctor_warns_on_missing_skeleton`: `agents` lists `codex` but `<repo>/.clank/agents/codex/` absent; Warn with "agent: codex" name + diagnostic mentioning `clank init`.
- `doctor_warns_on_orphan_skeleton`: `<repo>/.clank/agents/orphan/config.json` exists but `orphan` not in `agents` list; Warn naming the label + diagnostic mentioning `clank agent add` AND `rm -rf`.

## Related history

- `clank-init-seeds-default-agents` (FINISHED): user-scope `default_agents` + init seeding.
- `manage-clank-agents` (FINISHED, trimmed): `clank agent list` + doctor unbound-reviewer warning.
- `agent-config-and-start` (FINISHED `18a5e78`): shipped `clank agent start`; explicitly deferred Phase 1 + Phase 3 to this plan.
