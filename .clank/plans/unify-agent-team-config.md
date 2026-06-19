# unify-agent-team-config

Collapse the agent/team config into ONE self-contained model. Same
building-block structs at both scopes; the only difference is that
global holds many teams (templates) and a repo holds exactly one.

## Problem

The current model fuses two orthogonal axes incoherently:
- Repo team is a bespoke `TeamField`/`TeamEntry` array
  (`Include`/`ByName`/`Inline`/bare-string in `teams_config.rs`) that
  fuses agent DEFINITIONS (inline agents) with team MEMBERSHIP.
- Global config uses a DIFFERENT representation (`agents` map +
  `teams` map).
- `clank team` edits global only; `clank agent` edits repo-or-global
  via `--global` (a flag that switches concept, not just scope); master
  is split across `clank agent promote` (repo) and `clank team
  set-master` (global).

## Core model

Two structs, identical in both scopes:
- `AgentDescription` — a definition (tool, launch, initial_prompt).
- `TeamComposition` — `{ master, commit_reviewers[], gate_reviewers[] }`,
  all by NAME.

Containers:
- **Repo config**: `{ agents: Map<label, AgentDescription>, team:
  TeamComposition }` — exactly one team. Self-contained.
- **Global config**: `{ agents: Map<..>, teams: Map<name,
  TeamComposition> }` — many teams = templates.

Rules:
- Team entries are name-refs into the SAME config's `agents` map. NO
  runtime fallback to global — a repo resolves master/reviewers
  entirely within its own `agents`.
- Global config is ALWAYS templates: read only by `init`, written only
  by `save`.
- Inline agents are GONE. `promoted` is GONE — repo master is
  `team.master`.
- Collisions are checked only at the copy seams (`save`), not at local
  `agent add`; a repo's `agents` namespace is independent until publish.

**No backwards compat (lloyd).** Old-shape configs are NOT migrated:
they fail-closed with an error pointing at `clank init --team <name>`.
Re-initting existing repos is post-finish operational cleanup, not plan
code.

## M1 — schema unification + resolver

- Replace repo `TeamField`/`TeamEntry`/`IncludeEntry`/`ByNameEntry`/
  `InlineAgent`/bare-string with `RepoConfigFile { agents, team }`
  reusing `AgentDescription`/`TeamComposition`.
- Resolver (`agent_store`, `resolve_registered_set`): resolve
  master/reviewers against the repo `agents` map ONLY; fail-closed if a
  referenced name is missing OR the config is the old shape (message →
  `clank init --team`).
- Drop `promoted`; master = `team.master`.
- **BOOTSTRAP (ruthless ad14e7f — design requirement, not just
  rollout):** the no-compat fail-close needs a from-scratch escape, or
  migration is circular (`init --team <name>` needs a NEW-shape global
  template that won't exist until the first `team save`, and the user
  global is also old-shape). So bare `clank init` (no `--team`) MUST
  write a VALID, possibly-empty new-shape `RepoConfigFile` (empty
  `agents`, empty `team`) WITHOUT requiring any global template. The
  from-scratch path: `clank init` → `agent add` + `team set-master` /
  `team add` (M2) → operational → `team save` mints the first template.
- Resolver fail-closes with DISTINCT messages: old-shape config →
  "re-run `clank init`"; valid-but-empty team (no master) → "set up the
  team (`clank team set-master …`)". The empty case is normal for a
  freshly-bootstrapped repo, not an error to panic on.
- Tests: resolve `{agents, team}` → registered set; missing-ref errors;
  old-shape errors with the re-init hint; empty bootstrapped config
  parses + resolves to a clear no-master error.

## M2 — command reshuffle

- `clank agent {add,list,remove,show}` = the DEFINITION registry;
  `--global` selects WHERE the def is stored (global vs repo `agents`),
  nothing else. No team mutation.
- `clank team {add,remove,set-master,show}` = the repo's operating
  team; refs must exist in repo `agents` (`team add <name>` of a name
  that's only global copies the def down first).
- `clank team {list,delete}` = the global template library.
- Retire global-scoped `team create`/`add`/`set-master` (replaced by
  compose-locally + `save`) and `agent promote` (→ `team set-master`).
- Tests: `agent add` (repo|`--global`) writes the right `agents` map;
  `team add`/`set-master` mutate the repo team and validate refs.

## M3 — the bridge: save / init / export

- `clank team save <name>`: copy repo `team` → global `teams[<name>]`;
  merge referenced repo agent defs → global `agents`, HARD ERROR on any
  name whose global body differs; confirm if `teams[<name>]` exists.
- `clank init --team <name>`: copy global `teams[<name>]` + referenced
  agents → repo config; confirm-before-overwrite if the repo already
  has a team (`--force` skips). This is the "update local config" path.
- `clank export`: serialize the (self-contained) repo config to stdout.
- Tests: save merges + errors on conflicting agent body + confirms team
  overwrite; init copies down + overwrite-confirm; export round-trips.

## Testing (in-process; no binary spawning — [[no-binary-spawning-tests]])

All milestones tested against in-process config structs + temp dirs;
never spawn the clank binary.

## Rollout (operational, NOT reviewable plan code)

After finish + `cargo install`, the new binary rejects old-shape
configs. Re-init clank's OWN repo immediately, then sweep `~/src` for
other clank repos and `clank init --team <name>` each.

## Non-goals

- No migration/compat code for old configs (deliberate — lloyd).
- Live-team ACTIVATION (spawning a zellij pane + binding a session for a
  newly-added agent) — the original "add to the running team"
  ergonomics — is separate; this plan is the config model + CLI only.
