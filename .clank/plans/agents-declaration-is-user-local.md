# agents-declaration-is-user-local

The `agents` array in `.clank/config.json` is a per-USER choice,
not a team-shared decision. Today the file is tracked, which
forces all collaborators on a repo to use the same agent set
(claude/codex/ruthless or whatever). Different users may want
different agent panels — one user runs claude+codex, another
runs claude+ruthless, etc.

## What needs to happen

Split `.clank/config.json`:

- **Tracked, team-shared**: hooks config, possibly diff editor
  defaults, anything else that's repo-wide policy.
- **Local, per-user**: the `agents` declaration.

Options to consider (decide in the plan):

1. **`.clank/config.local.json`** (gitignored, optional). Same
   shape as `.clank/config.json`. Local fields override tracked
   fields. Agents live here. Mirrors how `.claude/settings.json`
   vs `.claude/settings.local.json` works.
2. **`.clank/agents.local.json`** — a dedicated file just for
   the agents declaration. Cleaner separation but adds a third
   config source to the merge logic
   (`load_merged_agents`: user-scope `~/.clank/config.json#/default_agents`
   → repo-local `.clank/agents.local.json` → tracked
   `.clank/config.json#/agents` (deprecated)).
3. **Remove repo-scope agents entirely**: lean on user-scope
   `~/.clank/config.json#/default_agents` exclusively. Users
   who want repo-specific agents put repo-aware logic in their
   user-scope config somehow (cwd matcher? out of scope for
   clank?).

Option 1 feels right — it generalizes to other repo settings
that might be local-overridable later (editor command,
WFW timeout, etc.).

## Surfaces

- `crates/cli/src/cli/config.rs`: `load_merged_agents`,
  `load_repo_agents`, `load_declared_agents` — all need a
  third source.
- `crates/cli/src/init_facts.rs`: `CLANK_GITIGNORE_BODY` —
  add `/config.local.json`.
- `crates/cli/src/cli/agent.rs`: `add` / `remove` / `promote`
  — `--global` flag already targets user-scope; need a new
  default or `--local` flag to target the per-user-per-repo
  file. The current default (repo-scope tracked) becomes
  deprecated.
- Migration: if `.clank/config.json#/agents` is set, `clank init`
  (or a one-off `clank agent migrate`) moves it to
  `.clank/config.local.json` and removes from the tracked
  file.

## Why this matters

The current shape encodes "agents are a repo property,"
which is empirically wrong — agents are a user property
that's sometimes constrained by the repo. The fix makes the
model match reality and removes a class of friction (users
fighting over what gets committed to `.clank/config.json`).

Also: shipping this means we can ALSO un-track existing
repos' `.clank/config.json#/agents` content via the migration
without breaking team workflows.

## Open questions

- Does the `hooks` field stay tracked, or also move to local?
  Hooks are team-policy IMO (consistent across the team =
  consistent review cadence), so tracked.
- Does `diff.editor.command` stay tracked? Probably local —
  editor preferences are personal.
- Migration UX: warn + auto-move, or require explicit opt-in?

## Status

Stub captured for later — not yet sized or queued.
Lloyd 2026-06-07: noted while looking at `~/docs/trading`
where `.clank/config.json` was tracked. Will revisit once
the zellij plans clear.
