# team-members-ref-library
# Team members reference the agents library

## Problem

In `~/.clank/config.json` a team (`teams.<name>`) is a `Roster` —
`BTreeMap<AgentLabel, RosterAgent>` — where every member carries a
*full* definition (`tool` / `launch` / `initial_prompt`) plus its
`role`. The `agents` library already stores those same definitions
(role-free `AgentDescription`s). So a team member duplicates its
library entry: `ruthless` appears once in `agents` and again, byte
for byte plus a `--agent` launch, inside `teams.default`. Edit the
library and the team silently drifts. There is no single source of
truth for "what is the ruthless agent".

The user wants team members to **reference** library agents by name
and carry only the role, while still allowing a **custom** inline
member for a team-only agent that isn't in the library.

## Model

A team member is a sum type — exactly the two kinds the user named:

```rust
/// A team-template member: a reference into the `agents` library,
/// or a full inline definition for a team-only agent.
#[serde(untagged)]
pub enum TeamMember {
    /// Inline custom definition (a team-only agent not in the
    /// library). Same shape as a repo roster entry.
    Inline(RosterAgent),
    /// Reference a library agent (`UserConfigFile.agents`), carrying
    /// only the role. `tool`/`launch`/`initial_prompt` resolve from
    /// the library at consume time.
    Ref(TeamRef),
}

#[serde(deny_unknown_fields)]
pub struct TeamRef {
    /// Library agent to pull the definition from. Defaults to the
    /// member's KEY; set only to alias a differently-named library
    /// agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentLabel>,
    pub role: RosterRole,
}

pub type TeamRoster = BTreeMap<AgentLabel, TeamMember>;
```

`UserConfigFile.teams` becomes `BTreeMap<String, TeamRoster>`.

## HARD INVARIANT: the repo config is not touched AT ALL

The repo's `.clank/config.json` is out of scope, full stop. This
change is confined to `~/.clank/config.json#/teams` and the types
backing it. Concretely:

- `Roster` and `RosterAgent` are **unchanged** (no new fields, no new
  variants, identical serde). The repo config's `agents` stays a
  `BTreeMap<AgentLabel, RosterAgent>` of full inline entries — it is
  the self-contained operating roster; there is no library to
  reference there, and refs are NOT accepted there.
- The `Ref` form exists **only** inside `UserConfigFile.teams`. It is
  resolved into a concrete `Roster` of full inline `RosterAgent`s
  *before* anything is written to a repo.
- `init --team` still writes the **exact same** repo-config wire
  shape it writes today (full inline agents). A team saved as refs
  and a team saved inline must produce a **byte-identical** repo
  `.clank/config.json` — resolution happens entirely in the home-
  config layer.
- No repo-config reader/writer changes:
  `agent_store::{load_repo_config_required, repo_config_if_valid}`,
  `resolve_registered_set`, `try_resolve_via_team_with`,
  `RepoConfigFile`, and the repo `agent add/promote/remove` paths are
  untouched.

If any part of the implementation would change the repo config
schema, its wire format, or what `init --team` writes into a repo,
that is a defect against this plan.

### Untagged discrimination

`Inline` is listed first and `RosterAgent` **requires** `tool`, so a
ref value (`{"role":"gate"}` / `{"agent":"x","role":"gate"}`) — which
has no `tool` — cannot accidentally match `Inline`; it falls through
to `Ref`. An inline value (`{"tool":"claude","role":"gate"}`) matches
`Inline` directly. `deny_unknown_fields` on `TeamRef` gives a clean
error for a mistyped ref. Wire shapes:

```json
"ruthless": { "role": "gate" }                       // ref, key = library name
"reviewer": { "agent": "ruthless", "role": "gate" }  // ref, aliased
"oneoff":   { "tool": "claude", "role": "commit",    // inline custom
              "launch": { "args": ["--agent","x"] } }
```

## Resolution

Add to `teams_config.rs`:

- `TeamMember::role(&self) -> RosterRole` — role regardless of kind.
- `TeamMember::resolve(&self, key, library) -> Result<RosterAgent, TeamResolveError>`
  — `Inline` → clone; `Ref` → look up `library[agent.unwrap_or(key)]`,
  build via `RosterAgent::from_description(desc, role)`. A dangling
  ref (target not in the library) → `TeamResolveError` naming the
  member and the missing library agent.
- `resolve_team(team: &TeamRoster, library: &BTreeMap<AgentLabel, AgentDescription>) -> Result<Roster, TeamResolveError>`
  — resolve every member.

## Consumer changes

1. **`init.rs::load_user_team_roster`** (the `init --team` copy-down
   — the one place a team becomes a repo roster): deserialize the
   selected team value as `TeamRoster`, also read the file's `agents`
   library, call `resolve_team` → `Roster`, write that concrete
   snapshot into the repo. A dangling ref fails closed with an
   actionable message (which member, which missing library agent).
   Keep the existing per-team raw-JSON isolation so an unrelated
   old-shape team can't block this one.

2. **`team.rs` display** (`list` / `show`):
   - `RosterJson::from_roster` only buckets labels by role — switch
     it to take `&TeamRoster` and use `member.role()`. No library
     needed (the `--json` shape is unchanged).
   - `print_roster` shows `label (tool)`; pass the library so a
     `Ref`'s tool resolves for display, falling back to a clear
     marker (e.g. `?`) for a dangling ref. Display must never
     hard-fail.

3. **`team.rs::save_team`** (publish repo roster → team template):
   it already inserts each agent's role-free description into the
   library (and hard-errors on a conflicting existing library
   definition). So after that step every published agent's
   definition lives in the library under the same label — write the
   team members as `Ref { agent: None, role }` instead of full
   inline `RosterAgent`s. This realizes "prefer teams defined by
   picking agents from the roster": `team save` now produces the
   deduped ref shape automatically.

4. **`agent.rs::remove_global_agent`** (scrub a removed library agent
   from every team): remove any member that RESOLVES to the removed
   label — i.e. a `Ref` whose `agent.unwrap_or(key) == label` (the
   common case is key == label). Leave `Inline` members (they don't
   reference the library). Report which teams were touched, as today.

No other consumer touches `teams`. `agent_store` /
`resolve_registered_set` / `try_resolve_via_team_with` operate on the
repo's `Roster` and are untouched.

## Backward compatibility

Non-breaking. Existing `teams` values are full inline objects with a
`tool` field → they deserialize as `TeamMember::Inline`, so every
current `~/.clank/config.json` keeps loading unchanged. The old
`TeamComposition` shape (`{master, commit_reviewers,
gate_reviewers}`) still fails to parse as a `TeamRoster` (a string /
array value is neither variant), so `old_teams_shape_hint` and the
`load_user_team_roster` old-shape fallback keep working — verify with
the existing fail-closed tests, adjusting only types.

## Home-config cleanup (after build, before install)

Because the change is non-breaking, this is a tidy-up, not a forced
migration. Once the new binary is built, rewrite the real
`~/.clank/config.json` `teams` to the ref shape so it stops
duplicating the library:

```json
"teams": {
  "default":  { "claude": {"role":"master"}, "codex": {"role":"commit"},
                "ruthless": {"role":"gate"} },
  "fast-dev": { "claude": {"role":"master"}, "codex": {"role":"commit"} }
}
```

(`ruthless`'s `--agent ruthless-code-reviewer` launch now lives ONLY
in `agents.ruthless`, where it belongs.) Validate the rewritten file
loads with `clank team show default` / `clank team list` before
`cargo install`.

## Acceptance criteria

- `TeamMember` sum type (`Inline` | `Ref`) + `TeamRef` +
  `TeamRoster`; `UserConfigFile.teams: BTreeMap<String, TeamRoster>`.
  `Roster` / `RosterAgent` / repo config unchanged.
- Serde round-trips all three wire shapes; ref values without `tool`
  never match `Inline`; a mistyped ref errors cleanly.
- `init --team` resolves refs against the library into a concrete
  self-contained repo `Roster`; dangling ref → actionable error.
- **Repo config untouched (guard test):** a team saved as refs and
  the equivalent team saved inline produce a **byte-identical** repo
  `.clank/config.json` via `init --team`; `RepoConfigFile` / `Roster`
  / `RosterAgent` types and the repo-config readers/writers are
  unchanged.
- `team save` writes ref members (deduped against the library).
- `team list`/`show` render ref-based teams (tool resolved for
  display, dangling shown, never panics).
- `remove_global_agent` scrubs members that resolve to the removed
  agent.
- Existing inline-shape teams still load (back-compat test);
  old-`TeamComposition` shape still fails closed.
- Real `~/.clank/config.json` migrated to refs and validated;
  `cargo install --path crates/cli --force`.

## Out of scope

- Repo `.clank/config.json` schema (stays full inline — self-
  contained operating roster; nothing to reference).
- A CLI verb to add a team member as a ref (teams are minted by
  `team save` and consumed by `init --team`; no in-place team
  editing exists and none is added here).
- The `RegisteredSet` / resolution output shape (unchanged).
