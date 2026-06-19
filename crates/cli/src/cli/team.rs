//! `clank team` — compose THIS repo's operating team and inspect
//! the global team-template library.
//!
//! - REPO TEAM (`show` / `add` / `remove` / `set-master`):
//!   operate on `<repo>/.clank/config.json#/team`, the repo's
//!   single operating roster (by-name refs into the repo's own
//!   `agents`). No team-name argument.
//! - GLOBAL LIBRARY (`list` / `delete`): named team templates in
//!   `~/.clank/config.json#/teams`. Templates are no longer edited
//!   in place — they are minted (M3 `team save`) and consumed by
//!   `clank init --team`.
//!
//! Plan: `teams-based-agent-registration`.

use std::path::{Path, PathBuf};

use anyhow::Context;
use clank_core::ids::AgentLabel;

use crate::cli::teams_config::{AgentDescription, ReviewKind, TeamComposition, UserConfigFile};
use crate::cli::{
    TeamAddArgs, TeamArgs, TeamCmd, TeamListArgs, TeamRemoveArgs, TeamSaveArgs, TeamSetMasterArgs,
    TeamShowArgs, resolve_repo,
};

pub async fn run(args: TeamArgs) -> anyhow::Result<()> {
    // `run` is the thin imperative shell: resolve `$HOME` / repo,
    // unpack clap args, call the env-free/args-free `pub` cores
    // below. Plan: dogfood-init-setup-in-tests (Phase A).
    match args.command {
        // Repo-team subcommands operate on the repo's single team.
        TeamCmd::Show(a) => show_repo_team(a),
        TeamCmd::Add(a) => team_add(a),
        TeamCmd::Remove(a) => team_remove(a),
        TeamCmd::SetMaster(a) => team_set_master(a),
        TeamCmd::Save(a) => team_save(a),
        // Global library subcommands operate on user-scope teams.
        TeamCmd::List(a) => list(&home_dir()?, a),
        TeamCmd::Delete(a) => delete_team(&home_dir()?, &a.team, a.force),
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

/// `master` + both reviewer tiers as wire strings. The shared
/// `--json` shape for `clank team show` and (flattened under each
/// entry's `name`) `clank team list` (typed-json-not-json-macro).
#[derive(serde::Serialize)]
struct TeamCompJson<'a> {
    master: Option<&'a str>,
    commit_reviewers: Vec<&'a str>,
    gate_reviewers: Vec<&'a str>,
}

impl<'a> TeamCompJson<'a> {
    fn from_comp(comp: &'a TeamComposition) -> Self {
        Self {
            master: comp.master.as_ref().map(|l| l.as_str()),
            commit_reviewers: comp.commit_reviewers.iter().map(|l| l.as_str()).collect(),
            gate_reviewers: comp.gate_reviewers.iter().map(|l| l.as_str()).collect(),
        }
    }
}

/// `clank team list --json` row: the team `name` plus its
/// composition flattened to the top level (matching the prior
/// `json!` which put `name` alongside the three comp keys).
#[derive(serde::Serialize)]
struct TeamListJson<'a> {
    name: &'a str,
    #[serde(flatten)]
    team: TeamCompJson<'a>,
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
        let rows: Vec<TeamListJson> = cfg
            .teams
            .iter()
            .map(|(name, comp)| TeamListJson {
                name,
                team: TeamCompJson::from_comp(comp),
            })
            .collect();
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

/// `clank team show` — print THIS repo's operating team (master +
/// both reviewer tiers). Replaces the old global `team show
/// <name>`. Reads the repo config's own `agents` for tool labels.
fn show_repo_team(args: TeamShowArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    let cfg = read_repo_config(&repo)?;
    let comp = &cfg.team;
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&TeamCompJson::from_comp(comp))?
        );
        return Ok(());
    }
    println!("this repo's team:");
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

// ── repo-team shells (clank team add / remove / set-master) ──

/// `clank team add <label> [--review ...]` — thin shell.
fn team_add(args: TeamAddArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    let label = parse_label(&args.agent)?;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    add_to_repo_team(&repo, home.as_deref(), &label, args.review.into())
}

/// `clank team remove <label>` — thin shell.
fn team_remove(args: TeamRemoveArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    let label = parse_label(&args.agent)?;
    remove_from_repo_team(&repo, &label)
}

/// `clank team set-master <label>` — thin shell. Reuses
/// `agent::promote_repo_master`'s validated body (the old `clank
/// agent promote` logic). Resolves/copies-down the label first so
/// the repo stays self-contained.
fn team_set_master(args: TeamSetMasterArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    let label = parse_label(&args.agent)?;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    set_repo_team_master(&repo, home.as_deref(), &label)
}

/// `clank team save <name> [--force]` — thin shell. Publishes THIS
/// repo's operating team as a reusable global template.
fn team_save(args: TeamSaveArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    save_team(&home_dir()?, &repo, &args.name, args.force)
}

// ── repo-team cores ──────────────────────────────────────────
//
// Operate on `<repo>/.clank/config.json#/team`. `pub`, env-free
// (explicit `repo`/`home`), args-free.

/// Add an existing agent to THIS repo's team's commit (default)
/// or gate reviewer list. Resolves `label`: if defined in the
/// repo's `agents`, use it; else if declared in user-scope
/// `agents`, copy the description DOWN into repo `agents` first;
/// else error. Refuses if already in the team (master or a list).
pub fn add_to_repo_team(
    repo: &Path,
    home: Option<&Path>,
    label: &AgentLabel,
    review: ReviewKind,
) -> anyhow::Result<()> {
    let mut repo_cfg = read_repo_config(repo)?;

    if repo_cfg.team.master.as_ref() == Some(label)
        || repo_cfg.team.commit_reviewers.contains(label)
        || repo_cfg.team.gate_reviewers.contains(label)
    {
        anyhow::bail!("agent `{}` is already in this repo's team", label.as_str());
    }

    // Resolve the definition: repo-local wins, else copy down from
    // user-scope, else error.
    if !repo_cfg.agents.contains_key(label) {
        let user_desc = home
            .map(|h| -> anyhow::Result<Option<AgentDescription>> {
                Ok(read_user_config(h)?.agents.get(label).cloned())
            })
            .transpose()?
            .flatten();
        let desc = user_desc.ok_or_else(|| {
            anyhow::anyhow!(
                "unknown agent `{}` (define it with `clank agent add`)",
                label.as_str()
            )
        })?;
        repo_cfg.agents.insert(label.clone(), desc);
    }

    match review {
        ReviewKind::Commit => repo_cfg.team.commit_reviewers.push(label.clone()),
        ReviewKind::Gate => repo_cfg.team.gate_reviewers.push(label.clone()),
    }
    write_repo_config(repo, &repo_cfg)?;
    eprintln!(
        "added `{}` to this repo's team as a `{}` reviewer",
        label.as_str(),
        match review {
            ReviewKind::Commit => "commit",
            ReviewKind::Gate => "gate",
        }
    );
    Ok(())
}

/// Remove an agent from THIS repo's team: clear master if it's the
/// master, else drop it from whichever reviewer list holds it.
/// LEAVES the definition in `agents`. Errors if not in the team.
pub fn remove_from_repo_team(repo: &Path, label: &AgentLabel) -> anyhow::Result<()> {
    let mut repo_cfg = read_repo_config(repo)?;

    let was_master = repo_cfg.team.master.as_ref() == Some(label);
    if was_master {
        repo_cfg.team.master = None;
    }
    let before = repo_cfg.team.commit_reviewers.len() + repo_cfg.team.gate_reviewers.len();
    repo_cfg.team.commit_reviewers.retain(|l| l != label);
    repo_cfg.team.gate_reviewers.retain(|l| l != label);
    let dropped_reviewer =
        before != repo_cfg.team.commit_reviewers.len() + repo_cfg.team.gate_reviewers.len();

    if !was_master && !dropped_reviewer {
        anyhow::bail!("agent `{}` is not in this repo's team", label.as_str());
    }
    write_repo_config(repo, &repo_cfg)?;
    eprintln!(
        "removed `{}` from this repo's team (definition kept in `agents`)",
        label.as_str()
    );
    Ok(())
}

/// Set THIS repo's team master, demoting the previous master into
/// commit_reviewers. Resolves/copies-down `label` like
/// [`add_to_repo_team`], then delegates the validated promote body
/// to [`crate::cli::agent::promote_repo_master`] (the old `clank
/// agent promote` logic).
pub fn set_repo_team_master(
    repo: &Path,
    home: Option<&Path>,
    label: &AgentLabel,
) -> anyhow::Result<()> {
    // Copy the definition down first if it only exists user-scope,
    // so promote_repo_master's resolver validation passes.
    let mut repo_cfg = read_repo_config(repo)?;
    if !repo_cfg.agents.contains_key(label) {
        let user_desc = home
            .map(|h| -> anyhow::Result<Option<AgentDescription>> {
                Ok(read_user_config(h)?.agents.get(label).cloned())
            })
            .transpose()?
            .flatten();
        let desc = user_desc.ok_or_else(|| {
            anyhow::anyhow!(
                "unknown agent `{}` (define it with `clank agent add`)",
                label.as_str()
            )
        })?;
        repo_cfg.agents.insert(label.clone(), desc);
        write_repo_config(repo, &repo_cfg)?;
    }

    if !crate::cli::agent::promote_repo_master(repo, home, label)? {
        anyhow::bail!("this repo has no config. Run `clank init` first.");
    }
    Ok(())
}

/// Publish THIS repo's operating team as a reusable GLOBAL
/// template named `name`. `pub`, env-free (explicit `home` +
/// `repo`), args-free.
///
/// The agents published are exactly the team's referenced labels
/// (master if set + every commit/gate reviewer), looked up in the
/// repo's own `agents`. Repo agents NOT on the team are not
/// published.
///
/// Fail-closed, collision check BEFORE any write — no partial
/// writes:
/// - The repo config is read through the same fail-closed loader
///   the resolver uses ([`crate::agent_store::load_repo_config_required`]),
///   so an old-shape repo config yields the re-init hint.
/// - For each referenced agent, its repo body is compared to any
///   identically-named user-scope agent: absent → will insert;
///   equal → no-op; DIFFERENT → HARD ERROR (not `--force`-able).
/// - An existing user-scope team of the same `name` requires
///   `--force` (clank's non-interactive confirm idiom).
/// - A team with NO master is allowed (a faithful partial
///   snapshot).
pub fn save_team(home: &Path, repo: &Path, name: &str, force: bool) -> anyhow::Result<()> {
    let repo_cfg = crate::agent_store::load_repo_config_required(repo)?;
    let mut user_cfg = read_user_config(home)?;

    // The agents to publish = the team's referenced labels, looked
    // up in the repo's own `agents`. A label referenced by the team
    // but absent from repo `agents` is a corrupt repo config — fail
    // closed rather than publish a dangling template.
    let referenced: Vec<&AgentLabel> = repo_cfg
        .team
        .master
        .iter()
        .chain(repo_cfg.team.commit_reviewers.iter())
        .chain(repo_cfg.team.gate_reviewers.iter())
        .collect();

    let mut to_resolve: Vec<(&AgentLabel, &AgentDescription)> = Vec::new();
    for label in &referenced {
        let desc = repo_cfg.agents.get(*label).ok_or_else(|| {
            anyhow::anyhow!(
                "team references agent `{}`, which is not defined in this repo's `agents`; \
                 re-run `clank init` to recreate the config",
                label.as_str()
            )
        })?;
        to_resolve.push((label, desc));
    }

    // Collision check FIRST, before any write (fail-closed). The
    // team-name overwrite check is also done here so the whole
    // operation either fully succeeds or leaves the user config
    // untouched.
    if user_cfg.teams.contains_key(name) && !force {
        anyhow::bail!("team `{name}` already exists in user-scope; pass --force to overwrite");
    }

    let mut to_insert: Vec<(AgentLabel, AgentDescription)> = Vec::new();
    let mut already_present: Vec<&AgentLabel> = Vec::new();
    for (label, repo_desc) in &to_resolve {
        match user_cfg.agents.get(*label) {
            None => to_insert.push(((*label).clone(), (*repo_desc).clone())),
            Some(global_desc) if global_desc == *repo_desc => already_present.push(label),
            Some(_) => anyhow::bail!(
                "agent `{}` already exists in user-scope `agents` with a different definition; \
                 rename it or reconcile before saving",
                label.as_str()
            ),
        }
    }

    // All checks passed — apply (single write).
    for (label, desc) in &to_insert {
        user_cfg.agents.insert(label.clone(), desc.clone());
    }
    user_cfg
        .teams
        .insert(name.to_string(), repo_cfg.team.clone());
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

/// Read `<repo>/.clank/config.json` as the typed
/// [`crate::cli::teams_config::RepoConfigFile`]. Missing file →
/// default; malformed JSON → error.
fn read_repo_config(repo: &Path) -> anyhow::Result<crate::cli::teams_config::RepoConfigFile> {
    use crate::cli::teams_config::RepoConfigFile;
    let path = repo.join(".clank/config.json");
    match std::fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s).with_context(|| format!("parsing {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(RepoConfigFile::default()),
        Err(e) => Err(anyhow::Error::from(e).context(format!("reading {}", path.display()))),
    }
}

/// Atomic write of `<repo>/.clank/config.json`.
fn write_repo_config(
    repo: &Path,
    file: &crate::cli::teams_config::RepoConfigFile,
) -> anyhow::Result<()> {
    write_typed_config(&repo.join(".clank/config.json"), file)
}

// ── global team-template library cores ───────────────────────
//
// `pub`, env-free (take an explicit `home: &Path`), args-free.
// `list` / `delete` are wired to `run()`; `create_team` /
// `set_master` / `add_member` / `remove_member` MINT a named
// user-scope template (the path `clank init --team` consumes and
// M3's `team save` will formalize). They are no longer wired to a
// `clank team` subcommand — in-place template editing was dropped
// — but remain the dogfood builders the integration-test setup
// uses to construct a template before `register_repo_team` copies
// it down. Plan: dogfood-init-setup-in-tests (Phase A). These do
// file IO (read/write `~/.clank/config.json`) — "core" means no
// env/clap, not pure.

/// Create an empty user-scope team. Errors if it already exists.
pub fn create_team(home: &Path, team: &str) -> anyhow::Result<()> {
    let mut cfg = read_user_config(home)?;
    if cfg.teams.contains_key(team) {
        anyhow::bail!("team `{team}` already exists in user-scope `teams`");
    }
    cfg.teams
        .insert(team.to_string(), TeamComposition::default());
    write_user_config(home, &cfg)?;
    eprintln!("created empty team template `{team}` in user-scope");
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

    fn seed_repo_config(repo: &Path, body: &str) {
        std::fs::create_dir_all(repo.join(".clank")).unwrap();
        std::fs::write(repo.join(".clank/config.json"), body).unwrap();
    }

    fn read_repo(repo: &Path) -> crate::cli::teams_config::RepoConfigFile {
        let body = std::fs::read_to_string(repo.join(".clank/config.json")).unwrap();
        serde_json::from_str(&body).unwrap()
    }

    // ── typed-json-not-json-macro: team --json wire shapes ──────

    #[test]
    fn team_json_shapes_match_prior() {
        // TeamCompJson (clank team show --json) and TeamListJson
        // (clank team list --json, name flattened with the comp) must
        // serialize to the same keys+values the old `json!` produced.
        // `json!` expresses the expected value; key order is
        // irrelevant (`to_value` equality is order-independent).
        let mut comp = TeamComposition::default();
        comp.master = Some(AgentLabel::parse("claude").unwrap());
        comp.commit_reviewers
            .push(AgentLabel::parse("codex").unwrap());
        comp.gate_reviewers
            .push(AgentLabel::parse("ruthless").unwrap());

        assert_eq!(
            serde_json::to_value(TeamCompJson::from_comp(&comp)).unwrap(),
            serde_json::json!({
                "master": "claude",
                "commit_reviewers": ["codex"],
                "gate_reviewers": ["ruthless"],
            })
        );

        let row = TeamListJson {
            name: "dev",
            team: TeamCompJson::from_comp(&comp),
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

        // master unset → null (matches the prior Option mapping).
        let empty = TeamComposition::default();
        assert_eq!(
            serde_json::to_value(TeamCompJson::from_comp(&empty)).unwrap(),
            serde_json::json!({
                "master": null,
                "commit_reviewers": [],
                "gate_reviewers": [],
            })
        );
    }

    // ── repo-team cores (clank team add / remove / set-master) ──

    #[test]
    fn add_to_repo_team_appends_local_agent_to_commit_tier() {
        let repo = tempfile::tempdir().unwrap();
        seed_repo_config(
            repo.path(),
            r#"{
                "agents": { "claude": { "tool": "claude" }, "codex": { "tool": "codex" } },
                "team": { "master": "claude" }
            }"#,
        );
        let codex = AgentLabel::parse("codex").unwrap();
        add_to_repo_team(repo.path(), None, &codex, ReviewKind::Commit).unwrap();
        let parsed = read_repo(repo.path());
        assert_eq!(parsed.team.commit_reviewers, vec![codex]);
    }

    #[test]
    fn add_to_repo_team_copies_global_only_agent_down() {
        // The label is NOT in repo `agents` but IS in user-scope
        // `agents` → its description is copied down, then added.
        let home_dir = setup_home();
        let home = home_dir.path();
        seed_user_config(home, &[("ruthless", Tool::Claude)], &[]);
        let repo = tempfile::tempdir().unwrap();
        seed_repo_config(
            repo.path(),
            r#"{
                "agents": { "claude": { "tool": "claude" } },
                "team": { "master": "claude" }
            }"#,
        );
        let ruthless = AgentLabel::parse("ruthless").unwrap();
        add_to_repo_team(repo.path(), Some(home), &ruthless, ReviewKind::Gate).unwrap();
        let parsed = read_repo(repo.path());
        // Description copied down so the repo stays self-contained.
        assert!(parsed.agents.contains_key(&ruthless));
        assert_eq!(parsed.agents[&ruthless].tool, Tool::Claude);
        assert_eq!(parsed.team.gate_reviewers, vec![ruthless]);
    }

    #[test]
    fn add_to_repo_team_errors_on_unknown_agent() {
        let home_dir = setup_home();
        let repo = tempfile::tempdir().unwrap();
        seed_repo_config(
            repo.path(),
            r#"{
                "agents": { "claude": { "tool": "claude" } },
                "team": { "master": "claude" }
            }"#,
        );
        let phantom = AgentLabel::parse("phantom").unwrap();
        let err = add_to_repo_team(
            repo.path(),
            Some(home_dir.path()),
            &phantom,
            ReviewKind::Commit,
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("unknown agent `phantom`"));
        assert!(msg.contains("clank agent add"));
    }

    #[test]
    fn add_to_repo_team_refuses_agent_already_in_team() {
        let repo = tempfile::tempdir().unwrap();
        seed_repo_config(
            repo.path(),
            r#"{
                "agents": { "claude": { "tool": "claude" }, "codex": { "tool": "codex" } },
                "team": { "master": "claude", "commit_reviewers": ["codex"] }
            }"#,
        );
        let codex = AgentLabel::parse("codex").unwrap();
        let err = add_to_repo_team(repo.path(), None, &codex, ReviewKind::Gate).unwrap_err();
        assert!(format!("{err:#}").contains("already in this repo's team"));
        // master is also refused.
        let claude = AgentLabel::parse("claude").unwrap();
        let err = add_to_repo_team(repo.path(), None, &claude, ReviewKind::Commit).unwrap_err();
        assert!(format!("{err:#}").contains("already in this repo's team"));
    }

    #[test]
    fn remove_from_repo_team_drops_reviewer_but_keeps_definition() {
        let repo = tempfile::tempdir().unwrap();
        seed_repo_config(
            repo.path(),
            r#"{
                "agents": { "claude": { "tool": "claude" }, "codex": { "tool": "codex" } },
                "team": { "master": "claude", "commit_reviewers": ["codex"] }
            }"#,
        );
        let codex = AgentLabel::parse("codex").unwrap();
        remove_from_repo_team(repo.path(), &codex).unwrap();
        let parsed = read_repo(repo.path());
        assert!(parsed.team.commit_reviewers.is_empty());
        // Definition is kept.
        assert!(parsed.agents.contains_key(&codex));
    }

    #[test]
    fn remove_from_repo_team_clears_master() {
        let repo = tempfile::tempdir().unwrap();
        seed_repo_config(
            repo.path(),
            r#"{
                "agents": { "claude": { "tool": "claude" } },
                "team": { "master": "claude" }
            }"#,
        );
        let claude = AgentLabel::parse("claude").unwrap();
        remove_from_repo_team(repo.path(), &claude).unwrap();
        let parsed = read_repo(repo.path());
        assert!(parsed.team.master.is_none());
        // Definition kept.
        assert!(parsed.agents.contains_key(&claude));
    }

    #[test]
    fn remove_from_repo_team_errors_when_not_in_team() {
        let repo = tempfile::tempdir().unwrap();
        seed_repo_config(
            repo.path(),
            r#"{
                "agents": { "claude": { "tool": "claude" }, "spare": { "tool": "codex" } },
                "team": { "master": "claude" }
            }"#,
        );
        // `spare` is defined but not in the team.
        let spare = AgentLabel::parse("spare").unwrap();
        let err = remove_from_repo_team(repo.path(), &spare).unwrap_err();
        assert!(format!("{err:#}").contains("not in this repo's team"));
    }

    #[test]
    fn set_repo_team_master_sets_master_and_demotes_previous() {
        let repo = tempfile::tempdir().unwrap();
        seed_repo_config(
            repo.path(),
            r#"{
                "agents": { "claude": { "tool": "claude" }, "codex": { "tool": "codex" } },
                "team": { "master": "claude", "commit_reviewers": ["codex"] }
            }"#,
        );
        let codex = AgentLabel::parse("codex").unwrap();
        set_repo_team_master(repo.path(), None, &codex).unwrap();
        let parsed = read_repo(repo.path());
        assert_eq!(
            parsed.team.master.as_ref().map(|l| l.as_str()),
            Some("codex")
        );
        // Previous master (claude) demoted to commit_reviewers;
        // codex removed from there.
        let cr: Vec<_> = parsed
            .team
            .commit_reviewers
            .iter()
            .map(|l| l.as_str())
            .collect();
        assert!(cr.contains(&"claude"));
        assert!(!cr.contains(&"codex"));
    }

    #[test]
    fn set_repo_team_master_copies_global_only_agent_down() {
        let home_dir = setup_home();
        let home = home_dir.path();
        seed_user_config(home, &[("boss", Tool::Claude)], &[]);
        let repo = tempfile::tempdir().unwrap();
        seed_repo_config(
            repo.path(),
            r#"{
                "agents": { "claude": { "tool": "claude" } },
                "team": { "master": "claude" }
            }"#,
        );
        let boss = AgentLabel::parse("boss").unwrap();
        set_repo_team_master(repo.path(), Some(home), &boss).unwrap();
        let parsed = read_repo(repo.path());
        // boss copied down + set as master.
        assert!(parsed.agents.contains_key(&boss));
        assert_eq!(
            parsed.team.master.as_ref().map(|l| l.as_str()),
            Some("boss")
        );
        // claude demoted.
        assert!(
            parsed
                .team
                .commit_reviewers
                .iter()
                .any(|l| l.as_str() == "claude")
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

    // ── team save (M3) ───────────────────────────────────────────

    #[test]
    fn save_team_publishes_new_template_and_copies_referenced_agents() {
        let home_dir = setup_home();
        let home = home_dir.path();
        // Empty global config to start.
        seed_user_config(home, &[], &[]);
        let repo = tempfile::tempdir().unwrap();
        // `spare` is defined in repo agents but NOT on the team —
        // it must NOT be published.
        seed_repo_config(
            repo.path(),
            r#"{
                "agents": {
                    "claude": { "tool": "claude" },
                    "codex": { "tool": "codex" },
                    "spare": { "tool": "codex" }
                },
                "team": { "master": "claude", "commit_reviewers": ["codex"] }
            }"#,
        );

        save_team(home, repo.path(), "dev", false).unwrap();

        let cfg = read_user_config(home).unwrap();
        let dev = cfg.teams.get("dev").unwrap();
        assert_eq!(dev.master.as_ref().unwrap().as_str(), "claude");
        assert_eq!(dev.commit_reviewers[0].as_str(), "codex");
        // Only referenced agents copied; `spare` excluded.
        assert!(
            cfg.agents
                .contains_key(&AgentLabel::parse("claude").unwrap())
        );
        assert!(
            cfg.agents
                .contains_key(&AgentLabel::parse("codex").unwrap())
        );
        assert!(
            !cfg.agents
                .contains_key(&AgentLabel::parse("spare").unwrap())
        );
    }

    #[test]
    fn save_team_identical_agent_resave_is_noop_not_error() {
        let home_dir = setup_home();
        let home = home_dir.path();
        // `claude` already in global with the SAME body as the repo.
        seed_user_config(home, &[("claude", Tool::Claude)], &[]);
        let repo = tempfile::tempdir().unwrap();
        seed_repo_config(
            repo.path(),
            r#"{
                "agents": { "claude": { "tool": "claude" } },
                "team": { "master": "claude" }
            }"#,
        );

        // Identical agent → no error.
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
            r#"{
                "agents": { "claude": { "tool": "claude" } },
                "team": { "master": "claude" }
            }"#,
        );

        // Not bypassable by --force.
        for force in [false, true] {
            let err = save_team(home, repo.path(), "dev", force).unwrap_err();
            let msg = format!("{err:#}");
            assert!(msg.contains("different definition"), "force={force}: {msg}");
        }
        // No partial write: team not created, global agent unchanged.
        let cfg = read_user_config(home).unwrap();
        assert!(!cfg.teams.contains_key("dev"));
        assert_eq!(
            cfg.agents[&AgentLabel::parse("claude").unwrap()].tool,
            Tool::Codex
        );
    }

    #[test]
    fn save_team_existing_name_requires_force() {
        let home_dir = setup_home();
        let home = home_dir.path();
        let existing = TeamComposition::default();
        seed_user_config(home, &[], &[("dev", &existing)]);
        let repo = tempfile::tempdir().unwrap();
        seed_repo_config(
            repo.path(),
            r#"{
                "agents": { "claude": { "tool": "claude" } },
                "team": { "master": "claude" }
            }"#,
        );

        // Without --force: refuse.
        let err = save_team(home, repo.path(), "dev", false).unwrap_err();
        assert!(format!("{err:#}").contains("--force"));
        // The existing (empty) team is untouched.
        let cfg = read_user_config(home).unwrap();
        assert!(cfg.teams.get("dev").unwrap().master.is_none());

        // With --force: overwrite.
        save_team(home, repo.path(), "dev", true).unwrap();
        let cfg = read_user_config(home).unwrap();
        assert_eq!(
            cfg.teams
                .get("dev")
                .unwrap()
                .master
                .as_ref()
                .unwrap()
                .as_str(),
            "claude"
        );
    }

    #[test]
    fn save_team_without_master_is_allowed() {
        let home_dir = setup_home();
        let home = home_dir.path();
        seed_user_config(home, &[], &[]);
        let repo = tempfile::tempdir().unwrap();
        // No master, just a commit reviewer.
        seed_repo_config(
            repo.path(),
            r#"{
                "agents": { "codex": { "tool": "codex" } },
                "team": { "commit_reviewers": ["codex"] }
            }"#,
        );

        save_team(home, repo.path(), "partial", false).unwrap();
        let cfg = read_user_config(home).unwrap();
        let team = cfg.teams.get("partial").unwrap();
        assert!(team.master.is_none());
        assert_eq!(team.commit_reviewers[0].as_str(), "codex");
        assert!(
            cfg.agents
                .contains_key(&AgentLabel::parse("codex").unwrap())
        );
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
}
