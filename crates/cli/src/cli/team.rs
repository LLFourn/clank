//! `clank team` — the GLOBAL team-template library only
//! (`~/.clank/config.json#/teams`). A "team" is no longer a repo
//! concept: the repo's roster (`clank agent …`) is the operating
//! team. This module:
//!
//! - `save <name>` — capture THIS repo's roster as a reusable
//!   global template (same-shape [`Roster`] copy up).
//! - `list` / `show <name>` — inspect the template library.
//! - `delete <name>` — drop a template.
//!
//! Templates are minted (by `save`) and consumed by
//! `clank init --team`; there is no in-place template editing.
//!
//! Plan: `repo-agents-no-team`.

use std::path::{Path, PathBuf};

use clank_core::ids::AgentLabel;

use std::collections::BTreeMap;

use crate::cli::teams_config::{
    AgentDescription, RosterRole, TeamMember, TeamRef, TeamRoster, UserConfigFile,
};
use crate::cli::{
    TeamArgs, TeamCmd, TeamDeleteArgs, TeamListArgs, TeamSaveArgs, TeamShowArgs, resolve_repo,
};

pub async fn run(args: TeamArgs) -> anyhow::Result<()> {
    // `run` is the thin imperative shell: resolve `$HOME` / repo,
    // unpack clap args, call the env-free/args-free `pub` cores
    // below.
    match args.command {
        TeamCmd::Save(a) => team_save(a),
        TeamCmd::List(a) => list(&home_dir()?, a),
        TeamCmd::Show(a) => show_team(&home_dir()?, a),
        TeamCmd::Delete(a) => delete(&home_dir()?, a),
    }
}

fn home_dir() -> anyhow::Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("$HOME not set; `clank team` needs a user-scope config"))
}

fn user_config_path(home: &Path) -> PathBuf {
    home.join(".clank/config.json")
}

/// Read user-scope `~/.clank/config.json` as the typed
/// [`UserConfigFile`]. `pub` so integration tests can assert
/// team state after calling the cores.
pub fn read_user_config(home: &Path) -> anyhow::Result<UserConfigFile> {
    let path = user_config_path(home);
    match std::fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s).map_err(|e| {
            // An OLD-shape `teams` value (a TeamComposition, not a
            // roster) fails to parse here — fail closed with the
            // re-save hint instead of a cryptic serde message.
            match crate::cli::teams_config::old_teams_shape_hint(&s) {
                Some(hint) => anyhow::anyhow!("{hint}"),
                None => anyhow::Error::from(e).context(format!("parsing {}", path.display())),
            }
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(UserConfigFile::default()),
        Err(e) => Err(anyhow::Error::from(e).context(format!("reading {}", path.display()))),
    }
}

/// Resolve a label's EFFECTIVE auto-mode (`auto-mode-default-on`):
/// the explicit per-agent setting if any, else the `~/.clank`
/// user-global default, else Off. THE single resolution entry
/// point — the stop hook, agent start, and `clank auto status` all
/// call this, so both the user-default read and the precedence live
/// in exactly one place (never re-implemented per consumer). `home`
/// is explicit (dogfood) so it's unit-testable; `per_agent` is the
/// caller's already-loaded config (avoids a re-load).
pub(crate) fn resolve_effective_auto_mode(
    per_agent: Option<&clank_core::agent_config::AgentConfig>,
    home: Option<&Path>,
) -> clank_core::vocab::AutoMode {
    let user_default = home
        .and_then(|h| read_user_config(h).ok())
        .and_then(|c| c.auto);
    clank_core::agent_config::effective_auto_mode(per_agent.and_then(|c| c.auto_mode), user_default)
}

fn write_user_config(home: &Path, file: &UserConfigFile) -> anyhow::Result<()> {
    write_typed_config(&user_config_path(home), file)
}

/// Atomic write of a typed config via tempfile + rename.
fn write_typed_config<T: serde::Serialize>(path: &Path, value: &T) -> anyhow::Result<()> {
    use std::io::Write;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("no parent for `{}`", path.display()))?;
    std::fs::create_dir_all(parent)?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".clank-config-")
        .suffix(".json.tmp")
        .tempfile_in(parent)?;
    tmp.write_all(serde_json::to_string_pretty(value)?.as_bytes())?;
    tmp.write_all(b"\n")?;
    tmp.as_file_mut().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

/// A team's master + reviewer tiers as wire strings. The shared
/// `--json` shape for `clank team show` and (flattened under each
/// entry's `name`) `clank team list`. Derived from a [`TeamRoster`]
/// by bucketing on each member's role — the `--json` shape lists
/// labels only, so it needs no library lookup.
#[derive(serde::Serialize)]
struct RosterJson<'a> {
    master: Option<&'a str>,
    commit_reviewers: Vec<&'a str>,
    gate_reviewers: Vec<&'a str>,
}

impl<'a> RosterJson<'a> {
    fn from_team(team: &'a TeamRoster) -> Self {
        let mut master = None;
        let mut commit_reviewers = Vec::new();
        let mut gate_reviewers = Vec::new();
        for (label, member) in team {
            match member.role() {
                RosterRole::Master => master = Some(label.as_str()),
                RosterRole::Commit => commit_reviewers.push(label.as_str()),
                RosterRole::Gate => gate_reviewers.push(label.as_str()),
            }
        }
        Self {
            master,
            commit_reviewers,
            gate_reviewers,
        }
    }
}

/// `clank team list --json` row: the team `name` plus its roster
/// flattened to the top level.
#[derive(serde::Serialize)]
struct TeamListJson<'a> {
    name: &'a str,
    #[serde(flatten)]
    team: RosterJson<'a>,
}

/// Format `label (tool)` for the human listing. A [`TeamMember::Ref`]
/// resolves its tool from the `library`; an aliased ref shows its
/// target, and a dangling ref shows `→target MISSING` rather than
/// hard-failing (display must never error).
fn fmt_member(
    label: &AgentLabel,
    member: &TeamMember,
    library: &BTreeMap<AgentLabel, AgentDescription>,
) -> String {
    match member {
        TeamMember::Inline(agent) => format!("{} ({})", label.as_str(), agent.tool.as_str()),
        TeamMember::Ref(r) => {
            let target = r.agent.as_ref().unwrap_or(label);
            match library.get(target) {
                Some(desc) if r.agent.is_some() => format!(
                    "{} (→{}, {})",
                    label.as_str(),
                    target.as_str(),
                    desc.tool.as_str()
                ),
                Some(desc) => format!("{} ({})", label.as_str(), desc.tool.as_str()),
                None => format!("{} (→{} MISSING)", label.as_str(), target.as_str()),
            }
        }
    }
}

fn print_roster(team: &TeamRoster, library: &BTreeMap<AgentLabel, AgentDescription>) {
    let by_role = |want: RosterRole| -> String {
        let entries: Vec<String> = team
            .iter()
            .filter(|(_, m)| m.role() == want)
            .map(|(l, m)| fmt_member(l, m, library))
            .collect();
        if entries.is_empty() {
            "—".to_string()
        } else {
            entries.join(", ")
        }
    };
    let master = team
        .iter()
        .find(|(_, m)| m.role() == RosterRole::Master)
        .map(|(l, m)| fmt_member(l, m, library))
        .unwrap_or_else(|| "<unset>".to_string());
    println!("  master:           {master}");
    println!("  commit reviewers: {}", by_role(RosterRole::Commit));
    println!("  gate reviewers:   {}", by_role(RosterRole::Gate));
}

// ── subcommands ─────────────────────────────────────────────

fn list(home: &Path, args: TeamListArgs) -> anyhow::Result<()> {
    let cfg = read_user_config(home)?;
    if args.json {
        let rows: Vec<TeamListJson> = cfg
            .teams
            .iter()
            .map(|(name, team)| TeamListJson {
                name,
                team: RosterJson::from_team(team),
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if cfg.teams.is_empty() {
        println!("no teams defined in {}", user_config_path(home).display());
        return Ok(());
    }
    for (name, team) in &cfg.teams {
        println!("{name}");
        print_roster(team, &cfg.agents);
    }
    Ok(())
}

/// `clank team show <name>` — print a single global template's
/// roster.
fn show_team(home: &Path, args: TeamShowArgs) -> anyhow::Result<()> {
    let cfg = read_user_config(home)?;
    let team = cfg.teams.get(&args.team).ok_or_else(|| {
        anyhow::anyhow!(
            "team `{}` not in user-scope `teams` ({})",
            args.team,
            user_config_path(home).display()
        )
    })?;
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&RosterJson::from_team(team))?
        );
        return Ok(());
    }
    println!("team `{}`:", args.team);
    print_roster(team, &cfg.agents);
    Ok(())
}

/// `clank team save <name> [--force]` — thin shell. Publishes THIS
/// repo's roster as a reusable global template.
fn team_save(args: TeamSaveArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    save_team(&home_dir()?, &repo, &args.name, args.force)
}

/// `clank team delete <name> [--force]` — thin shell.
fn delete(home: &Path, args: TeamDeleteArgs) -> anyhow::Result<()> {
    delete_team(home, &args.team, args.force)
}

// ── global team-template library cores ───────────────────────
//
// `pub`, env-free (take an explicit `home: &Path`), args-free.

/// Publish THIS repo's roster as a reusable GLOBAL template named
/// `name`. `pub`, env-free (explicit `home` + `repo`), args-free.
///
/// The published team is the repo's roster, copied UP same-shape.
/// Each roster agent's role-free DESCRIPTION is also copied into
/// the user-scope `agents` library (the by-name pool).
///
/// Fail-closed, collision check BEFORE any write — no partial
/// writes:
/// - The repo config is read through the same fail-closed loader
///   the resolver uses ([`crate::agent_store::load_repo_config_required`]),
///   so an old-shape repo config yields the re-init hint.
/// - For each roster agent, its repo DESCRIPTION is compared to
///   any identically-named user-scope library agent: absent →
///   will insert; equal → no-op; DIFFERENT → HARD ERROR (not
///   `--force`-able).
/// - An existing user-scope team of the same `name` requires
///   `--force`.
/// - A roster with NO master is allowed (a faithful partial
///   snapshot).
pub fn save_team(home: &Path, repo: &Path, name: &str, force: bool) -> anyhow::Result<()> {
    let repo_cfg = crate::agent_store::load_repo_config_required(repo)?;
    let mut user_cfg = read_user_config(home)?;

    // Collision check FIRST, before any write (fail-closed).
    if user_cfg.teams.contains_key(name) && !force {
        anyhow::bail!("team `{name}` already exists in user-scope; pass --force to overwrite");
    }

    let mut to_insert: Vec<(AgentLabel, AgentDescription)> = Vec::new();
    let mut already_present: Vec<AgentLabel> = Vec::new();
    for (label, agent) in &repo_cfg.agents {
        let repo_desc = agent.to_description();
        match user_cfg.agents.get(label) {
            None => to_insert.push((label.clone(), repo_desc)),
            Some(global_desc) if *global_desc == repo_desc => already_present.push(label.clone()),
            Some(_) => anyhow::bail!(
                "agent `{}` already exists in user-scope `agents` with a different definition; \
                 rename it or reconcile before saving",
                label.as_str()
            ),
        }
    }

    // All checks passed — apply (single write). Every roster agent's
    // description now lives in the library (just inserted, or already
    // present and identical), so the team is published as REFERENCES
    // — one source of truth, no duplicated definitions.
    for (label, desc) in &to_insert {
        user_cfg.agents.insert(label.clone(), desc.clone());
    }
    let team: TeamRoster = repo_cfg
        .agents
        .iter()
        .map(|(label, agent)| {
            (
                label.clone(),
                TeamMember::Ref(TeamRef {
                    agent: None,
                    role: agent.role,
                }),
            )
        })
        .collect();
    user_cfg.teams.insert(name.to_string(), team);
    write_user_config(home, &user_cfg)?;

    let added: Vec<&str> = to_insert.iter().map(|(l, _)| l.as_str()).collect();
    let present: Vec<&str> = already_present.iter().map(|l| l.as_str()).collect();
    eprintln!("published team `{name}` to user-scope `teams`");
    eprintln!(
        "  agents added:           {}",
        if added.is_empty() {
            "—".to_string()
        } else {
            added.join(", ")
        }
    );
    eprintln!(
        "  agents already present: {}",
        if present.is_empty() {
            "—".to_string()
        } else {
            present.join(", ")
        }
    );
    Ok(())
}

/// Delete a user-scope team. Requires `force` (clank can't see
/// which repos are seeded from it).
pub fn delete_team(home: &Path, team: &str, force: bool) -> anyhow::Result<()> {
    let mut cfg = read_user_config(home)?;
    if !cfg.teams.contains_key(team) {
        anyhow::bail!("team `{team}` not in user-scope `teams`");
    }
    if !force {
        anyhow::bail!(
            "team `{team}` may have been used to seed a repo (`clank init --team {team}`). \
             Pass `--force` to delete anyway."
        );
    }
    cfg.teams.remove(team);
    write_user_config(home, &cfg)?;
    eprintln!("deleted team `{team}` from user-scope");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clank_core::vocab::Tool;
    use tempfile::TempDir;

    fn setup_home() -> TempDir {
        tempfile::tempdir().unwrap()
    }

    fn lbl(s: &str) -> AgentLabel {
        AgentLabel::parse(s).unwrap()
    }

    fn team_ref(role: RosterRole) -> TeamMember {
        TeamMember::Ref(TeamRef { agent: None, role })
    }

    fn seed_user_config(home: &Path, agents: &[(&str, Tool)], teams: &[(&str, TeamRoster)]) {
        let mut cfg = UserConfigFile::default();
        for (label, tool) in agents {
            cfg.agents.insert(
                lbl(label),
                AgentDescription {
                    tool: *tool,
                    launch: None,
                    initial_prompt: None,
                },
            );
        }
        for (name, team) in teams {
            cfg.teams.insert(name.to_string(), team.clone());
        }
        write_user_config(home, &cfg).unwrap();
    }

    fn seed_repo_config(repo: &Path, body: &str) {
        std::fs::create_dir_all(repo.join(".clank")).unwrap();
        std::fs::write(repo.join(".clank/config.json"), body).unwrap();
    }

    // ── team --json wire shapes ─────────────────────────────

    #[test]
    fn team_json_shapes_bucket_by_role() {
        let mut team: TeamRoster = std::collections::BTreeMap::new();
        team.insert(lbl("claude"), team_ref(RosterRole::Master));
        team.insert(lbl("codex"), team_ref(RosterRole::Commit));
        team.insert(lbl("ruthless"), team_ref(RosterRole::Gate));

        assert_eq!(
            serde_json::to_value(RosterJson::from_team(&team)).unwrap(),
            serde_json::json!({
                "master": "claude",
                "commit_reviewers": ["codex"],
                "gate_reviewers": ["ruthless"],
            })
        );

        let row = TeamListJson {
            name: "dev",
            team: RosterJson::from_team(&team),
        };
        assert_eq!(
            serde_json::to_value(row).unwrap(),
            serde_json::json!({
                "name": "dev",
                "master": "claude",
                "commit_reviewers": ["codex"],
                "gate_reviewers": ["ruthless"],
            })
        );

        // master unset → null.
        let empty: TeamRoster = std::collections::BTreeMap::new();
        assert_eq!(
            serde_json::to_value(RosterJson::from_team(&empty)).unwrap(),
            serde_json::json!({
                "master": null,
                "commit_reviewers": [],
                "gate_reviewers": [],
            })
        );
    }

    #[test]
    fn resolve_effective_auto_mode_layers_global_default() {
        use clank_core::agent_config::AgentConfig;
        use clank_core::vocab::AutoMode;
        let home_dir = setup_home();
        let home = home_dir.path();

        // No user-global default, no per-agent config → Off.
        assert_eq!(resolve_effective_auto_mode(None, Some(home)), AutoMode::Off);

        // Seed a user-global default-on.
        let cfg = UserConfigFile {
            auto: Some(AutoMode::On),
            ..Default::default()
        };
        write_user_config(home, &cfg).unwrap();

        // A MISSING per-agent config under a global default-on
        // resolves On.
        assert_eq!(resolve_effective_auto_mode(None, Some(home)), AutoMode::On);

        // An unset per-agent config also inherits the global default.
        let unset = AgentConfig::default();
        assert_eq!(
            resolve_effective_auto_mode(Some(&unset), Some(home)),
            AutoMode::On
        );

        // An explicit per-agent OFF wins (sticky off under default-on).
        let off = AgentConfig {
            auto_mode: Some(AutoMode::Off),
            ..Default::default()
        };
        assert_eq!(
            resolve_effective_auto_mode(Some(&off), Some(home)),
            AutoMode::Off
        );
    }

    // ── delete ───────────────────────────────────────────────

    #[test]
    fn delete_without_force_refuses() {
        let home_dir = setup_home();
        let home = home_dir.path();
        let mut dev: TeamRoster = std::collections::BTreeMap::new();
        dev.insert(lbl("claude"), team_ref(RosterRole::Master));
        seed_user_config(home, &[("claude", Tool::Claude)], &[("dev", dev)]);

        let err = delete_team(home, "dev", false).unwrap_err();
        assert!(format!("{err:#}").contains("--force"));

        // Confirm still present.
        let cfg = read_user_config(home).unwrap();
        assert!(cfg.teams.contains_key("dev"));
    }

    #[test]
    fn delete_with_force_removes() {
        let home_dir = setup_home();
        let home = home_dir.path();
        let mut dev: TeamRoster = std::collections::BTreeMap::new();
        dev.insert(lbl("claude"), team_ref(RosterRole::Master));
        seed_user_config(home, &[("claude", Tool::Claude)], &[("dev", dev)]);
        delete_team(home, "dev", true).unwrap();
        assert!(!read_user_config(home).unwrap().teams.contains_key("dev"));
    }

    // ── team save ─────────────────────────────────────────────

    #[test]
    fn save_team_publishes_roster_and_copies_descriptions() {
        let home_dir = setup_home();
        let home = home_dir.path();
        seed_user_config(home, &[], &[]);
        let repo = tempfile::tempdir().unwrap();
        seed_repo_config(
            repo.path(),
            r#"{ "agents": {
                "claude": { "tool": "claude", "role": "master" },
                "codex": { "tool": "codex", "role": "commit" },
                "ruthless": { "tool": "claude", "role": "gate" }
            } }"#,
        );

        save_team(home, repo.path(), "dev", false).unwrap();

        let cfg = read_user_config(home).unwrap();
        let dev = cfg.teams.get("dev").unwrap();
        // Published as REFERENCES (roles preserved, no inline tool).
        assert_eq!(dev.get(&lbl("claude")).unwrap().role(), RosterRole::Master);
        assert_eq!(dev.get(&lbl("codex")).unwrap().role(), RosterRole::Commit);
        assert_eq!(dev.get(&lbl("ruthless")).unwrap().role(), RosterRole::Gate);
        assert!(
            matches!(dev.get(&lbl("claude")).unwrap(), TeamMember::Ref(_)),
            "save must publish references, not inline definitions"
        );
        // Role-free descriptions copied into the library.
        assert!(cfg.agents.contains_key(&lbl("claude")));
        assert!(cfg.agents.contains_key(&lbl("codex")));
        assert!(cfg.agents.contains_key(&lbl("ruthless")));
    }

    #[test]
    fn save_team_identical_agent_resave_is_noop_not_error() {
        let home_dir = setup_home();
        let home = home_dir.path();
        seed_user_config(home, &[("claude", Tool::Claude)], &[]);
        let repo = tempfile::tempdir().unwrap();
        seed_repo_config(
            repo.path(),
            r#"{ "agents": { "claude": { "tool": "claude", "role": "master" } } }"#,
        );

        save_team(home, repo.path(), "dev", false).unwrap();
        let cfg = read_user_config(home).unwrap();
        assert!(cfg.teams.contains_key("dev"));
        assert_eq!(cfg.agents.len(), 1);
    }

    #[test]
    fn save_team_different_agent_body_is_hard_error_even_with_force() {
        let home_dir = setup_home();
        let home = home_dir.path();
        // Global `claude` is a codex (DIFFERENT body from the repo's).
        seed_user_config(home, &[("claude", Tool::Codex)], &[]);
        let repo = tempfile::tempdir().unwrap();
        seed_repo_config(
            repo.path(),
            r#"{ "agents": { "claude": { "tool": "claude", "role": "master" } } }"#,
        );

        for force in [false, true] {
            let err = save_team(home, repo.path(), "dev", force).unwrap_err();
            let msg = format!("{err:#}");
            assert!(msg.contains("different definition"), "force={force}: {msg}");
        }
        // No partial write.
        let cfg = read_user_config(home).unwrap();
        assert!(!cfg.teams.contains_key("dev"));
        assert_eq!(cfg.agents[&lbl("claude")].tool, Tool::Codex);
    }

    #[test]
    fn save_team_existing_name_requires_force() {
        let home_dir = setup_home();
        let home = home_dir.path();
        let existing: TeamRoster = std::collections::BTreeMap::new();
        seed_user_config(home, &[], &[("dev", existing)]);
        let repo = tempfile::tempdir().unwrap();
        seed_repo_config(
            repo.path(),
            r#"{ "agents": { "claude": { "tool": "claude", "role": "master" } } }"#,
        );

        let err = save_team(home, repo.path(), "dev", false).unwrap_err();
        assert!(format!("{err:#}").contains("--force"));
        // The existing (empty) team is untouched.
        let cfg = read_user_config(home).unwrap();
        assert!(cfg.teams.get("dev").unwrap().is_empty());

        save_team(home, repo.path(), "dev", true).unwrap();
        let cfg = read_user_config(home).unwrap();
        assert_eq!(
            cfg.teams
                .get("dev")
                .unwrap()
                .get(&lbl("claude"))
                .unwrap()
                .role(),
            RosterRole::Master
        );
    }

    #[test]
    fn save_team_without_master_is_allowed() {
        let home_dir = setup_home();
        let home = home_dir.path();
        seed_user_config(home, &[], &[]);
        let repo = tempfile::tempdir().unwrap();
        seed_repo_config(
            repo.path(),
            r#"{ "agents": { "codex": { "tool": "codex", "role": "commit" } } }"#,
        );

        save_team(home, repo.path(), "partial", false).unwrap();
        let cfg = read_user_config(home).unwrap();
        let team = cfg.teams.get("partial").unwrap();
        assert!(!team.values().any(|m| m.role() == RosterRole::Master));
        assert_eq!(team.get(&lbl("codex")).unwrap().role(), RosterRole::Commit);
        assert!(cfg.agents.contains_key(&lbl("codex")));
    }

    #[test]
    fn save_team_fails_closed_on_old_shape_repo_config() {
        let home_dir = setup_home();
        let home = home_dir.path();
        seed_user_config(home, &[], &[]);
        let repo = tempfile::tempdir().unwrap();
        seed_repo_config(repo.path(), r#"{"team": "dev", "promoted": "codex"}"#);

        let err = save_team(home, repo.path(), "dev", false).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("old team schema") && msg.contains("clank init"),
            "expected re-init hint; got: {msg}"
        );
    }

    // ── show ─────────────────────────────────────────────────

    #[test]
    fn show_team_errors_on_unknown_template() {
        let home_dir = setup_home();
        let home = home_dir.path();
        seed_user_config(home, &[], &[]);
        let err = show_team(
            home,
            TeamShowArgs {
                team: "nope".into(),
                json: false,
            },
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("not in user-scope `teams`"));
    }

    #[test]
    fn read_user_config_fails_closed_on_old_teams_shape() {
        // The real ~/.clank/config.json shape today: `teams` values
        // are old `TeamComposition` objects, not rosters. Reading
        // must fail closed with the re-save hint, not a cryptic
        // serde error. `list`/`show` go through this loader.
        let home_dir = setup_home();
        let home = home_dir.path();
        let path = home.join(".clank/config.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{"teams":{"default":{"master":"claude","commit_reviewers":["codex"],"gate_reviewers":["ruthless"]}}}"#,
        )
        .unwrap();
        let err = read_user_config(home).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("old team schema") && msg.contains("clank team save"),
            "expected re-save hint; got: {msg}"
        );
    }

    #[test]
    fn show_team_ok_for_existing_template() {
        let home_dir = setup_home();
        let home = home_dir.path();
        let mut dev: TeamRoster = std::collections::BTreeMap::new();
        dev.insert(lbl("claude"), team_ref(RosterRole::Master));
        seed_user_config(home, &[("claude", Tool::Claude)], &[("dev", dev)]);
        show_team(
            home,
            TeamShowArgs {
                team: "dev".into(),
                json: true,
            },
        )
        .unwrap();
    }
}
