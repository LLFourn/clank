# teams-based-agent-registration

## Implementation status (lloyd 2026-06-09)

Phases 1-6c shipped. The new model is functional
end-to-end. Lloyd then chose the HARD CUT (Option A):
**delete all legacy agent-declaration code paths.** No
coexistence. After the cut:

- Per-agent skeleton (`.clank/agents/<label>/config.json`)
  is STATE ONLY: `session`, `auto_mode`, `wfw_timeout`. The
  declaration fields (`role`, `tool`, `launch`,
  `initial_prompt`) are GONE from the skeleton — they live
  in user-scope `agents` (descriptions) + per-team role
  assignment.
- Registration is ALWAYS the team resolver. A repo with no
  `team` field errors with "run `clank init --team`".
- `default_agents`, `DefaultAgent`, `load_merged_agents`,
  `load_default_agents`, `load_repo_agents`,
  `load_skeleton_agents`, `load_expected_reviewers`,
  `ensure_unique_master`, `ensure_unique_master_via_skeletons`,
  the legacy `migrate_legacy_agents_block*`, and the
  `.empty` sentinel + all its lifecycle code are DELETED.
- `clank agent add/remove`, `clank promote` operate only on
  the new schema (no dual-write, no `--global`-to-legacy).

This repo (clank itself) was migrated to the new model
first (user-scope `agents`+`teams`, repo `team: "default"`)
so the dogfooded review loop survived the deletion.

---

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
    "ruthless": { "tool": "claude", "launch": { "command": "claude", "args": ["--skill", "ruthless"] } }
  },
  "teams": {
    "default": {
      "master": "claude",
      "commit_reviewers": ["codex"]
    },
    "dev": {
      "master": "claude",
      "commit_reviewers": ["codex"],
      "gate_reviewers":   ["ruthless"]
    }
  }
}
```

(Examples use only `claude` and `codex` as tool names —
clank doesn't support other tools yet. The `tool` field
is a typed enum, not a free string. Future plan:
`tool-specific-launch-shortcuts` lets you write
`"tool": "claude", "skills": ["ruthless"]` instead of the
verbose `launch` form below — out of scope for this plan.)

- `agents` is a label-keyed dictionary of agent
  DESCRIPTIONS only. No role, no team affiliation. Just
  what tool to spawn and how. Reused across teams.
- `teams` is a label-keyed dictionary of compositions. Each
  team picks ONE master and TWO reviewer sets by tier:
  - `commit_reviewers`: review every commit on the plan.
    Master waits for them per-commit.
  - `gate_reviewers`: only review when ALL commit-reviewers
    have voted approve OR finished — they weigh in at gate
    transition moments (Approved, Finished). Used for
    deep / architectural checks at structural moments
    rather than per-commit churn.
  Roles are per-team (a single agent can be master in one
  team, commit-reviewer in another, gate-reviewer in a
  third). Both reviewer lists are optional; see Gate
  state machine below for the empty-set rule.
- The `default` team is just one entry in the table. No
  magic. `clank init` without `--team` uses it.

### Repo-scope (`<repo>/.clank/config.json`, gitignored, per-user-per-repo)

```json
{
  "team": [
    { "include": "dev" },
    "ruthless",
    { "agent": "alice", "review": "gate" },
    { "label": "bob", "tool": "claude", "review": "commit" }
  ],
  "promoted": "codex"
}
```

Or the common case (just one user-scope team, no extras):

```json
{
  "team": "dev"
}
```

- **`team`**: either a string (sugar for `[{"include":
  "<name>"}]`) or an array. Array entries are
  untagged-enum variants (see Typed serde section):
  1. **Bare string** (`"ruthless"`) — by-name reference
     to a user-scope agent. Review tier defaults to
     `commit`.
  2. **`{ "include": "<team-name>" }`** — pulls in a
     team's full composition (master + both reviewer
     lists). At most ONE include per array; multiple
     includes are out of scope for v1 (use locals to
     add specific agents instead).
  3. **`{ "agent": "<label>", "review": "..." }`** —
     by-name reference with explicit review tier. Used
     when you want a user-scope agent at a non-default
     tier for this repo.
  4. **Fully inline** (`{ "label": "...", "tool":
     "...", "review": "...", ... }`) — for agents you
     want ONLY in this repo, not declared globally.
     Fields: `label`, `tool`, optional `launch`,
     optional `role`, optional `review` (default
     `commit`).
- **`promoted`** (optional): label of an agent who takes
  the master role IN THIS REPO ONLY. Written by `clank
  promote <agent>`. Doesn't modify the included team's
  user-scope master config. Resolution swaps: this label
  becomes master, the included team's original master
  gets demoted to commit-reviewer (joining
  `commit_reviewers`, not dropped).
- **Master designation is single-source-of-truth**: an
  inline local entry cannot directly designate master.
  The only paths to master are:
  - `clank team set-master <team> <agent>` (user-scope
    team config), OR
  - `clank promote <agent>` (writes `promoted` at repo
    scope).
  Role-field handling on inline entries:
  - **Omitted**: defaults to `reviewer`.
  - **`"reviewer"`**: valid (explicit form of the
    default — encouraged for readability).
  - **`"master"`**: REJECTED at parse time with an
    actionable error pointing at `clank promote <label>`
    as the right surface.
  To make a local agent the repo's master: add them in
  the `team` array AND then `clank promote <label>`.
- **`review` field values**: `"commit"` or `"gate"`.
  Names what the agent reviews (commits or gates), not
  which "tier" they belong to. Same semantic, friendlier
  name.

### Repo per-agent state (`<repo>/.clank/agents/<label>/config.json`, gitignored)

Purely STATE: `session`, `auto_mode`, `wfw_timeout`. NO
declaration fields (no role, no tool, no launch).

**The presence of this file does NOT determine
registration.** Registration is computed from the `team`
field (team includes + local entries) plus `promoted`.
State files for agents that aren't in the registered set
become orphan state — kept on disk but ignored by gates
/ lists / spawners.

`clank doctor` surfaces orphan state files so the user can
clean them up.

## Registration resolution

For a given repo, the registered set is computed by
folding over `team` entries in order:

1. Read `<repo>/.clank/config.json#/team` — either a
   string (treat as single-element array
   `[{"include": "<name>"}]`) or an array.
2. Initialize empty `master = None`,
   `commit_reviewers = []`, `gate_reviewers = []`.
3. For each entry in the array:
   - **`{ "include": "<team-name>" }`**: look up the
     team in `~/.clank/config.json#/teams`. If
     `team.master` is `Some(label)`, set
     `master = Some(agents[label])`. Append
     `agents[team.commit_reviewers[i]]` to
     `commit_reviewers`. Append
     `agents[team.gate_reviewers[i]]` to
     `gate_reviewers`. At most one `include` per array;
     reject the second with a parse-time error.
   - **Bare string `"<label>"`**: look up label in
     user-scope `agents`. Add to `commit_reviewers`.
   - **`{ "agent": "<label>", "review": "..." }`**:
     look up label in user-scope. Add to the named
     review list (`commit_reviewers` or
     `gate_reviewers`).
   - **Inline** (`{ "label", "tool", "review"?, ... }`):
     add verbatim to the named review list (default
     `commit_reviewers`).
4. **Apply `promoted`** (if set):
   - new_master = registered agent matching the
     `promoted` label (looked up in the set built so
     far — must be present somewhere, else error).
   - prev_master = current `master` (may be None if
     the included team has no master designated).
   - Master = new_master.
   - commit_reviewers = (existing commit_reviewers ∪
     (prev_master.into_iter())) − {new_master}.
   - gate_reviewers = existing gate_reviewers − {new_master}.
   - (When prev_master is None, no demotion happens —
     `promoted` is the first master designation.)
5. **Validate master is set** (codex 4c79ed2 catch): if
   `master` is still `None` after step 4, the repo's team
   composition has no master and `promoted` wasn't set
   at repo scope. Error with hint:
   ```
   team `<name>` has no master designated. Either set a
   team-level master via `clank team set-master <name>
   <agent>`, or designate a per-repo master via
   `clank promote <agent>`.
   ```
   This validation runs at registration-resolution time,
   NOT at config deserialize time — empty-team scaffolding
   via `clank team create` is allowed as a transient
   state.
6. The resulting (master, commit_reviewers,
   gate_reviewers) tuple IS the registered set for this
   repo.

## Gate state machine with commit/gate tiers

`compute_gate` in `crates/core/src/wait.rs` extends to
five states. The two-tier reviewer model is FIRST-CLASS
in the core type, not hidden in `wfw`'s wake logic.

States (transition order):

| State | Condition |
|---|---|
| `Unreviewed` | At least one commit-reviewer hasn't voted approve/finished (no Request-Changes filed by anyone) |
| `ChangesRequested` | Any reviewer (commit OR gate) voted Request-Changes |
| `ApprovedPendingGate` | All commit-reviewers voted approve/finished AND ≥1 gate-reviewer hasn't voted approve/finished (no Request-Changes) |
| `Approved` | All commit-reviewers AND all gate-reviewers voted approve OR finished AND ≥1 voted only Approve (not Finished) |
| `Finished` | All commit-reviewers AND all gate-reviewers voted Finished |

Wake conditions:

| State | Who wakes |
|---|---|
| `Unreviewed` | Commit-reviewers who haven't voted |
| `ChangesRequested` | Master → revise |
| `ApprovedPendingGate` | Gate-reviewers who haven't voted |
| `Approved` | Master → continue |
| `Finished` | Master → finalize |

Edge cases pinned:

1. **Empty `commit_reviewers`, non-empty `gate_reviewers`**:
   "all commit-reviewers approved" is vacuously true on
   the empty set, so state transitions to
   `ApprovedPendingGate` immediately on first commit.
   Gate-reviewer fires per-commit. Functionally identical
   to gate-reviewer being a commit-reviewer. **No
   special-case code** — set semantics produce it.
2. **Empty both `commit_reviewers` and `gate_reviewers`**:
   gate stays at `Approved` (matching today's
   zero-reviewer behavior). Initial draft said
   `Finished` here, but that would noise every commit
   in a master-only repo with "ready to finalize"
   — master keeps working and runs `clank finish` when
   they decide. Implementation corrected at commit
   8cb01b6; codex 8cb01b6 caught the plan/code drift.
3. **Gate-reviewer votes Request-Changes**: trumps state
   regardless of tier — gate transitions to
   `ChangesRequested`. Master revises. After re-commit,
   cycle restarts from the new commit's `Unreviewed`
   state.
4. **Gate-reviewer approves but doesn't Finish**: state =
   `Approved` (not `Finished`). Master sees "continue."
   Eventually master signals reviewers to upgrade verdict
   to Finished; when all of both tiers Finish → state =
   `Finished`.
5. **Gate-reviewer votes before commit-reviewers** (a
   gate-reviewer who's "watching" votes early): verdict
   is stored but the gate state ignores it until
   commit-reviewers are all-approved/finished. The vote
   sits as latent state. State machine reads
   commit-reviewers FIRST.
6. **Same agent in both tier lists**: rejected at parse
   time. One tier per team per agent.
7. **Master in either reviewer list**: rejected at parse
   time. Master is the master slot, not a reviewer.

## Config errors propagate via anyhow

**No special "panic everywhere except init" handling.**
The typed serde struct + anyhow propagation is enough.
Lloyd's directive (2026-06-08): just let serde deserialize
into the proper struct; if it fails, anyhow propagates
the error up through `main` with a clear message. No
ad-hoc detection code, no detect-and-panic, no separate
loader path.

**What this looks like in practice** (corrected to match
the SHIPPED code — ruthless pin on 29483bf/d772410; the
earlier "deserialize fails with `invalid type: sequence`"
sub-story was factually wrong):

- Old `agents-declaration-is-user-local` repo config had a
  top-level `"agents": [...]` array. The new repo-scope
  `RepoConfigFile` has NO `agents` field, so that array
  lands in the `#[serde(flatten)] extra` catchall. **Parse
  SUCCEEDS** — there is no `Vec<LocalAgentEntry>` field for
  the array to fail against. The legacy key is preserved on
  round-trip but is invisible to registration.
  - The repo therefore has no `team` field set →
    `try_resolve_via_team` returns `None` → resolver-backed
    commands (`wfw`, `finish`, `promote`) get the
    `no_team_configured()` error: `this repo has no team
    configured. Run \`clank init --team <name>\` to set
    one.` Read-only renderers (`status`, `html`) degrade to
    empty reviewers instead (see `reviewer_tiers_for_render`).
  - The genuine **parse-failure** path is when a
    strongly-typed field has the wrong shape — e.g.
    `"team": 42` (not a string or array). Then serde errors,
    `.with_context("parsing <path>")` adds the file path,
    anyhow propagates to `main`. The `a9772ef` dispatch
    tests pin both paths
    (`try_resolve_via_team_with_fails_closed_on_malformed_repo_config`
    + `..._legacy_agents_array_lands_in_extra_returns_none`).
- `no_team_configured()` (in `agent_store.rs`) is the single
  authority for the no-team message; there is no bespoke
  "this is a legacy config" detection anywhere.

**`clank init`** (note: the LegacyRepoConfigFile migration
fallback was NOT built — the hard cut deleted the legacy
types entirely rather than carry a migration path). A repo
with a stale `agents` array in `extra` is simply re-`init`ed
with `--team`, which writes the `team` field; the leftover
`agents` key rides the `extra` catchall harmlessly until
overwritten. No automated legacy migration ships.

**Scope**: config files only. Per-agent skeletons
(`.clank/agents/<label>/config.json`) and the `.empty`
sentinel are STATE, not config, and are tolerated as
harmless (the new resolution doesn't read them).

User-scope `~/.clank/config.json` gets the same
treatment: a legacy `default_agents` key would either
fail typed deserialization (anyhow propagates) or land
in `extra` (gets ignored). No `clank init --user`
migration — users hand-edit their own dotfiles.

## Read-only renderers degrade; workflow commands hard-error

A boundary that emerged during implementation (ruthless NIT
on f6feba2 asked it be recorded): when a repo has no `team`
configured, the two classes of command behave differently.

- **Workflow / mutation commands** — `clank wfw`, the stop-hook,
  `clank finish`, `clank promote`, `clank demote`, `clank purge`
  (anything that drives or gates the review loop) — **hard-error**
  via `agent_store::load_reviewer_tiers` →
  `no_team_configured()`: "this repo has no team configured. Run
  `clank init --team <name>`." You can't run the workflow without
  a team, so refusing is correct.
- **Read-only renderers** — `clank status`, `clank html`,
  `clank open`, `clank doctor` — **degrade**: no team → empty
  reviewer tiers → the gate computes as zero-reviewer (Approved)
  and the timeline/status renders anyway. A brand-new repo with
  commits but no team must still render; crashing a viewer on
  config absence is wrong.

The degrade has ONE structural point: `status::StatusSnapshot`
calls `agent_store::reviewer_tiers_for_render` (never-errors;
no team / misconfigured team → empty tiers), and BOTH `clank
status` and `clank html` route through `StatusSnapshot`, so the
single decision covers every renderer. `doctor` reports the
no-team condition as a finding; `open` catches the error into a
display message. This makes "renderers degrade, workflow
hard-errors" a property of the architecture, not a thing each
command must remember.

## `clank init` assigns nothing (lloyd 2026-06-09)

`clank init` is **pure repo setup**: scaffold `.clank/`
(gitignore), install the `post-rewrite` hook + claude
permissions, and — with `--team <name>` — write the repo's
`team` field. It does NOT bind sessions, read agent env vars
(`CLAUDE_CODE_SESSION_ID` / `CODEX_THREAD_ID`), prompt for an
identity, or assign a role. Session binding is `clank as`'s
sole responsibility; roles are team-derived. (The `--yes` flag
and the `bootstrap_agent_identity` step are gone.)

## CLI surface changes

**AS SHIPPED (hard cut, lloyd 2026-06-09):** the
init/migration design below was simplified during
implementation. There is NO old-format migration, NO
default-team fallback, and NO session bind in `clank init`.
The authoritative behavior:

- **`clank init`** — pure repo setup: scaffold `.clank/`,
  install the `post-rewrite` hook + claude perms, warn on
  globally-excluded paths. Assigns nothing (no session bind,
  no env lookup, no role). See "`clank init` assigns
  nothing" above.
- **`clank init --team <name>`** — additionally validates the
  team exists in user-scope `~/.clank/config.json#/teams` and
  writes `team: "<name>"` to `<repo>/.clank/config.json` (+
  gitignores the file). Idempotent: re-running rewrites the
  `team` field.
- **`clank init` with no `--team`** — does NOT pick a
  `default` team and does NOT migrate old configs. A repo left
  without a `team` simply has none; workflow commands then
  error with `no_team_configured()` until you re-run with
  `--team`. A stale legacy `agents` array (from the
  pre-cut shipped model) lands in the new schema's `extra`
  catchall and is ignored — no automated cleanup.

The original plan envisioned an old-format detection +
migration path (legacy `agents` block removal, `default`
team fallback, the `.empty` sentinel). The hard cut deleted
the legacy types outright instead, so none of that ships;
the historical design is preserved here only as context.

- **`clank agent`** — agent DESCRIPTIONS (the "who"):
  - `add --global <label> --tool <tool> [--launch-cmd ...]
    [--initial-prompt ...]`: write to user-scope `agents`
    table.
  - `add <label> --tool <tool> [--review commit|gate]`
    (no `--global`): append fully-inline entry to repo's
    `team` array. Default review kind is `commit`.
  - `add <label> [--review gate]` (no `--tool`, no
    `--global`): append by-name entry to repo's `team`
    array; looks up user-scope `agents` at registration
    time. If `--review` is set to a non-default value,
    writes the `{ "agent": "<label>", "review": "..." }`
    shape; otherwise writes the bare-string shape.
  - `remove --global <label>`: remove from user-scope.
    Also removes from any team that referenced it
    (refuses if the label is a master of some team
    without `--force`).
  - `remove <label>` (no `--global`): drop from repo's
    `team` array (any entry referencing this label).
  - `list`: registered for THIS repo (runs the full
    resolution algorithm — team includes + entries +
    `promoted`). Shows review kind for each reviewer.
  - `start <label>`: unchanged behavior (launches tool).

- **`clank team`** — team COMPOSITIONS (the "which set").
  All operations implicitly target user-scope. No
  `--global` flag needed.
  - `list`: list all teams in user-scope.
  - `create <name>`: create an empty team (no master, no
    reviewers in either tier; must be populated before
    it's useful).
  - `delete <name>`: remove team. Refuses if any
    `<repo>/.clank/config.json#/team` currently references
    it (clank doesn't track which repos use which teams,
    so this either requires user confirmation or uses
    `--force` to delete blindly).
  - `add <team> <agent> [--review commit|gate]`: add an
    agent to the team. Default review kind is `commit`.
    Refuses if the agent isn't declared in user-scope
    `agents`. Refuses if the agent is already in the
    team (any review kind). To move an agent between
    kinds, use `remove` then `add`.
    Naming: just `add` (not `add-reviewer`) because
    master is a SEPARATE designation via `set-master` —
    you don't `add-reviewer` and `add-master`, you add
    agents and one of them is set to master.
  - `remove <team> <agent>`: remove agent from team.
    Finds them in whichever reviewer tier they're in.
    Refuses to remove a team's master (use `set-master`
    to designate a different agent first, or `delete`
    the team).
  - `set-master <team> <agent>`: designates master. If
    the agent was previously in either reviewer tier
    list, they're moved out of it (one agent can't be
    both master and reviewer in the same team). The
    previous master is moved to `commit_reviewers` (NOT
    dropped — they're still a registered member of the
    team, just at the most-engaged tier).
  - `show <team>`: prints master + both tier reviewer
    lists. Format:
    ```
    team `dev`:
      master:           claude
      commit reviewers: codex
      gate reviewers:   ruthless
    ```

- **`clank promote <agent>`** STAYS — but its scope and
  data model change.
  - **Scope**: repo-scope only. It's a per-repo master
    designation, not a team mutation.
  - **Data**: writes `promoted: <agent>` to
    `<repo>/.clank/config.json`. The team's user-scope
    master config is untouched.
  - **Resolution with `promoted`**: master = `promoted`;
    commit_reviewers = (existing commit_reviewers ∪
    {previous_master}) − {new_master}; gate_reviewers =
    existing gate_reviewers − {new_master}. Same swap
    semantic as today's `clank agent promote` repair
    behavior — the displaced master joins
    commit_reviewers, doesn't drop.
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

## Typed serde throughout

Every config file round-trips through a typed struct with
`#[derive(Deserialize, Serialize)]`. The discipline
(codex a7db7a2 catch — sharpened from the earlier
overstatement):

- **No whole-document `serde_json::Value` parsing**. Every
  key the schema knows about is typed.
- **No ad-hoc `Value` mutation** in production paths. The
  prior plan's `init.rs:90` migration code parsed the
  whole config as `Value` and used `obj.remove("agents")`
  on it — that's the regression this plan fixes.
- **`Value` IS allowed in `#[serde(flatten)] extra:
  BTreeMap<String, serde_json::Value>` catchalls**. The
  catchall serves two purposes: forward-compat preserves
  unknown keys across round-trip, AND it's the typed
  surface where we detect leftover-keys like the legacy
  `agents`. Removing the catchall would lose forward-
  compat AND break detection.

Typed structs:

```rust
// User-scope (~/.clank/config.json)
#[derive(Deserialize, Serialize)]
pub struct UserConfigFile {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub agents: BTreeMap<AgentLabel, AgentDescription>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub teams: BTreeMap<String, TeamComposition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<ReviewSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hooks: Option<HooksSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<DiffConfig>,
    // Forward-compat catchall + leftover-key detection point.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

#[derive(Deserialize, Serialize)]
pub struct AgentDescription {
    pub tool: Tool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch: Option<LaunchConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_prompt: Option<String>,
}

#[derive(Deserialize, Serialize)]
pub struct TeamComposition {
    /// Optional at the storage layer so `clank team create
    /// <name>` can scaffold an empty team. Validated as
    /// REQUIRED at USE time (codex 4c79ed2 catch — was
    /// non-optional, conflicted with CLI's empty-create
    /// flow). Resolution: if a repo's `team` includes a
    /// team whose `master` is None AND `promoted` isn't
    /// set at repo scope, error with a hint pointing at
    /// `clank team set-master <team> <agent>` or
    /// `clank promote <agent>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub master: Option<AgentLabel>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commit_reviewers: Vec<AgentLabel>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gate_reviewers: Vec<AgentLabel>,
}

#[derive(Deserialize, Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum ReviewKind {
    Commit,
    Gate,
}

// Repo-scope (<repo>/.clank/config.json)
#[derive(Deserialize, Serialize)]
pub struct RepoConfigFile {
    /// Either a string (sugar for `[{"include": <name>}]`)
    /// or an array of TeamEntry. Serde untagged enum
    /// distinguishes via `serde_json::Value::is_string()`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team: Option<TeamField>,
    /// Optional repo-scope master promotion. Renamed from
    /// `master_override` per lloyd 2026-06-08.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub promoted: Option<AgentLabel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<ReviewSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hooks: Option<HooksSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<DiffConfig>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

#[derive(Deserialize, Serialize)]
#[serde(untagged)]
pub enum TeamField {
    /// Sugar: `"team": "dev"` == `"team": [{"include": "dev"}]`.
    Single(String),
    /// Full form: array of mixed entries.
    Array(Vec<TeamEntry>),
}

#[derive(Deserialize, Serialize)]
#[serde(untagged)]
pub enum TeamEntry {
    /// `{ "include": "<team-name>" }` — pull in a team
    /// composition.
    Include { include: String },
    /// `{ "agent": "<label>", "review": "..." }` —
    /// by-name reference with explicit review kind.
    ByName {
        agent: AgentLabel,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        review: Option<ReviewKind>,
    },
    /// Fully inline: `{ "label", "tool", "review"?, ... }`.
    Inline(InlineAgent),
    /// Bare string: `"ruthless"`. Equivalent to
    /// `{ "agent": "ruthless" }`.
    BareString(AgentLabel),
}

#[derive(Deserialize, Serialize)]
pub struct InlineAgent {
    pub label: AgentLabel,
    pub tool: Tool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch: Option<LaunchConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_prompt: Option<String>,
    /// Optional explicit role. Validated AFTER deserialize:
    /// `Some(Role::Master)` is rejected with an actionable
    /// error pointing at `clank promote`. `None` and
    /// `Some(Role::Reviewer)` both resolve to reviewer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<Role>,
    /// What kind of review they do. Default `commit` if
    /// omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<ReviewKind>,
}
```

**Old-format handling**: see the "Config errors propagate
via anyhow" section above for the authoritative story. The
short version (corrected — the old `agents: [...]` array
does NOT fail to parse): repo-scope `RepoConfigFile` has no
`agents` field, so a legacy `agents` array lands in the
`extra` flatten catchall and the parse SUCCEEDS with `team:
None`. Resolver-backed commands then surface
`no_team_configured()`; a genuine wrong-type field (e.g.
`team: 42`) is the only thing that makes
`serde_json::from_str::<RepoConfigFile>` itself error, which
`.with_context("parsing <path>")` + anyhow propagate to
`main`.

**What was NOT built**: the `LegacyRepoConfigFile`
migration fallback. The hard cut (lloyd 2026-06-09) deleted
the legacy types outright instead of carrying an automated
migration. A stale repo is fixed by re-running
`clank init --team <name>`, which writes the `team` field;
the leftover `agents` key rides `extra` harmlessly. No
detect-and-migrate code ships.

**Migration write path**: init mutates the typed
`RepoConfigFile` in memory (drops the legacy block, sets
`team`), then `serde_json::to_string_pretty(&typed)`
serializes it back through typed serde. The `extra`
flatten preserves any forward-compat keys this clank
version doesn't know about.

The existing `no_json_literal_config_writes_in_tests` lint
already covers config WRITES; this plan extends discipline
to READS by never `Value`-parsing whole config files.

## What goes away

- All sentinel-aware code paths in the production code:
  the checks in `bootstrap_agent_identity`, `clank as`,
  `clank auto on/off`, `clank agent add/remove`'s
  sentinel lifecycle. The new resolution never reads the
  sentinel file; if it's present on disk from an
  unmigrated repo, the new code ignores it (the file is
  harmless dead bytes).
- `DefaultAgent` in `crates/cli/src/cli/config.rs` —
  superseded by `AgentDescription` + `TeamComposition`.
- The `RepoConfigFile.agents` deprecated field.
- `clank agent add`'s skeleton write — skeletons no
  longer carry declaration fields. The skeleton becomes
  purely state.
- The legacy-migration code in `clank init` from the
  prior plan (`agents-declaration-is-user-local`) —
  replaced by the simpler config.json-only cleanup
  described above.

**What stays on disk** (potentially, from old repos):
- `.clank/agents/<label>/config.json` files with leftover
  declaration fields. Sessions are sacred; init never
  touches these.
- `.clank/agents/.empty` if present. New code ignores
  it; we don't clean it up because that's still a
  per-agent-dir write.

## Why this matters

Three architectural wins:

1. **Default agents flow into repos by default.** "Use
   my normal agents + one local addition" is just
   `clank init --team dev` + `clank agent add <label>`
   (which appends to the `team` array).
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

- **Bind UX when no team is set yet**: `clank as <label>`
  before `clank init --team dev` — refuse? Auto-init
  with default team? Leaning refuse + suggest
  `clank init`, mirroring how the just-shipped
  `agents-declaration-is-user-local` handles the
  no-declaration case. Pin during implementation.

## Why this is a NEW plan, not a revision

`agents-declaration-is-user-local` shipped and serves as
the back-compat point we crash against. This plan
supersedes that one's data model entirely; it's not a
small revision. Separate plan.

## Multi-user scope (codex 66751c8 pin)

This plan does NOT solve multi-user gate alignment.
Teams live in each user's `~/.clank/config.json` — they
share a NAME convention, not a definition. Two users on
the same repo can each `clank init --team dev` and have
completely different `dev` compositions on each machine,
so gate states still diverge.

Concretely: alice's `dev` = {claude (master), codex
(reviewer)}, bob's `dev` = {codex (master), claude
(reviewer), ruthless (reviewer)}. Same repo, same commit,
different gate state on each machine.

**Scope**: single-user-multi-machine via dotfile sync.
The user's `~/.clank/config.json` flows to all their
machines; `dev` means the same thing across their own
environments. Multi-user gate alignment is OUT OF SCOPE
and would require a separate tracked snapshot/version
mechanism (e.g., a tracked
`<repo>/.clank/team_snapshot.json` capturing the team
composition at a specific commit so reviewers see the
same expected set). That mechanism is a separate plan if
multi-user becomes a real use case; today clank's actual
workflow is single-user-per-repo and this plan is
honest about that.

## Status

Stub — not yet sized.
