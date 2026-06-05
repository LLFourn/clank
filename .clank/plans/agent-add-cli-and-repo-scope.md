# agent-add-cli-and-repo-scope
# Repo-scope `<repo>/.clank/config.json` `agents` field + `clank agent add/remove/set-role` CLI. Deferred from agent-config-and-start.

## Problem

Two related gaps left over from `agent-config-and-start`'s rescope (FINISHED `18a5e78`):

1. **No repo-scope override of the agent set.** `~/.clank/config.json` `default_agents` exists (user-scope, seeds skeletons at `clank init`). But `<repo>/.clank/config.json` has no `agents` field for per-repo overrides. Customizing the agent set per-repo means hand-editing `<repo>/.clank/agents/<label>/config.json` files. No CLI surface.

2. **No `clank agent add/remove/set-role` CLI.** Adding an agent today is `clank as <label>` (binds session) + manually editing `<repo>/.clank/agents/<label>/config.json` for role. The deferred plan trimmed these subcommands out of scope to keep that plan focused; they belong here.

## Approach (sketch)

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

## Acceptance (sketch)

- `<repo>/.clank/config.json` with `agents` field overrides user-scope `default_agents` for that repo (full replace, not append).
- `clank init` in a repo with repo-scope `agents` seeds the override set, NOT the user-scope set.
- `clank agent add codex --tool codex --launch-cmd codex --launch-arg --profile --launch-arg deep` writes the repo agents entry AND `<repo>/.clank/agents/codex/config.json` with `launch = { command: "codex", args: ["--profile", "deep"] }`.
- `clank agent add lloyd --global --role master` writes to `~/.clank/config.json`.
- `clank agent remove codex` removes from the agents list, preserves the per-agent directory (and its feedback history).
- `clank agent set-role codex master` flips the role.
- Malformed repo-scope `agents` field fails closed at load time (same strict-fail semantics as user-scope `default_agents`).
- `cargo test --workspace` passes.

## Related history

- `clank-init-seeds-default-agents` (FINISHED): user-scope `default_agents` + init seeding.
- `manage-clank-agents` (FINISHED, trimmed): `clank agent list` + doctor unbound-reviewer warning.
- `agent-config-and-start` (FINISHED `18a5e78`): shipped `clank agent start`; explicitly deferred Phase 1 + Phase 3 to this plan.
