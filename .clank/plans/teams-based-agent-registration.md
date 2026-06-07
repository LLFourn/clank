# teams-based-agent-registration

Replaces the model from `agents-declaration-is-user-local`
(currently shipped) which conflated "agent exists in this
repo" with "per-agent skeleton file exists on disk." That
model has UX problems:

1. **REPLACE semantics**: the moment you `clank agent add foo`
   in a repo, ALL user-scope defaults vanish. "Defaults +
   local addition" requires re-adding every default. Wrong
   for the common case.
2. **`.empty` sentinel sprawl**: defending the explicit-empty
   intent across migration / init bootstrap / `clank as` /
   `clank auto` required five separate sentinel-aware
   checkpoints. That's a code smell — the architecture is
   leaking the implementation detail.
3. **`.clank/config.json` is tracked but nothing in it is
   genuinely team-shared**. Every field — review policy,
   hooks, diff editor, the now-gone agents block — is
   per-user. Tracking it is a mismodeling.

The cleanup is two coupled changes: (a) un-track
`.clank/config.json`, (b) move "registration" out of the
per-agent skeleton files and into a team-membership model
in user-scope.

## The new model

### User-scope (`~/.clank/config.json`, per-user, dotfile-synced)

Two top-level keys:

```json
{
  "agents": {
    "claude":   { "tool": "claude" },
    "codex":    { "tool": "codex" },
    "grok":     { "tool": "claude" },
    "ruthless": { "tool": "claude", "launch": { "command": "claude", "args": ["--skill", "ruthless"] } }
  },
  "teams": {
    "default": {
      "master": "claude",
      "reviewers": ["codex"]
    },
    "dev": {
      "master": "claude",
      "reviewers": ["codex", "ruthless"]
    },
    "research": {
      "master": "grok",
      "reviewers": ["claude"]
    }
  }
}
```

- `agents` is a label-keyed dictionary of agent
  DESCRIPTIONS only. No role, no team affiliation. Just
  what tool to spawn and how. Reused across teams.
- `teams` is a label-keyed dictionary of compositions. Each
  team picks ONE master and N reviewers by referring to
  agent names. Roles are per-team (a single agent can be
  master in one team, reviewer in another, absent from a
  third).
- The `default` team is just one entry in the table. No
  magic. `clank init` without `--team` uses it.

### Repo-scope (`<repo>/.clank/config.json`, gitignored, per-user-per-repo)

```json
{
  "team": "dev",
  "master_override": "codex",
  "local_agents": [
    "ruthless",
    { "label": "alice", "tool": "claude", "role": "reviewer" }
  ]
}
```

- `team`: name of a team in user-scope `teams`.
- `master_override` (optional): label of an agent who
  takes the master role IN THIS REPO ONLY. Written by
  `clank promote <agent>`. Doesn't modify the team's
  user-scope master config. Resolution swaps: this label
  becomes master, the team's original master gets demoted
  to reviewer (joining the reviewer list, not dropped).
- `local_agents`: optional list of additions for THIS repo
  only. Each entry is either:
  - A string (by-name reference): resolved against user-
    scope `agents`. Lets the user pull in an agent that's
    not part of their chosen team for this specific repo.
    Role defaults to reviewer; can be overridden by a
    fully-inline form.
  - A fully-specified inline object: label + tool +
    optional launch + role. For agents the user wants ONLY
    in this repo, not declared globally.

### Repo per-agent state (`<repo>/.clank/agents/<label>/config.json`, gitignored)

Purely STATE: `session`, `auto_mode`, `wfw_timeout`. NO
declaration fields (no role, no tool, no launch).

**The presence of this file does NOT determine
registration.** Registration is computed from user-scope
team + local_agents. State files for agents that aren't in
the registered set become orphan state — kept on disk but
ignored by gates / lists / spawners.

`clank doctor` surfaces orphan state files so the user can
clean them up.

## Registration resolution

For a given repo, the registered set is computed as:

1. Read `<repo>/.clank/config.json#/team` → team name.
2. Look up the team in `~/.clank/config.json#/teams`.
3. Master = `agents[teams.<team>.master]`. Reviewers =
   `agents[teams.<team>.reviewers[i]]` for each i.
4. Append `local_agents`:
   - By-name reference: look up label in user-scope
     `agents`. Add as reviewer.
   - Inline object: add verbatim.
5. **Apply `master_override`** (if set in repo config):
   - new_master = registered agent matching the override
     label (looked up in the set built so far — must be
     present, else error).
   - prev_master = the team's original master.
   - Master = new_master.
   - Reviewers = (existing reviewers ∪ {prev_master}) −
     {new_master}.
   - Same swap semantic as today's `clank agent promote`
     repair — displaced master joins reviewers, doesn't
     drop.
6. The resulting list IS the registered set for this repo
   (replaces every current `load_merged_agents` caller).

## CLI surface changes

- **`clank init --team <name>`**: writes the team field to
  `<repo>/.clank/config.json` AND adds `/config.json` to
  `.clank/.gitignore` AND crashes if it detects the old
  `agents` block from `agents-declaration-is-user-local`
  with a clear "this repo is in the old format; re-run
  `clank init` to migrate." NO silent migration.
- **`clank init --team <name>`** is ALSO the way to CHANGE
  the team on an already-initialized repo. Idempotent +
  re-runnable: rewrites the team field to the new value,
  leaves `local_agents` and per-agent state files
  untouched. The registered set changes immediately on
  the next gate computation. Useful for "I started this
  repo under `default` but it's really `research` work —
  swap it."
- **`clank init`** (no `--team`): on a fresh repo, uses
  team `default`. On an already-initialized repo, no-op
  (preserves the existing team selection rather than
  silently reverting to `default`).
- **`clank agent`** — agent DESCRIPTIONS (the "who"):
  - `add --global <label> --tool <tool> [--launch-cmd ...]`:
    write to user-scope `agents` table.
  - `add <label> --tool <tool>` (no `--global`): add inline
    to repo's `local_agents`.
  - `add <label>` (by-name, no `--tool`): add by-name
    reference to repo's `local_agents`; looks up
    user-scope `agents` at registration time.
  - `remove --global <label>`: remove from user-scope. Also
    removes from any team that referenced it (refuses if
    the label is a master of some team without `--force`).
  - `remove <label>` (no `--global`): drop from repo's
    `local_agents`.
  - `list`: registered for THIS repo (runs the full
    resolution algorithm — team + local_agents).
  - `start <label>`: unchanged behavior (launches tool).

- **`clank team`** — team COMPOSITIONS (the "which set"):
  All operations implicitly target user-scope (teams live
  nowhere else). No `--global` flag needed.
  - `list`: list all teams in user-scope.
  - `create <name>`: create an empty team (no master, no
    reviewers; must be populated before it's useful).
  - `delete <name>`: remove team. Refuses if any
    `<repo>/.clank/config.json#/team` currently references
    it (clank doesn't track which repos use which teams,
    so this either requires user confirmation or uses
    `--force` to delete blindly).
  - `set-master <team> <agent>`: change a team's master.
    Replaces the existing master (the unique-master
    invariant is per-team, automatic).
  - `add-reviewer <team> <agent>`: pull agent into team as
    reviewer. Refuses if agent isn't declared in
    user-scope `agents`.
  - `remove-reviewer <team> <agent>`: drop from team's
    reviewers list.
  - `show <team>`: print master + reviewers.

- **`clank promote <agent>`** STAYS — but its scope and
  data model change.
  - **Scope**: repo-scope only. It's a per-repo master
    OVERRIDE, not a team mutation.
  - **Data**: writes `master_override: <agent>` to
    `<repo>/.clank/config.json`. The team's user-scope
    master config is untouched.
  - **Resolution with override**: master =
    `master_override`; reviewers = (team reviewers ∪
    local_agents) − new_master + previous_master_demoted.
    Same swap semantic as today's `clank agent promote`
    repair behavior — the displaced master joins the
    reviewer list, doesn't drop.
  - **No `--global` flag**: master changes at the team
    level go through `clank team set-master`, not
    `promote`.

  Distinction from `clank team set-master`:
  - `clank team set-master dev codex`: "from now on, my
    dev team's master is codex." Global preference;
    every repo using `dev` flips.
  - `clank promote codex`: "in THIS repo, codex drives.
    My dev team master stays as it is for other repos."
    Local override.

- **`clank config`** stays as-is for flat key-value
  settings that don't have rich structure (hooks, diff
  editor). The three concerns are then cleanly
  partitioned: `agent` (who exists), `team` (who's
  grouped + with what role), `config` (everything else
  flat).
- **`clank as`, `clank auto`**: their sentinel-aware
  refusal code is REMOVED. The sentinel goes away
  entirely — there's no need for it in the new model
  because state files don't drive registration. Empty
  registration is just "team has no agents" or "team is
  unset."

## What goes away

- `.clank/agents/.empty` sentinel and ALL its lifecycle
  code (sentinel-aware checks in init bootstrap, `clank
  as`, `clank auto on/off`, etc.). The new model doesn't
  need it.
- `DefaultAgent` in `crates/cli/src/cli/config.rs` —
  superseded by `AgentDescription` + `TeamComposition`.
- The `RepoConfigFile.agents` deprecated field.
- `clank agent add`'s skeleton write — skeletons no
  longer carry declaration fields.
- The legacy-migration code in `clank init` from the
  prior plan (`agents-declaration-is-user-local`) — old
  repos crash with "re-run clank init" instead of being
  auto-migrated.

## Migration story for old-format repos

`clank init` detects EITHER:
- `<repo>/.clank/config.json#/agents` is `Some(...)` (the
  pre-`agents-declaration-is-user-local` shape), OR
- `<repo>/.clank/agents/<label>/config.json` has the
  `tool` / `initial_prompt` declaration fields populated
  (the post-`agents-declaration-is-user-local` shape).

In either case, crash with:

```
this repo is in an old clank format. Re-run `clank init
--team <name>` to migrate to the team-based model. Any
existing per-agent state (sessions, auto_mode) will be
preserved.
```

The re-run does:
1. Strip declaration fields (tool, initial_prompt) from
   per-agent skeleton files. Keep session, auto_mode,
   wfw_timeout.
2. Delete `.clank/agents/.empty` if present.
3. Write `<repo>/.clank/config.json` with the chosen team
   (default if not specified).
4. Add `/config.json` to `.clank/.gitignore`.
5. Print: "migrated repo to team-based model. Registered
   agents are now determined by the `<team>` team in
   `~/.clank/config.json`."

## Why this matters

Three architectural wins:

1. **Default agents flow into repos by default.** "Use
   my normal agents + one local addition" is just
   `clank init --team dev` + adding to `local_agents`.
   The current REPLACE model makes this require manual
   re-adding of every default.
2. **The sentinel goes away.** Five separate sentinel-
   aware code paths collapse to zero. The model that
   needed the sentinel (skeleton-as-declaration) is gone.
3. **Separation of concerns in user-scope.** Agent
   descriptions (what tool, what launch) and team
   compositions (who reviews who) are now orthogonal.
   One agent participates in multiple teams with
   different roles, declaratively, without duplication.

Plus the user-scope file becomes the natural unit for
dotfile sync — your agents and teams flow across machines
together.

## Open questions

- `local_agents` by-name reference: does it default to
  reviewer role, or pick up the role from whatever team
  context exists? Probably reviewer-by-default since the
  team's master is already chosen.
- Should the team selector in repo-scope have a way to
  override the team's master for THIS repo (e.g.,
  `"team_master": "codex"`)? Or is that overengineering?
- What's the bind UX when no team is set yet?
  `clank as <label>` before `clank init --team dev` —
  refuse? Auto-init with default team? Probably refuse
  + suggest init.

## Why this is a NEW plan, not a revision

`agents-declaration-is-user-local` shipped and serves as
the back-compat point we crash against. This plan
supersedes that one's data model entirely; it's not a
small revision. Separate plan.

## Status

Stub — not yet sized. Two follow-up architectural
revisits queued in conversation:
- This (team-based registration + un-track config.json).
- The cleanup of how `expected_reviewers` is computed
  multi-user (codex previously noted gate divergence
  between users; team model resolves this since both
  users on the same team see the same composition).
