//! `clank agent` — enumerate / launch / declare agents under the
//! team-based registration model (`teams-based-agent-registration`).
//!
//! Source of truth for "who is registered in this repo" is the
//! TEAM RESOLVER (`agent_store::try_resolve_via_team`): a repo's
//! `team` field plus the user-scope `agents` + `teams` maps.
//! The per-agent skeleton at `.clank/agents/<label>/config.json`
//! holds ONLY per-machine state (`auto_mode`, `wfw_timeout`,
//! `session`) — no role / tool / launch declaration. `list`
//! joins the resolved set with each agent's skeleton session.

use std::path::Path;

use anyhow::Context;
use serde::Serialize;

use crate::agent_store::load_agent_config;
use crate::cli::teams_config::{
    AgentDescription, ByNameEntry, InlineAgent, RepoConfigFile, ReviewKind, TeamEntry, TeamField,
    UserConfigFile,
};
use clank_core::agent_config::{LaunchConfig, Session};
use clank_core::ids::AgentLabel;
use clank_core::vocab::{AutoMode, Tool};
use std::collections::BTreeMap;

use super::{
    AgentAddArgs, AgentArgs, AgentCmd, AgentListArgs, AgentPromoteArgs, AgentRemoveArgs,
    AgentStartArgs, resolve_repo,
};

pub async fn run(args: AgentArgs) -> anyhow::Result<()> {
    match args.command {
        AgentCmd::List(a) => list(a),
        AgentCmd::Start(a) => start(a),
        AgentCmd::Add(a) => add(a),
        AgentCmd::Remove(a) => remove(a),
        AgentCmd::Promote(a) => promote(a),
    }
}

#[derive(Debug, Serialize)]
struct AgentRow {
    label: String,
    role: String,
    /// `—` for master; `commit` / `gate` for reviewers.
    review: String,
    bound: bool,
    tool: Option<String>,
    session_id: Option<String>,
}

impl AgentRow {
    fn from_join(
        label: &AgentLabel,
        role: &str,
        review: &str,
        declared_tool: Tool,
        session: Option<&Session>,
    ) -> Self {
        let (bound, tool_str, session_id) = match session {
            // Session.tool wins for the displayed tool when bound —
            // it's the tool actually running the agent.
            Some(s) => (
                true,
                Some(s.tool.as_str().to_string()),
                Some(s.id.as_str().to_string()),
            ),
            None => (false, Some(declared_tool.as_str().to_string()), None),
        };
        Self {
            label: label.as_str().to_string(),
            role: role.to_string(),
            review: review.to_string(),
            bound,
            tool: tool_str,
            session_id,
        }
    }
}

fn list(args: AgentListArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    // Registration is the resolved team set; skeleton supplies
    // session state only.
    let Some(set) = crate::agent_store::try_resolve_via_team(&repo)? else {
        anyhow::bail!(
            "this repo has no team configured. Run `clank init --team <name>` to set one."
        );
    };
    let mut rows: Vec<AgentRow> = Vec::new();
    // Master first.
    {
        let skeleton = load_agent_config(&repo, &set.master)?;
        rows.push(AgentRow::from_join(
            &set.master,
            "master",
            "—",
            set.master_desc.tool,
            skeleton.as_ref().and_then(|c| c.session.as_ref()),
        ));
    }
    for r in &set.commit_reviewers {
        let skeleton = load_agent_config(&repo, &r.label)?;
        rows.push(AgentRow::from_join(
            &r.label,
            "reviewer",
            "commit",
            r.desc.tool,
            skeleton.as_ref().and_then(|c| c.session.as_ref()),
        ));
    }
    for r in &set.gate_reviewers {
        let skeleton = load_agent_config(&repo, &r.label)?;
        rows.push(AgentRow::from_join(
            &r.label,
            "reviewer",
            "gate",
            r.desc.tool,
            skeleton.as_ref().and_then(|c| c.session.as_ref()),
        ));
    }
    if args.json {
        let s = serde_json::to_string_pretty(&rows)?;
        println!("{s}");
    } else {
        print_human(&rows);
    }
    Ok(())
}

fn print_human(rows: &[AgentRow]) {
    if rows.is_empty() {
        println!("no agents registered");
        return;
    }
    // Column widths sized to the data.
    let w_label = rows.iter().map(|r| r.label.len()).max().unwrap_or(5).max(5);
    let w_role = rows.iter().map(|r| r.role.len()).max().unwrap_or(4).max(4);
    let w_bound = 5; // "BOUND"
    let w_tool = rows
        .iter()
        .map(|r| r.tool.as_deref().unwrap_or("—").len())
        .max()
        .unwrap_or(4)
        .max(4);
    println!(
        "{:<w_label$}  {:<w_role$}  {:<w_bound$}  {:<w_tool$}  SESSION",
        "LABEL", "ROLE", "BOUND", "TOOL"
    );
    for r in rows {
        let bound = if r.bound { "yes" } else { "NO" };
        let tool = r.tool.as_deref().unwrap_or("—");
        let session = r.session_id.as_deref().unwrap_or("—");
        // Shorten session ID for the human view; JSON keeps the full id.
        let session_short: String = session.chars().take(8).collect();
        let session_display = if session == "—" {
            "—".to_string()
        } else {
            format!("{session_short}…")
        };
        println!(
            "{:<w_label$}  {:<w_role$}  {:<w_bound$}  {:<w_tool$}  {session_display}",
            r.label, r.role, bound, tool
        );
    }
}

/// `clank agent start <name>`: exec into the agent's CLI tool.
///
/// Two paths:
/// - **Resume** (declaration + bound session): exec with the
///   configured launch profile + session-restore suffix.
/// - **Bootstrap** (declaration but no bound session — skeleton
///   missing OR `session: None`): exec the bare tool with a seed
///   prompt instructing the agent to run `clank as <label>`. After
///   that bind, subsequent calls hit the resume path. See
///   `compose_bootstrap_launch` for the program-resolution policy.
///   Plan: `agent-start-bootstraps-missing-skeleton`.
///
/// Launch profile source: the merged declaration's `launch`
/// field (per `agent-add-cli-and-repo-scope`). Session binding
/// state comes from the per-agent skeleton.
fn start(args: AgentStartArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    let label = AgentLabel::parse(&args.name)
        .map_err(|e| anyhow::anyhow!("invalid agent label `{}`: {e}", args.name))?;

    // Registration is the resolved team set. The agent's
    // description (tool / launch / initial_prompt) comes from
    // there; the skeleton supplies only session + auto_mode.
    let Some(set) = crate::agent_store::try_resolve_via_team(&repo)? else {
        anyhow::bail!(
            "this repo has no team configured. Run `clank init --team <name>` to set one."
        );
    };
    let desc = find_in_set(&set, &label).ok_or_else(|| {
        anyhow::anyhow!(
            "no agent `{}` registered in this repo's team (master/reviewers: {})",
            args.name,
            registered_labels(&set).join(", ")
        )
    })?;

    let cfg = load_agent_config(&repo, &label)?;
    let bound_session = cfg.as_ref().and_then(|c| c.session.as_ref());

    let composed = match bound_session {
        None => compose_bootstrap_launch(&label, &desc)?,
        Some(session) => {
            let auto_mode = cfg.as_ref().map(|c| c.auto_mode).unwrap_or_default();
            let resolved_prompt = resolve_initial_prompt(desc.initial_prompt.as_deref(), auto_mode);
            compose_launch(
                &repo,
                session,
                desc.launch.as_ref(),
                resolved_prompt.as_deref(),
            )
        }
    };

    if args.print {
        print_composed(&composed);
        return Ok(());
    }

    exec_composed(composed)
}

/// Find an agent's resolved `AgentDescription` in the registered
/// set, whether it's the master or a reviewer in either tier.
fn find_in_set(
    set: &crate::cli::teams_config::RegisteredSet,
    label: &AgentLabel,
) -> Option<AgentDescription> {
    if &set.master == label {
        return Some(set.master_desc.clone());
    }
    set.commit_reviewers
        .iter()
        .chain(set.gate_reviewers.iter())
        .find(|a| &a.label == label)
        .map(|a| a.desc.clone())
}

fn registered_labels(set: &crate::cli::teams_config::RegisteredSet) -> Vec<String> {
    let mut out = vec![set.master.as_str().to_string()];
    out.extend(
        set.commit_reviewers
            .iter()
            .chain(set.gate_reviewers.iter())
            .map(|a| a.label.as_str().to_string()),
    );
    out
}

/// Bootstrap launch: a registered agent has no bound session in
/// this repo. Spawn the tool (from its `AgentDescription`) with a
/// seed prompt instructing the agent to run `clank as <label>`,
/// which creates the skeleton + session binding. Subsequent
/// `clank agent start <label>` calls resume normally.
fn compose_bootstrap_launch(
    label: &AgentLabel,
    desc: &AgentDescription,
) -> anyhow::Result<ComposedLaunch> {
    let program = desc
        .launch
        .as_ref()
        .and_then(|l| l.command.clone())
        .unwrap_or_else(|| desc.tool.as_str().to_string());

    let mut args = desc
        .launch
        .as_ref()
        .map(|l| l.args.clone())
        .unwrap_or_default();
    args.push(bootstrap_bind_prompt(label));

    let env_overrides = desc
        .launch
        .as_ref()
        .map(|l| l.env.clone())
        .unwrap_or_default();

    Ok(ComposedLaunch {
        program,
        args,
        env_overrides,
    })
}

/// Seed prompt for the bootstrap launch. Verbatim per the plan's
/// PINNED string — `bootstrap_uses_tool_from_declaration` test
/// asserts on this with `assert_eq!`, so a wording tweak fails the
/// test deliberately.
pub(super) fn bootstrap_bind_prompt(label: &AgentLabel) -> String {
    format!("Run `clank as {}` to bind this session.", label.as_str())
}

/// Composed launch line: executable + argv + env additions.
#[derive(Debug, Clone)]
struct ComposedLaunch {
    program: String,
    args: Vec<String>,
    env_overrides: std::collections::BTreeMap<String, String>,
}

/// Default `initial_prompt` when `auto_mode == On` and the
/// declaration's `initial_prompt` field is unset. Verbatim per
/// `agent-start-initial-prompt` Phase 3:
/// - Triggers turn-end with minimum surface (both tools produce
///   a one-line ack).
/// - Does NOT instruct the agent to run `clank wfw` itself (the
///   stop hook is the orchestrator; double-trigger to avoid).
/// - Does NOT expose orchestration internals (no "stop hook",
///   no "work loop") — agent only learns contextual location.
///
/// Test `resolve_initial_prompt_uses_default_when_auto_on_and_declaration_unset`
/// asserts on this string with `assert_eq!`, so a tweak fails
/// the test deliberately.
pub(super) const DEFAULT_AUTO_PROMPT: &str = "Session resumed.";

/// Resolve the initial prompt for an agent-start invocation.
///
/// Three-input policy:
/// - `declaration`: `DefaultAgent.initial_prompt`. `Some("")`
///   means "explicitly disable" — falls back to None, NOT
///   Some("") through. Lets a user with auto_mode=On opt out
///   of the prompt without disabling auto_mode itself.
/// - `auto_mode`: per-machine preference from the agent skeleton.
///   When On, supplies the [`DEFAULT_AUTO_PROMPT`] fallback.
/// - Returns the resolved prompt or None.
pub(super) fn resolve_initial_prompt(
    declaration: Option<&str>,
    auto_mode: AutoMode,
) -> Option<String> {
    if let Some(s) = declaration {
        if s.is_empty() {
            return None;
        }
        return Some(s.to_string());
    }
    if auto_mode == AutoMode::On {
        return Some(DEFAULT_AUTO_PROMPT.to_string());
    }
    None
}

fn compose_launch(
    repo: &Path,
    session: &Session,
    launch: Option<&LaunchConfig>,
    initial_prompt: Option<&str>,
) -> ComposedLaunch {
    let tool = session.tool;
    let program = launch
        .and_then(|l| l.command.clone())
        .unwrap_or_else(|| tool.as_str().to_string());

    let launch_args = launch.map(|l| l.args.clone()).unwrap_or_default();

    let session_restore = session_restore_args(tool, session.id.as_str(), repo);

    let mut args = launch_args;
    args.extend(session_restore);
    if let Some(prompt) = initial_prompt {
        args.push(prompt.to_string());
    }

    let env_overrides = launch.map(|l| l.env.clone()).unwrap_or_default();

    ComposedLaunch {
        program,
        args,
        env_overrides,
    }
}

/// Tool-specific session-restore suffix. Goes AFTER `launch.args`
/// so codex's `resume` subcommand doesn't capture them (see Phase
/// B step 3 of the plan). The initial prompt (when present) is
/// appended by [`compose_launch`] AFTER this suffix — for codex
/// that means the prompt is the final positional, AFTER `--cd
/// <repo>` (clap parses options + positionals independently, so
/// the option/positional ordering is flexible).
fn session_restore_args(tool: Tool, session_id: &str, repo: &Path) -> Vec<String> {
    match tool {
        Tool::Claude => vec!["--resume".into(), session_id.into()],
        Tool::Codex => vec![
            "resume".into(),
            session_id.into(),
            "--cd".into(),
            repo.to_string_lossy().into_owned(),
        ],
    }
}

/// Print the composed launch: shell-quoted argv on stdout, env
/// diff on stderr. Used by `--print` for tests + zellij-style
/// introspection.
fn print_composed(c: &ComposedLaunch) {
    let mut line = shell_quote(&c.program);
    for a in &c.args {
        line.push(' ');
        line.push_str(&shell_quote(a));
    }
    println!("{line}");
    if !c.env_overrides.is_empty() {
        for (k, v) in &c.env_overrides {
            eprintln!("env: {k}={v}");
        }
    }
}

fn shell_quote(s: &str) -> String {
    // POSIX single-quote escaping. Closes out for embedded
    // single quotes via the standard `'\''` dance.
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

#[cfg(unix)]
fn exec_composed(c: ComposedLaunch) -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;
    let mut cmd = std::process::Command::new(&c.program);
    cmd.args(&c.args);
    for (k, v) in &c.env_overrides {
        cmd.env(k, v);
    }
    // `exec` replaces the current process; the only return
    // value is the io::Error if exec itself fails (e.g.
    // command-not-found, permissions).
    let err = cmd.exec();
    Err(anyhow::anyhow!("failed to exec `{}`: {err}", c.program))
}

#[cfg(not(unix))]
fn exec_composed(c: ComposedLaunch) -> anyhow::Result<()> {
    // Non-unix fallback: spawn + wait. Process replacement isn't
    // available on Windows; the calling shell sees clank exit
    // with the child's status.
    let status = std::process::Command::new(&c.program)
        .args(&c.args)
        .envs(c.env_overrides.iter())
        .status()
        .map_err(|e| anyhow::anyhow!("failed to spawn `{}`: {e}", c.program))?;
    if !status.success() {
        anyhow::bail!("`{}` exited with status {}", c.program, status);
    }
    Ok(())
}

// ── clank agent add / remove / promote (team model) ──────────
//
// `--global` edits user-scope `~/.clank/config.json#/agents`
// (agent DESCRIPTIONS, reusable across teams). Repo-scope edits
// THIS repo's `team` array (local entries). `promote` writes the
// repo `promoted` field. Per-agent skeletons hold only state
// (session / auto_mode / wfw_timeout) — never declaration.

/// `clank agent add <label> [...]`.
fn add(args: AgentAddArgs) -> anyhow::Result<()> {
    let label = AgentLabel::parse(&args.label)
        .map_err(|e| anyhow::anyhow!("invalid agent label `{}`: {e}", args.label))?;
    let repo = resolve_repo(args.repo.as_deref())?;
    let env = parse_env_overrides(&args.launch_envs)?;
    let launch = build_launch(args.launch_cmd, args.launch_args, env);

    if args.global {
        let tool: Tool = args
            .tool
            .ok_or_else(|| anyhow::anyhow!("--global add requires --tool <claude|codex>"))?
            .into();
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        let home_ref = home
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("--global requires $HOME"))?;
        let mut file = read_user_config(home_ref)?;
        if file.agents.contains_key(&label) {
            anyhow::bail!(
                "agent `{}` already declared in user-scope `agents`",
                args.label
            );
        }
        file.agents.insert(
            label.clone(),
            AgentDescription {
                tool,
                launch,
                initial_prompt: args.initial_prompt.clone(),
            },
        );
        write_user_config(home_ref, &file)?;
        eprintln!("declared `{}` in user-scope `agents`", args.label);
    } else {
        // Repo-scope: append a local entry to this repo's `team`
        // array.
        let mut repo_cfg = read_repo_config(&repo)?;
        let existing = repo_cfg.team.take().ok_or_else(|| {
            anyhow::anyhow!(
                "this repo has no team set. Run `clank init --team <name>` before adding local agents."
            )
        })?;
        let mut entries = team_field_into_entries(existing);
        if team_entries_contain(&entries, &label) {
            anyhow::bail!("agent `{}` is already in this repo's team", args.label);
        }
        let review = args
            .review
            .map(ReviewKind::from)
            .unwrap_or(ReviewKind::Commit);
        let entry = match args.tool {
            Some(tool_arg) => TeamEntry::Inline(InlineAgent {
                label: label.clone(),
                tool: tool_arg.into(),
                launch,
                initial_prompt: args.initial_prompt.clone(),
                role: None,
                review: Some(review),
            }),
            None => TeamEntry::ByName(ByNameEntry {
                agent: label.clone(),
                review: Some(review),
            }),
        };
        entries.push(entry);
        repo_cfg.team = Some(TeamField::Array(entries));
        write_repo_config(&repo, &repo_cfg)?;
        eprintln!(
            "added `{}` to this repo's team as a `{}` reviewer",
            args.label,
            match review {
                ReviewKind::Commit => "commit",
                ReviewKind::Gate => "gate",
            }
        );
    }
    Ok(())
}

/// `clank agent remove <label> [--global]`.
fn remove(args: AgentRemoveArgs) -> anyhow::Result<()> {
    let label = AgentLabel::parse(&args.label)
        .map_err(|e| anyhow::anyhow!("invalid agent label `{}`: {e}", args.label))?;
    let repo = resolve_repo(args.repo.as_deref())?;

    if args.global {
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        let home_ref = home
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("--global requires $HOME"))?;
        let mut file = read_user_config(home_ref)?;
        if file.agents.remove(&label).is_none() {
            anyhow::bail!("agent `{}` not in user-scope `agents`", args.label);
        }
        // Scrub the label from every team composition so no
        // dangling reference survives (it would error at
        // resolution time otherwise).
        let mut touched = Vec::new();
        for (name, team) in file.teams.iter_mut() {
            let before = team.commit_reviewers.len()
                + team.gate_reviewers.len()
                + usize::from(team.master.is_some());
            if team.master.as_ref() == Some(&label) {
                team.master = None;
            }
            team.commit_reviewers.retain(|l| l != &label);
            team.gate_reviewers.retain(|l| l != &label);
            let after = team.commit_reviewers.len()
                + team.gate_reviewers.len()
                + usize::from(team.master.is_some());
            if before != after {
                touched.push(name.clone());
            }
        }
        write_user_config(home_ref, &file)?;
        if touched.is_empty() {
            eprintln!("removed `{}` from user-scope `agents`", args.label);
        } else {
            eprintln!(
                "removed `{}` from user-scope `agents` (and from teams: {})",
                args.label,
                touched.join(", ")
            );
        }
    } else {
        let mut repo_cfg = read_repo_config(&repo)?;
        let existing = repo_cfg
            .team
            .take()
            .ok_or_else(|| anyhow::anyhow!("this repo has no team set; nothing to remove"))?;
        let entries = team_field_into_entries(existing);
        let before = entries.len();
        let kept: Vec<TeamEntry> = entries
            .into_iter()
            .filter(|e| !team_entry_matches(e, &label))
            .collect();
        if kept.len() == before {
            anyhow::bail!(
                "agent `{}` is not a local entry in this repo's team",
                args.label
            );
        }
        repo_cfg.team = Some(TeamField::Array(kept));
        write_repo_config(&repo, &repo_cfg)?;
        eprintln!("removed `{}` from this repo's team", args.label);
    }
    Ok(())
}

/// Expand a `TeamField` into its entry list. A `Single(name)`
/// string is sugar for a one-element `[Include(name)]` array.
fn team_field_into_entries(field: TeamField) -> Vec<TeamEntry> {
    match field {
        TeamField::Single(name) => {
            vec![TeamEntry::Include(crate::cli::teams_config::IncludeEntry {
                include: name,
            })]
        }
        TeamField::Array(v) => v,
    }
}

/// True if any entry in the array references `label` (by-name,
/// inline, or bare string — Include entries name a team, not an
/// agent, so they never match).
fn team_entries_contain(entries: &[TeamEntry], label: &AgentLabel) -> bool {
    entries.iter().any(|e| team_entry_matches(e, label))
}

fn team_entry_matches(entry: &TeamEntry, label: &AgentLabel) -> bool {
    match entry {
        TeamEntry::Include(_) => false,
        TeamEntry::ByName(b) => &b.agent == label,
        TeamEntry::Inline(i) => &i.label == label,
        TeamEntry::BareString(l) => l == label,
    }
}

/// `clank agent promote <label>` — repo-scope master designation
/// via the `promoted` field. Team-level master changes go through
/// `clank team set-master`.
fn promote(args: AgentPromoteArgs) -> anyhow::Result<()> {
    let label = AgentLabel::parse(&args.label)
        .map_err(|e| anyhow::anyhow!("invalid agent label `{}`: {e}", args.label))?;
    let repo = resolve_repo(args.repo.as_deref())?;
    if !write_repo_promoted_field_if_team_set(&repo, &label)? {
        anyhow::bail!("this repo has no team configured. Run `clank init --team <name>` first.");
    }
    Ok(())
}

/// Plan: teams-based-agent-registration (phase 6c).
///
/// When the repo's `.clank/config.json` has a new-schema
/// `team` field set, write the `promoted: <label>` field
/// instead of mutating per-agent skeletons. Validates that
/// the label is reachable in the registered set (so a
/// nonexistent label errors before writing).
///
/// Returns:
/// - `Ok(true)` when the new-schema path handled the
///   promote. Caller should return early.
/// - `Ok(false)` when the repo is unmigrated (no team
///   field) — caller continues to the legacy skeleton
///   path.
/// - `Err(_)` when reading/parsing the config files fails
///   or when the label isn't a valid promote target
///   (UnknownAgent, NoMaster, etc. from the resolver).
fn write_repo_promoted_field_if_team_set(
    repo: &Path,
    promoted: &AgentLabel,
) -> anyhow::Result<bool> {
    use crate::cli::teams_config::{RepoConfigFile, UserConfigFile};
    use anyhow::Context;
    use std::io::Write;

    let repo_cfg_path = repo.join(".clank/config.json");
    let body = match std::fs::read_to_string(&repo_cfg_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    let mut repo_cfg: RepoConfigFile = serde_json::from_str(&body).map_err(|e| {
        anyhow::anyhow!(
            "parsing {} as new-schema RepoConfigFile: {e}",
            repo_cfg_path.display()
        )
    })?;
    if repo_cfg.team.is_none() {
        return Ok(false);
    }

    // Validate via the resolver. Load user-scope, set
    // `promoted` in a TEMPORARY clone of repo_cfg, run the
    // resolver — if the label isn't reachable it errors
    // (UnknownAgent / PromotedNotPresent). Only persist on
    // success.
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let user_cfg: UserConfigFile = match home.as_deref() {
        Some(h) => {
            let user_path = h.join(".clank/config.json");
            match std::fs::read_to_string(&user_path) {
                Ok(s) => serde_json::from_str(&s).with_context(|| {
                    format!(
                        "parsing {} as new-schema UserConfigFile",
                        user_path.display()
                    )
                })?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => UserConfigFile::default(),
                Err(e) => {
                    return Err(
                        anyhow::Error::from(e).context(format!("reading {}", user_path.display()))
                    );
                }
            }
        }
        None => UserConfigFile::default(),
    };

    // Validate FIRST by running the resolver with the proposed
    // promoted field, including the no-op case (codex 63b25a1
    // observation: a stale existing `promoted` pointing at an
    // agent that no longer exists in the registered set should
    // surface, not silently succeed).
    let mut probe = repo_cfg.clone();
    probe.promoted = Some(promoted.clone());
    let _resolved = crate::cli::teams_config::resolve_registered_set(&user_cfg, &probe)
        .with_context(|| format!("validating `promote {}`", promoted.as_str()))?;

    // No-op short-circuit AFTER validation: promoted already
    // equals this label AND the registered set is still valid.
    if repo_cfg.promoted.as_ref() == Some(promoted) {
        eprintln!(
            "note: `{}` is already the promoted master in repo-scope",
            promoted.as_str()
        );
        return Ok(true);
    }

    // Persist.
    repo_cfg.promoted = Some(promoted.clone());
    let parent = repo_cfg_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("no parent for `{}`", repo_cfg_path.display()))?;
    std::fs::create_dir_all(parent)?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".clank-config-")
        .suffix(".json.tmp")
        .tempfile_in(parent)?;
    tmp.write_all(serde_json::to_string_pretty(&repo_cfg)?.as_bytes())?;
    tmp.write_all(b"\n")?;
    tmp.as_file_mut().sync_all()?;
    tmp.persist(&repo_cfg_path).map_err(|e| e.error)?;
    eprintln!(
        "promoted `{}` to master in repo-scope (new-schema `promoted` field)",
        promoted.as_str()
    );
    Ok(true)
}

fn parse_env_overrides(raw: &[String]) -> anyhow::Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for s in raw {
        let (k, v) = s
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("--launch-env value `{s}` must be `KEY=VAL`"))?;
        if k.is_empty() {
            anyhow::bail!("--launch-env key cannot be empty: `{s}`");
        }
        out.insert(k.to_string(), v.to_string());
    }
    Ok(out)
}

/// Construct a `LaunchConfig` from CLI args. Returns `None` when
/// no launch-related flag was passed (Phase 6 default: skeleton
/// declaration's `launch` field is absent rather than `Some({})`).
fn build_launch(
    cmd: Option<String>,
    args: Vec<String>,
    env: BTreeMap<String, String>,
) -> Option<LaunchConfig> {
    if cmd.is_none() && args.is_empty() && env.is_empty() {
        return None;
    }
    Some(LaunchConfig {
        command: cmd,
        args,
        env,
    })
}

/// Read `~/.clank/config.json` as a typed [`UserConfigFile`].
/// Missing file → default; malformed JSON → error.
fn read_user_config(home: &Path) -> anyhow::Result<UserConfigFile> {
    let path = home.join(".clank/config.json");
    match std::fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s).with_context(|| format!("parsing {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(UserConfigFile::default()),
        Err(e) => Err(anyhow::Error::from(e).context(format!("reading {}", path.display()))),
    }
}

/// Atomic write of `~/.clank/config.json`. Round-trip preserves
/// `review`, `hooks`, and any unknown fields (via the `extra`
/// flatten catchall on [`UserConfigFile`]).
fn write_user_config(home: &Path, file: &UserConfigFile) -> anyhow::Result<()> {
    write_typed_config(&home.join(".clank/config.json"), file)
}

/// Read `<repo>/.clank/config.json` as the typed
/// [`RepoConfigFile`]. Missing file → default; malformed JSON →
/// error.
fn read_repo_config(repo: &Path) -> anyhow::Result<RepoConfigFile> {
    let path = repo.join(".clank/config.json");
    match std::fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s).with_context(|| format!("parsing {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(RepoConfigFile::default()),
        Err(e) => Err(anyhow::Error::from(e).context(format!("reading {}", path.display()))),
    }
}

/// Atomic write of `<repo>/.clank/config.json`. The `extra`
/// flatten catchall preserves unknown sections on round-trip.
fn write_repo_config(repo: &Path, file: &RepoConfigFile) -> anyhow::Result<()> {
    write_typed_config(&repo.join(".clank/config.json"), file)
}

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

#[cfg(test)]
mod tests {
    use super::*;

    fn label(s: &str) -> AgentLabel {
        AgentLabel::parse(s).unwrap()
    }

    fn make_session() -> Session {
        Session {
            id: clank_core::ids::SessionId::parse("11111111-1111-1111-1111-111111111111").unwrap(),
            tool: Tool::Claude,
            updated_at: "2026-06-04T12:00:00Z".to_string(),
        }
    }

    // ── compose_launch initial-prompt tests ────────────────────

    fn claude_session() -> Session {
        Session {
            id: clank_core::ids::SessionId::parse("aaaaaaaa-1111-2222-3333-444444444444").unwrap(),
            tool: Tool::Claude,
            updated_at: "2026-06-04T12:00:00Z".to_string(),
        }
    }

    fn codex_session() -> Session {
        Session {
            id: clank_core::ids::SessionId::parse("bbbbbbbb-1111-2222-3333-444444444444").unwrap(),
            tool: Tool::Codex,
            updated_at: "2026-06-04T12:00:00Z".to_string(),
        }
    }

    #[test]
    fn write_repo_promoted_field_when_team_set() {
        // Plan: teams-based-agent-registration (phase 6c).
        // When the repo has a `team` field, `clank promote`
        // writes `promoted: <label>` to the repo config
        // instead of mutating per-agent skeletons.
        use crate::cli::teams_config::{
            AgentDescription, RepoConfigFile, TeamComposition, UserConfigFile,
        };
        let user_home = tempfile::tempdir().unwrap();
        // Seed user-scope: claude as master of `dev`, codex as
        // commit reviewer.
        let mut user_cfg = UserConfigFile::default();
        user_cfg.agents.insert(
            AgentLabel::parse("claude").unwrap(),
            AgentDescription {
                tool: Tool::Claude,
                launch: None,
                initial_prompt: None,
            },
        );
        user_cfg.agents.insert(
            AgentLabel::parse("codex").unwrap(),
            AgentDescription {
                tool: Tool::Codex,
                launch: None,
                initial_prompt: None,
            },
        );
        user_cfg.teams.insert(
            "dev".to_string(),
            TeamComposition {
                master: Some(AgentLabel::parse("claude").unwrap()),
                commit_reviewers: vec![AgentLabel::parse("codex").unwrap()],
                gate_reviewers: vec![],
            },
        );
        std::fs::create_dir_all(user_home.path().join(".clank")).unwrap();
        std::fs::write(
            user_home.path().join(".clank/config.json"),
            serde_json::to_string_pretty(&user_cfg).unwrap(),
        )
        .unwrap();
        // Seed repo with `team: dev` and no promoted.
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join(".clank")).unwrap();
        std::fs::write(repo.path().join(".clank/config.json"), r#"{"team":"dev"}"#).unwrap();

        // Run promote with $HOME set to our temp dir. Use a
        // mutex to serialize against other tests that touch
        // $HOME — phase-6a tests dropped this pattern after
        // lifting `home` to a param, but `promote` flows
        // through the production agent CLI dispatch which
        // reads $HOME directly. Acceptable trade-off for this
        // test because the alternative (lifting home all the
        // way through `promote`) cascades into the CLI surface.
        let lock = home_test_lock().lock().unwrap();
        let prev = std::env::var_os("HOME");
        // SAFETY: serialized by `lock`.
        unsafe {
            std::env::set_var("HOME", user_home.path());
        }

        let codex_label = AgentLabel::parse("codex").unwrap();
        let handled = write_repo_promoted_field_if_team_set(repo.path(), &codex_label).unwrap();

        unsafe {
            match prev {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
        drop(lock);

        assert!(handled, "team field set → new-schema path should handle");
        let body = std::fs::read_to_string(repo.path().join(".clank/config.json")).unwrap();
        let parsed: RepoConfigFile = serde_json::from_str(&body).unwrap();
        assert_eq!(
            parsed.promoted.as_ref().map(|l| l.as_str()),
            Some("codex"),
            "promoted field should be `codex`"
        );
    }

    #[test]
    fn write_repo_promoted_field_returns_false_when_no_team_set() {
        // Repo has no `team` field → caller should continue to
        // the legacy skeleton path.
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join(".clank")).unwrap();
        std::fs::write(repo.path().join(".clank/config.json"), "{}").unwrap();
        let lock = home_test_lock().lock().unwrap();
        let prev = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", repo.path()); // not seeded; irrelevant
        }
        let label = AgentLabel::parse("codex").unwrap();
        let handled = write_repo_promoted_field_if_team_set(repo.path(), &label).unwrap();
        unsafe {
            match prev {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
        drop(lock);
        assert!(
            !handled,
            "no team → legacy path should run (handler returns false)"
        );
    }

    #[test]
    fn write_repo_promoted_field_returns_false_when_no_repo_config() {
        let repo = tempfile::tempdir().unwrap();
        let label = AgentLabel::parse("codex").unwrap();
        let handled = write_repo_promoted_field_if_team_set(repo.path(), &label).unwrap();
        assert!(!handled);
    }

    #[test]
    fn write_repo_promoted_field_rejects_nonexistent_label_before_persisting() {
        // Ruthless 63b25a1 pin: probe-resolve-before-persist
        // is the load-bearing safety property. A label that
        // isn't reachable in the resolved registered set must
        // error BEFORE the repo config is mutated.
        use crate::cli::teams_config::{
            AgentDescription, RepoConfigFile, TeamComposition, UserConfigFile,
        };
        let user_home = tempfile::tempdir().unwrap();
        let mut user_cfg = UserConfigFile::default();
        user_cfg.agents.insert(
            AgentLabel::parse("claude").unwrap(),
            AgentDescription {
                tool: Tool::Claude,
                launch: None,
                initial_prompt: None,
            },
        );
        user_cfg.teams.insert(
            "dev".to_string(),
            TeamComposition {
                master: Some(AgentLabel::parse("claude").unwrap()),
                commit_reviewers: vec![],
                gate_reviewers: vec![],
            },
        );
        std::fs::create_dir_all(user_home.path().join(".clank")).unwrap();
        std::fs::write(
            user_home.path().join(".clank/config.json"),
            serde_json::to_string_pretty(&user_cfg).unwrap(),
        )
        .unwrap();
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join(".clank")).unwrap();
        std::fs::write(repo.path().join(".clank/config.json"), r#"{"team":"dev"}"#).unwrap();

        let lock = home_test_lock().lock().unwrap();
        let prev = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", user_home.path());
        }
        let phantom = AgentLabel::parse("phantom").unwrap();
        let err = write_repo_promoted_field_if_team_set(repo.path(), &phantom).unwrap_err();
        unsafe {
            match prev {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
        drop(lock);

        let msg = format!("{err:#}");
        // Resolver error names the validation context.
        assert!(
            msg.contains("validating")
                || msg.contains("phantom")
                || msg.contains("UnknownAgent")
                || msg.contains("PromotedNotPresent"),
            "expected resolver-rejection error; got: {msg}"
        );
        // Repo config was NOT mutated.
        let body = std::fs::read_to_string(repo.path().join(".clank/config.json")).unwrap();
        let parsed: RepoConfigFile = serde_json::from_str(&body).unwrap();
        assert!(
            parsed.promoted.is_none(),
            "promoted field should not have been written on validation failure; got {:?}",
            parsed.promoted
        );
    }

    #[test]
    fn write_repo_promoted_field_no_op_when_already_promoted() {
        // Ruthless 63b25a1 pin: the no-op short-circuit hits
        // when the requested label already equals the existing
        // `promoted` field. After codex 63b25a1 hardening,
        // validation runs FIRST — so a stale `promoted` would
        // surface, not silently succeed.
        use crate::cli::teams_config::{
            AgentDescription, RepoConfigFile, TeamComposition, UserConfigFile,
        };
        let user_home = tempfile::tempdir().unwrap();
        let mut user_cfg = UserConfigFile::default();
        user_cfg.agents.insert(
            AgentLabel::parse("claude").unwrap(),
            AgentDescription {
                tool: Tool::Claude,
                launch: None,
                initial_prompt: None,
            },
        );
        user_cfg.agents.insert(
            AgentLabel::parse("codex").unwrap(),
            AgentDescription {
                tool: Tool::Codex,
                launch: None,
                initial_prompt: None,
            },
        );
        user_cfg.teams.insert(
            "dev".to_string(),
            TeamComposition {
                master: Some(AgentLabel::parse("claude").unwrap()),
                commit_reviewers: vec![AgentLabel::parse("codex").unwrap()],
                gate_reviewers: vec![],
            },
        );
        std::fs::create_dir_all(user_home.path().join(".clank")).unwrap();
        std::fs::write(
            user_home.path().join(".clank/config.json"),
            serde_json::to_string_pretty(&user_cfg).unwrap(),
        )
        .unwrap();
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join(".clank")).unwrap();
        // Seed repo config with promoted already set.
        std::fs::write(
            repo.path().join(".clank/config.json"),
            r#"{"team":"dev","promoted":"codex"}"#,
        )
        .unwrap();

        let lock = home_test_lock().lock().unwrap();
        let prev = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", user_home.path());
        }
        let codex = AgentLabel::parse("codex").unwrap();
        let handled = write_repo_promoted_field_if_team_set(repo.path(), &codex).unwrap();
        unsafe {
            match prev {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
        drop(lock);

        assert!(
            handled,
            "no-op short-circuit should still return true (handled)"
        );
        // Config unchanged — still codex as promoted.
        let body = std::fs::read_to_string(repo.path().join(".clank/config.json")).unwrap();
        let parsed: RepoConfigFile = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed.promoted.as_ref().map(|l| l.as_str()), Some("codex"));
    }

    /// Tests that mutate `$HOME` MUST serialize against each
    /// other — `std::env` is process-global and cargo runs
    /// tests in parallel by default.
    fn home_test_lock() -> &'static std::sync::Mutex<()> {
        use std::sync::OnceLock;
        static LOCK: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    #[test]
    fn compose_launch_appends_initial_prompt_when_set() {
        let s = claude_session();
        let c = compose_launch(Path::new("/repo"), &s, None, Some("custom prompt"));
        assert_eq!(
            c.args.last().map(|s| s.as_str()),
            Some("custom prompt"),
            "trailing arg should be the prompt; got: {:?}",
            c.args
        );
    }

    #[test]
    fn compose_launch_omits_prompt_when_none() {
        let s = claude_session();
        let c = compose_launch(Path::new("/repo"), &s, None, None);
        // Current behavior preserved exactly: trailing arg is the session id, no prompt.
        assert_eq!(
            c.args.last().map(|s| s.as_str()),
            Some("aaaaaaaa-1111-2222-3333-444444444444"),
            "no trailing prompt should be appended; got: {:?}",
            c.args
        );
    }

    #[test]
    fn compose_launch_codex_prompt_is_final_positional_after_cd() {
        // For codex, --cd <repo> comes from session_restore_args
        // BEFORE the prompt. The prompt is the final positional.
        let s = codex_session();
        let c = compose_launch(Path::new("/repo"), &s, None, Some("ack"));
        // Expected: ["resume", "<id>", "--cd", "/repo", "ack"]
        assert_eq!(c.args.last().map(|s| s.as_str()), Some("ack"));
        // --cd <repo> appears before the prompt.
        let cd_pos = c.args.iter().position(|s| s == "--cd").unwrap();
        let prompt_pos = c.args.iter().position(|s| s == "ack").unwrap();
        assert!(
            cd_pos < prompt_pos,
            "--cd must come before prompt; argv: {:?}",
            c.args
        );
    }

    // ── resolve_initial_prompt policy tests ────────────────────

    #[test]
    fn resolve_initial_prompt_uses_declaration_field_when_set() {
        let out = resolve_initial_prompt(Some("foo"), AutoMode::Off);
        assert_eq!(out, Some("foo".to_string()));
    }

    #[test]
    fn resolve_initial_prompt_uses_default_when_auto_on_and_declaration_unset() {
        // Pinned: exact equality against the constant so a future
        // tweak to DEFAULT_AUTO_PROMPT fails the test deliberately.
        let out = resolve_initial_prompt(None, AutoMode::On);
        assert_eq!(out, Some("Session resumed.".to_string()));
    }

    #[test]
    fn resolve_initial_prompt_returns_none_when_auto_off_and_declaration_unset() {
        let out = resolve_initial_prompt(None, AutoMode::Off);
        assert_eq!(out, None);
    }

    #[test]
    fn resolve_initial_prompt_declaration_wins_over_auto_default() {
        let out = resolve_initial_prompt(Some("custom"), AutoMode::On);
        assert_eq!(out, Some("custom".to_string()));
    }

    #[test]
    fn resolve_initial_prompt_empty_declaration_string_disables_prompt() {
        // Ruthless 0fe1567 pin: Some("") is the explicit-disable
        // escape hatch. Without this, the only way to opt out of
        // the auto_mode default would be to disable auto_mode
        // itself — coupling two unrelated concerns.
        let out = resolve_initial_prompt(Some(""), AutoMode::On);
        assert_eq!(out, None);
    }

    // ── compose_bootstrap_launch policy tests ─────────────────
    // Plan: agent-start-bootstraps-missing-skeleton.

    fn desc_with(tool: Tool, launch: Option<LaunchConfig>) -> AgentDescription {
        AgentDescription {
            tool,
            launch,
            initial_prompt: None,
        }
    }

    #[test]
    fn bootstrap_uses_tool_from_description() {
        let desc = desc_with(
            Tool::Claude,
            Some(LaunchConfig {
                command: None,
                args: vec!["--skill".into(), "ruthless".into()],
                env: Default::default(),
            }),
        );
        let composed = compose_bootstrap_launch(&label("phantom"), &desc).expect("compose");
        assert_eq!(composed.program, "claude");
        assert_eq!(
            composed.args,
            vec![
                "--skill".to_string(),
                "ruthless".to_string(),
                "Run `clank as phantom` to bind this session.".to_string(),
            ]
        );
    }

    #[test]
    fn bootstrap_prefers_launch_command_over_tool() {
        let desc = desc_with(
            Tool::Claude,
            Some(LaunchConfig {
                command: Some("my-claude-wrapper".into()),
                args: vec![],
                env: Default::default(),
            }),
        );
        let composed = compose_bootstrap_launch(&label("phantom"), &desc).expect("compose");
        // launch.command wins; tool=claude is only the fallback.
        assert_eq!(composed.program, "my-claude-wrapper");
    }

    #[test]
    fn bootstrap_falls_back_to_tool_when_no_launch_command() {
        // No launch profile at all → program is the tool name.
        let desc = desc_with(Tool::Codex, None);
        let composed = compose_bootstrap_launch(&label("phantom"), &desc).expect("compose");
        assert_eq!(composed.program, "codex");
        assert_eq!(
            composed.args,
            vec!["Run `clank as phantom` to bind this session.".to_string()]
        );
    }

    #[test]
    fn bootstrap_bind_prompt_is_pinned_verbatim() {
        // Pinned: exact equality so a future wording tweak fails
        // the test deliberately.
        let s = bootstrap_bind_prompt(&label("phantom"));
        assert_eq!(s, "Run `clank as phantom` to bind this session.");
    }

    #[test]
    fn resolve_initial_prompt_empty_declaration_string_disables_prompt_under_auto_off() {
        // Declaration is authoritative regardless of auto_mode.
        let out = resolve_initial_prompt(Some(""), AutoMode::Off);
        assert_eq!(out, None);
    }

    #[test]
    fn agent_row_bound_uses_session_tool() {
        let session = make_session();
        let row = AgentRow::from_join(
            &label("alice"),
            "reviewer",
            "commit",
            Tool::Codex, // description tool (overridden by session when bound)
            Some(&session),
        );
        assert_eq!(row.label, "alice");
        assert_eq!(row.role, "reviewer");
        assert_eq!(row.review, "commit");
        assert!(row.bound);
        assert_eq!(row.tool.as_deref(), Some("claude")); // from session
        assert!(row.session_id.is_some());
    }

    #[test]
    fn agent_row_unbound_uses_description_tool() {
        let row = AgentRow::from_join(&label("alice"), "reviewer", "gate", Tool::Claude, None);
        assert!(!row.bound);
        assert_eq!(row.review, "gate");
        assert_eq!(row.tool.as_deref(), Some("claude"));
        assert!(row.session_id.is_none());
    }

    #[test]
    fn agent_row_master_has_no_review_tier() {
        let row = AgentRow::from_join(&label("boss"), "master", "—", Tool::Claude, None);
        assert_eq!(row.role, "master");
        assert_eq!(row.review, "—");
    }
}
