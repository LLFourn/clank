//! `clank team` — manage user-scope team compositions.
//!
//! Teams live in `~/.clank/config.json#/teams` and group agents
//! from `~/.clank/config.json#/agents` into a master + two
//! reviewer tiers (commit + gate). Plan:
//! `teams-based-agent-registration`.
//!
//! All operations implicitly target user-scope — teams live
//! nowhere else. Read/write goes through the typed
//! [`crate::cli::teams_config::UserConfigFile`].

use std::path::{Path, PathBuf};

use anyhow::Context;
use clank_core::ids::AgentLabel;

use crate::cli::teams_config::{AgentDescription, ReviewKind, TeamComposition, UserConfigFile};
use crate::cli::{TeamArgs, TeamCmd, TeamListArgs, TeamShowArgs};

pub async fn run(args: TeamArgs) -> anyhow::Result<()> {
    let home = home_dir()?;
    // `run` is the thin imperative shell: resolve `$HOME`, unpack
    // clap args, call the env-free/args-free `pub` cores below.
    // Plan: dogfood-init-setup-in-tests (Phase A).
    match args.command {
        TeamCmd::List(a) => list(&home, a),
        TeamCmd::Show(a) => show(&home, a),
        TeamCmd::Create(a) => create_team(&home, &a.team),
        TeamCmd::Delete(a) => delete_team(&home, &a.team, a.force),
        TeamCmd::Add(a) => add_member(&home, &a.team, &a.agent, a.review.into()),
        TeamCmd::Remove(a) => remove_member(&home, &a.team, &a.agent),
        TeamCmd::SetMaster(a) => set_master(&home, &a.team, &a.agent),
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
        Ok(s) => serde_json::from_str(&s).with_context(|| format!("parsing {}", path.display())),
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
    use std::io::Write;
    let path = user_config_path(home);
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("no parent for `{}`", path.display()))?;
    std::fs::create_dir_all(parent)?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".clank-config-")
        .suffix(".json.tmp")
        .tempfile_in(parent)?;
    tmp.write_all(serde_json::to_string_pretty(file)?.as_bytes())?;
    tmp.write_all(b"\n")?;
    tmp.as_file_mut().sync_all()?;
    tmp.persist(&path).map_err(|e| e.error)?;
    Ok(())
}

fn parse_label(s: &str) -> anyhow::Result<AgentLabel> {
    AgentLabel::parse(s).map_err(|e| anyhow::anyhow!("invalid label `{s}`: {e}"))
}

fn fmt_agent_with_tool(label: &AgentLabel, desc: Option<&AgentDescription>) -> String {
    match desc {
        Some(d) => format!("{} ({})", label.as_str(), d.tool.as_str()),
        None => label.as_str().to_string(),
    }
}

// ── subcommands ─────────────────────────────────────────────

fn list(home: &Path, args: TeamListArgs) -> anyhow::Result<()> {
    let cfg = read_user_config(home)?;
    if args.json {
        let mut rows: Vec<serde_json::Value> = Vec::new();
        for (name, comp) in &cfg.teams {
            rows.push(serde_json::json!({
                "name": name,
                "master": comp.master.as_ref().map(|l| l.as_str()),
                "commit_reviewers": comp.commit_reviewers.iter().map(|l| l.as_str()).collect::<Vec<_>>(),
                "gate_reviewers": comp.gate_reviewers.iter().map(|l| l.as_str()).collect::<Vec<_>>(),
            }));
        }
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if cfg.teams.is_empty() {
        println!("no teams defined in {}", user_config_path(home).display());
        return Ok(());
    }
    for (name, comp) in &cfg.teams {
        let master = comp
            .master
            .as_ref()
            .map(|l| l.as_str())
            .unwrap_or("<unset>");
        let commit = comp
            .commit_reviewers
            .iter()
            .map(|l| l.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let gate = comp
            .gate_reviewers
            .iter()
            .map(|l| l.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        println!(
            "{name}\n  master:           {master}\n  commit reviewers: {commit}\n  gate reviewers:   {gate}"
        );
    }
    Ok(())
}

fn show(home: &Path, args: TeamShowArgs) -> anyhow::Result<()> {
    let cfg = read_user_config(home)?;
    let comp = cfg
        .teams
        .get(&args.team)
        .ok_or_else(|| anyhow::anyhow!("team `{}` not in user-scope `teams`", args.team))?;
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "name": args.team,
                "master": comp.master.as_ref().map(|l| l.as_str()),
                "commit_reviewers": comp.commit_reviewers.iter().map(|l| l.as_str()).collect::<Vec<_>>(),
                "gate_reviewers": comp.gate_reviewers.iter().map(|l| l.as_str()).collect::<Vec<_>>(),
            }))?
        );
        return Ok(());
    }
    println!("team `{}`:", args.team);
    println!(
        "  master:           {}",
        comp.master
            .as_ref()
            .map(|l| fmt_agent_with_tool(l, cfg.agents.get(l)))
            .unwrap_or_else(|| "<unset>".to_string())
    );
    let fmt_list = |labels: &[AgentLabel]| -> String {
        if labels.is_empty() {
            "—".to_string()
        } else {
            labels
                .iter()
                .map(|l| fmt_agent_with_tool(l, cfg.agents.get(l)))
                .collect::<Vec<_>>()
                .join(", ")
        }
    };
    println!("  commit reviewers: {}", fmt_list(&comp.commit_reviewers));
    println!("  gate reviewers:   {}", fmt_list(&comp.gate_reviewers));
    Ok(())
}

// ── mutation cores ───────────────────────────────────────────
//
// `pub`, env-free (take an explicit `home: &Path`), args-free
// (take primitives, not clap `Args`). Both `run()` above and the
// integration-test setup call these, so the test model and the
// production model are THE SAME CODE. Plan:
// dogfood-init-setup-in-tests (Phase A). These still do file IO
// (read/write `~/.clank/config.json`) — "core" means no env/clap,
// not pure.

/// Create an empty user-scope team. Errors if it already exists.
pub fn create_team(home: &Path, team: &str) -> anyhow::Result<()> {
    let mut cfg = read_user_config(home)?;
    if cfg.teams.contains_key(team) {
        anyhow::bail!("team `{team}` already exists in user-scope `teams`");
    }
    cfg.teams
        .insert(team.to_string(), TeamComposition::default());
    write_user_config(home, &cfg)?;
    eprintln!(
        "created empty team `{team}` in user-scope (use `clank team set-master {team} <agent>` to designate the master)"
    );
    Ok(())
}

/// Delete a user-scope team. Requires `force` (clank can't see
/// which repos reference it).
pub fn delete_team(home: &Path, team: &str, force: bool) -> anyhow::Result<()> {
    let mut cfg = read_user_config(home)?;
    if !cfg.teams.contains_key(team) {
        anyhow::bail!("team `{team}` not in user-scope `teams`");
    }
    if !force {
        anyhow::bail!(
            "team `{team}` may be referenced by a repo's `.clank/config.json#/team` field. \
             Pass `--force` to delete anyway. Repos still pointing at the deleted team will \
             fail with `UnknownTeam` at registration time."
        );
    }
    cfg.teams.remove(team);
    write_user_config(home, &cfg)?;
    eprintln!("deleted team `{team}` from user-scope");
    Ok(())
}

/// Add an agent to a team's reviewer tier. The agent must be
/// declared in user-scope `agents`; one tier per team.
pub fn add_member(home: &Path, team: &str, agent: &str, review: ReviewKind) -> anyhow::Result<()> {
    let mut cfg = read_user_config(home)?;
    let label = parse_label(agent)?;

    if !cfg.agents.contains_key(&label) {
        anyhow::bail!(
            "agent `{agent}` is not declared in user-scope `agents`. Add it via `clank agent add --global {agent} --tool <claude|codex>` first."
        );
    }

    let team_comp = cfg
        .teams
        .get_mut(team)
        .ok_or_else(|| anyhow::anyhow!("team `{team}` not in user-scope `teams`"))?;

    if team_comp.master.as_ref() == Some(&label) {
        anyhow::bail!(
            "agent `{agent}` is already the master of team `{team}`. Use `clank team set-master` to change the master, or remove this agent from the master slot first."
        );
    }
    if team_comp.commit_reviewers.contains(&label) || team_comp.gate_reviewers.contains(&label) {
        anyhow::bail!(
            "agent `{agent}` is already in team `{team}` (one tier per team). Use `clank team remove` then `clank team add` to move them between tiers."
        );
    }

    match review {
        ReviewKind::Commit => team_comp.commit_reviewers.push(label),
        ReviewKind::Gate => team_comp.gate_reviewers.push(label),
    }
    write_user_config(home, &cfg)?;
    eprintln!(
        "added `{agent}` to team `{team}` as `{}` reviewer",
        match review {
            ReviewKind::Commit => "commit",
            ReviewKind::Gate => "gate",
        }
    );
    Ok(())
}

/// Remove an agent from a team's reviewer tiers. Refuses to
/// remove the team's master.
pub fn remove_member(home: &Path, team: &str, agent: &str) -> anyhow::Result<()> {
    let mut cfg = read_user_config(home)?;
    let label = parse_label(agent)?;

    let team_comp = cfg
        .teams
        .get_mut(team)
        .ok_or_else(|| anyhow::anyhow!("team `{team}` not in user-scope `teams`"))?;

    if team_comp.master.as_ref() == Some(&label) {
        anyhow::bail!(
            "agent `{agent}` is the master of team `{team}`. Use `clank team set-master` to designate a different master first, or `clank team delete` the team."
        );
    }
    let initial_commit = team_comp.commit_reviewers.len();
    team_comp.commit_reviewers.retain(|l| l != &label);
    let initial_gate = team_comp.gate_reviewers.len();
    team_comp.gate_reviewers.retain(|l| l != &label);

    if team_comp.commit_reviewers.len() == initial_commit
        && team_comp.gate_reviewers.len() == initial_gate
    {
        anyhow::bail!("agent `{agent}` is not in team `{team}`");
    }

    write_user_config(home, &cfg)?;
    eprintln!("removed `{agent}` from team `{team}`");
    Ok(())
}

/// Designate a team's master. Moves the agent out of any reviewer
/// tier; demotes the previous master to commit_reviewers.
pub fn set_master(home: &Path, team: &str, agent: &str) -> anyhow::Result<()> {
    let mut cfg = read_user_config(home)?;
    let label = parse_label(agent)?;

    if !cfg.agents.contains_key(&label) {
        anyhow::bail!(
            "agent `{agent}` is not declared in user-scope `agents`. Add it via `clank agent add --global {agent} --tool <claude|codex>` first."
        );
    }

    let team_comp = cfg
        .teams
        .get_mut(team)
        .ok_or_else(|| anyhow::anyhow!("team `{team}` not in user-scope `teams`"))?;

    // If the agent was previously in a reviewer tier, move them
    // out (one role per team).
    let was_commit = team_comp.commit_reviewers.iter().any(|l| l == &label);
    let was_gate = team_comp.gate_reviewers.iter().any(|l| l == &label);
    team_comp.commit_reviewers.retain(|l| l != &label);
    team_comp.gate_reviewers.retain(|l| l != &label);

    let prev_master = team_comp.master.replace(label);

    // Demote previous master to commit_reviewers (the more
    // engaged tier — they were master; the team probably wants
    // them tracking every commit, not just gates). Skip if it's
    // the same label (no-op promotion).
    let mut demoted: Option<AgentLabel> = None;
    if let Some(prev) = prev_master {
        if prev != *team_comp.master.as_ref().unwrap() {
            team_comp.commit_reviewers.push(prev.clone());
            demoted = Some(prev);
        }
    }
    write_user_config(home, &cfg)?;
    let mut msg = format!("set `{agent}` as master of team `{team}`");
    if let Some(d) = demoted {
        msg.push_str(&format!(" (demoted `{}` to commit reviewer)", d.as_str()));
    }
    if was_commit {
        msg.push_str(" (was commit reviewer)");
    }
    if was_gate {
        msg.push_str(" (was gate reviewer)");
    }
    eprintln!("{msg}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(unused_imports)]
    use crate::cli::teams_config::AgentDescription;
    use clank_core::vocab::Tool;
    use tempfile::TempDir;

    fn setup_home() -> TempDir {
        let dir = tempfile::tempdir().unwrap();
        dir
    }

    fn seed_user_config(home: &Path, agents: &[(&str, Tool)], teams: &[(&str, &TeamComposition)]) {
        let mut cfg = UserConfigFile::default();
        for (label, tool) in agents {
            cfg.agents.insert(
                AgentLabel::parse(label).unwrap(),
                AgentDescription {
                    tool: *tool,
                    launch: None,
                    initial_prompt: None,
                },
            );
        }
        for (name, comp) in teams {
            cfg.teams.insert(name.to_string(), (*comp).clone());
        }
        write_user_config(home, &cfg).unwrap();
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

        // THE codex regression: a MISSING per-agent config under a
        // global default-on resolves On — so the stop hook no longer
        // exits Silent for a fresh/unbound session.
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

    #[test]
    fn create_then_set_master_then_add_round_trip() {
        let home_dir = setup_home();
        let home = home_dir.path();
        seed_user_config(
            home,
            &[("claude", Tool::Claude), ("codex", Tool::Codex)],
            &[],
        );

        create_team(home, "dev").unwrap();
        set_master(home, "dev", "claude").unwrap();
        add_member(home, "dev", "codex", ReviewKind::Commit).unwrap();

        let cfg = read_user_config(home).unwrap();
        let dev = cfg.teams.get("dev").unwrap();
        assert_eq!(dev.master.as_ref().unwrap().as_str(), "claude");
        assert_eq!(dev.commit_reviewers.len(), 1);
        assert_eq!(dev.commit_reviewers[0].as_str(), "codex");
        assert!(dev.gate_reviewers.is_empty());
    }

    #[test]
    fn set_master_demotes_previous_master_to_commit_reviewer() {
        let home_dir = setup_home();
        let home = home_dir.path();
        let mut dev = TeamComposition::default();
        dev.master = Some(AgentLabel::parse("claude").unwrap());
        seed_user_config(
            home,
            &[("claude", Tool::Claude), ("codex", Tool::Codex)],
            &[("dev", &dev)],
        );

        set_master(home, "dev", "codex").unwrap();

        let cfg = read_user_config(home).unwrap();
        let dev = cfg.teams.get("dev").unwrap();
        assert_eq!(dev.master.as_ref().unwrap().as_str(), "codex");
        // Previous master (claude) demoted to commit reviewer.
        let cr_labels: Vec<_> = dev.commit_reviewers.iter().map(|l| l.as_str()).collect();
        assert!(cr_labels.contains(&"claude"));
    }

    #[test]
    fn add_with_gate_tier_lands_in_gate_reviewers() {
        let home_dir = setup_home();
        let home = home_dir.path();
        let mut dev = TeamComposition::default();
        dev.master = Some(AgentLabel::parse("claude").unwrap());
        seed_user_config(
            home,
            &[("claude", Tool::Claude), ("ruthless", Tool::Claude)],
            &[("dev", &dev)],
        );

        add_member(home, "dev", "ruthless", ReviewKind::Gate).unwrap();

        let cfg = read_user_config(home).unwrap();
        let dev = cfg.teams.get("dev").unwrap();
        assert_eq!(dev.gate_reviewers.len(), 1);
        assert_eq!(dev.gate_reviewers[0].as_str(), "ruthless");
        assert!(dev.commit_reviewers.is_empty());
    }

    #[test]
    fn add_rejects_agent_already_in_team() {
        let home_dir = setup_home();
        let home = home_dir.path();
        let mut dev = TeamComposition::default();
        dev.master = Some(AgentLabel::parse("claude").unwrap());
        dev.commit_reviewers
            .push(AgentLabel::parse("codex").unwrap());
        seed_user_config(
            home,
            &[("claude", Tool::Claude), ("codex", Tool::Codex)],
            &[("dev", &dev)],
        );

        let err = add_member(home, "dev", "codex", ReviewKind::Gate).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("already in team"));
    }

    #[test]
    fn delete_without_force_refuses() {
        let home_dir = setup_home();
        let home = home_dir.path();
        let mut dev = TeamComposition::default();
        dev.master = Some(AgentLabel::parse("claude").unwrap());
        seed_user_config(home, &[("claude", Tool::Claude)], &[("dev", &dev)]);

        let err = delete_team(home, "dev", false).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("--force"));

        // Confirm still present.
        let cfg = read_user_config(home).unwrap();
        assert!(cfg.teams.contains_key("dev"));
    }

    #[test]
    fn remove_master_is_rejected() {
        let home_dir = setup_home();
        let home = home_dir.path();
        let mut dev = TeamComposition::default();
        dev.master = Some(AgentLabel::parse("claude").unwrap());
        seed_user_config(home, &[("claude", Tool::Claude)], &[("dev", &dev)]);

        let err = remove_member(home, "dev", "claude").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("master"));
    }
}
