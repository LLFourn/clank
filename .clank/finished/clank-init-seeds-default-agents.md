# clank-init-seeds-default-agents
# `clank init` pre-creates agent skeletons from `~/.clank/config.json`

## Problem

After `clank init`, a new repo has `.clank/plans/` and `.clank/.gitignore` but **no registered reviewers**. Under the all-reviewers gate that shipped in `all-reviewers-gate`, an empty registered-reviewer set means master auto-finalizes — no review required.

To register a reviewer today, the user has to:
1. Launch the agent CLI in the new repo (e.g. `codex` in a fresh pane).
2. Inside it, type `clank as codex`.
3. Inside it, type `clank auto on --role reviewers`.

Repeat for every reviewer in every new repo. For a user with a stable set of agents (master + codex + ruthless), this is the same manual dance every time.

The `~/.clank/config.json` file already exists for user-scope settings (hook commands). Extending it with a `default_agents` list and having `clank init` consume it makes new-repo setup ergonomic.

## Scope discipline

**This plan is the smallest valuable piece** of the broader `manage-clank-agents` (which is still queued and covers schema migration, `clank agent add/remove/list`, doctor checks, and gate-projection data-source migration). This plan:

- Adds **only** the user-scope `default_agents` field and `clank init`'s consumption of it.
- Does **not** introduce repo-scope `agents` array changes — the repo config keeps its existing `master` field.
- Does **not** introduce `clank agent` CLI subcommands.
- Does **not** change gate-projection inputs.
- Does **not** modify `clank as` or `clank auto` semantics.

## Approach

### 1. User-scope config schema

Extend the existing `~/.clank/config.json` schema with a `default_agents` field:

```json
{
  "hooks": { ... existing ... },
  "default_agents": [
    { "label": "codex",    "role": "reviewers" },
    { "label": "ruthless", "role": "reviewers" }
  ]
}
```

Field schema:
- `label` (string, required): the agent label to register.
- `role` ("master" or "reviewers"): the agent's default role. Defaults to "reviewers" if omitted (the most common case).

Notably **omitted from this plan**:
- `tool` field — what CLI to invoke. The user's launcher (`open-worktree.sh`, future `clank open zellij`, etc.) decides this; `clank init` doesn't launch agents.
- `system_prompt_append` — deferred to a future plan that actually consumes the prompt.

The simpler the schema, the smaller the breaking-change surface when it later grows.

### 2. `clank init` consumes `default_agents`

After scaffolding `.clank/plans/` and `.clank/.gitignore`, `clank init`:

1. Reads `~/.clank/config.json` (lossy: missing file = empty list; malformed = error with diagnostic).
2. For each entry in `default_agents`:
   - If `<repo>/.clank/agents/<label>/config.json` already exists, skip (idempotent).
   - Otherwise, write `<repo>/.clank/agents/<label>/config.json` with:
     ```json
     {
       "auto_mode": "off",
       "role": "<role>"
     }
     ```
     (No `session` field. That gets populated when the agent first runs `clank as <label>`.)

3. Print a one-line summary: `seeded N agent skeletons: codex, ruthless`.

### 3. No backward-compat work needed

- Existing repos: `clank init` is idempotent. Running it on a repo that already has `.clank/agents/codex/config.json` skips that label (the existing file wins).
- User without `~/.clank/config.json`: behaves exactly as today — no agents seeded.
- User with `~/.clank/config.json` but no `default_agents`: same — no agents seeded.
- User who later wants to remove a default: edits `~/.clank/config.json`. No new clank surface needed.

### 4. Failure modes

Strict loading (fail-closed) for malformed user config — same principle as `load_expected_reviewers` from `all-reviewers-gate`:

- If `~/.clank/config.json` exists but won't parse, `clank init` errors with the parse diagnostic. Don't silently drop `default_agents` and seed nothing (that would feel like success).
- If `default_agents` contains an invalid label, error with the offending entry.
- If a per-agent config write fails (disk full, permission), error and stop. Don't partially seed.

## Tests

In `clank-cli` (integration):

- `clank init` in a fresh repo with `~/.clank/config.json` containing two reviewers → both `.clank/agents/<label>/config.json` files exist with `role: reviewers`, no `session` field.
- `clank init` is idempotent: running twice doesn't overwrite an agent config that already has a `session` field.
- `clank init` with no `~/.clank/config.json` → no per-agent dirs created (current behaviour preserved).
- `clank init` with `~/.clank/config.json` but no `default_agents` field → same as above.
- `clank init` with malformed `~/.clank/config.json` → errors with the parse diagnostic.
- `clank init` with one master + one reviewer in `default_agents` → both seeded with correct roles.

## Out of scope

- Repo-scope `agents` array schema (stays in `manage-clank-agents`).
- `clank agent add/remove/list/set-role` subcommands (stays in `manage-clank-agents`).
- Gate-projection data source migration (stays in `manage-clank-agents`).
- `clank doctor` checks for unbound reviewers (stays in `manage-clank-agents`).
- `tool` field on agent definitions — what CLI to invoke. Launchers decide this; clank doesn't.
- `system_prompt_append` field — defer to a follow-up plan with a concrete consumer.
- Auto-bind on first launch (i.e., having `clank` notice an unbound seeded agent and try to register it). Manual `clank as` stays the registration mechanism.

## Acceptance

- `~/.clank/config.json` carries `default_agents` and `clank` parses it.
- `clank init` in a fresh repo with user-scope `default_agents` produces matching `.clank/agents/<label>/config.json` files.
- The seeded skeletons satisfy the strict `load_expected_reviewers` introduced in `all-reviewers-gate` — `clank wfw` recognizes the registered reviewers immediately, before any of them runs `clank as`.
- Existing repos and users without user-scope defaults see no behaviour change.
- `cargo test --workspace` passes; `cargo fmt --all --check` clean.
