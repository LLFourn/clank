# repo-agents-no-team

The repo has a flat LIST OF AGENTS (each with a role), not a "team".
"Team" is a GLOBAL-only concept: a named template = a saved list of
agents. Adding an agent to the repo is ONE command.

## Problem (revises unify-agent-team-config)

The unify refactor gave the REPO config a `team: TeamComposition`
separate from its `agents` map, and split "add an agent" into `clank
agent add` (define) + `clank team add` (compose). At repo scope there's
exactly one team and no reuse, so that split is pure friction (two
commands; orphan-able definitions) and "team" is a misleading local
concept. (lloyd, converged after going in circles: "teams is a global
concept and is just a template for a list of agents.")

## Core model

- **Repo config = the operating roster**: a list/map of agents, each
  carrying its DEFINITION (tool, launch, initial_prompt) AND its ROLE
  (master | commit-reviewer | gate-reviewer). Exactly one master
  (validated at resolve). NO separate `team` field, NO separate
  define/compose step. This is "just a list of agents."
- **"Team" = a GLOBAL template only**: `~/.clank/config.json#/teams` maps
  a name → a list of agents-with-roles. Used to SEED a repo
  (`init --team <name>` copies the list down) and CAPTURED from a repo
  (`team save <name>` writes the repo roster up). Teams do not exist at
  repo scope.
- **Global `agents` library KEPT (lloyd)**: `~/.clank/config.json#/agents`
  remains a registry of reusable named agent DEFINITIONS — so you can add
  an agent to a repo BY NAME: `clank agent add <name>` copies that global
  description into the repo roster. Global has BOTH `agents` (the by-name
  pool) and `teams` (roster templates).

## CLI

- **`clank agent add <name> [--review commit|gate]`** — ONE command,
  adds `<name>` to the repo roster with a role (default commit-reviewer):
  - if `<name>` is in the global `agents` library → COPY its description
    into the repo roster (the by-name path — lloyd's main case);
  - with `--tool <claude|codex>` → define a fresh agent inline AND add it
    to the roster (no library entry needed).
  Replaces today's `agent add` + `team add` (one step, role included).
- **`clank agent add <name> --global --tool <claude|codex> [launch…]`** —
  add/declare a reusable definition in the GLOBAL library (no repo
  change); this populates the by-name pool.
- **`clank agent set-master <label>`** — set the repo master (moved from
  `clank team set-master`; previous master demoted to commit-reviewer).
- **`clank agent remove <label>`** — drop from the roster (no separate
  definition to leave behind).
- **`clank agent list`** — the roster (already exists).
- **Remove** `clank team add` / `team remove` / `team set-master` /
  `team show` (the repo-team ops — team isn't a repo concept now).
- **`clank team`** = global templates ONLY: `save <name>` (repo roster →
  template), `list`, `delete <name>`, `show <name>`.
- `clank init --team <name>` seeds the repo roster from a template.

## Resolved (lloyd): keep the global agents library

GLOBAL keeps a separate `agents` definition library — NOT self-contained
teams. Rationale: you want to add a single agent to a repo BY NAME
(`clank agent add <name>`), which copies that global description into the
repo roster. So global has BOTH `agents` (reusable named definitions =
the by-name pool) and `teams` (named roster templates). Repo `agent add`
resolves `<name>` from the library (copy-down), or `--tool` defines
inline. The def/compose split is removed only at REPO scope (one-command
add); the global library stays.

## Resolver

Build the registered set (master + commit/gate reviewers) DIRECTLY from
the repo roster's role field — no `TeamComposition` indirection. Keep
the same `RegisteredSet` output so workflow callers are untouched.

## No backwards compat / rollout

Old-shape repo configs (BOTH the legacy `team: "name"` AND the just-
shipped `{agents, team}` shape) fail-closed → re-init. NOTE: every repo
I migrated to `{agents, team}` an hour ago must be re-migrated to the
flat roster — operational sweep with the freshly-built binary BEFORE
`cargo install` ([[install-breaking-binary-after-config-migration]]).

## Testing (in-process; no binary spawning — [[no-binary-spawning-tests]])

- `agent add` (one command) puts an agent on the roster with the right
  role; `agent set-master` sets master + demotes; `agent remove` drops.
- Resolver builds the registered set from the roster (master + tiers);
  errors: no master, master-also-reviewer, etc.
- `team save` captures the roster; `init --team` seeds it; round-trip.
- Old `{agents, team}` and legacy `team:"name"` configs fail-closed with
  the re-init hint.

## Non-goals

- Live-team activation (panes/sessions) — still separate.
- The status-masks-unresolvable-team-as-approved bug — separate.
