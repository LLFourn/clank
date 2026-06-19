//! `clank agent` — enumerate / launch / declare agents under the
//! team-based registration model (`teams-based-agent-registration`).
//!
//! Source of truth for "who is registered in this repo" is the
//! TEAM RESOLVER (`agent_store::try_resolve_via_team`): the
//! repo's own `agents` map + its single `team` composition (the
//! repo config is self-contained).
//! The per-agent skeleton at `.clank/agents/<label>/config.json`
//! holds ONLY per-machine state (`auto_mode`, `wfw_timeout`,
//! `session`) — no role / tool / launch declaration. `list`
//! joins the resolved set with each agent's skeleton session.

use std::path::Path;

use anyhow::Context;
use serde::Serialize;

use crate::agent_store::load_agent_config;
use crate::cli::teams_config::{AgentDescription, RepoConfigFile, UserConfigFile};
use clank_core::agent_config::{LaunchConfig, Session};
use clank_core::ids::AgentLabel;
use clank_core::vocab::{AutoMode, Tool};
use std::collections::BTreeMap;

use super::{
    AgentAddArgs, AgentArgs, AgentCmd, AgentListArgs, AgentRemoveArgs, AgentStartArgs, resolve_repo,
};

pub async fn run(args: AgentArgs) -> anyhow::Result<()> {
    match args.command {
        AgentCmd::List(a) => list(a),
        AgentCmd::Start(a) => start(a),
        AgentCmd::Add(a) => add(a),
        AgentCmd::Remove(a) => remove(a),
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
        // A seeded fork spec (clank fork) takes precedence over the
        // plain bootstrap on first launch: fork the source session
        // with the orientation prompt; the forked id then binds via
        // the env-var hook and later starts hit the resume path. The
        // spec is ONE-SHOT — consume (delete) it on a real launch so
        // a later relaunch with a lost binding can't re-fork the
        // ancestor (fork-session-id-chaining). `--print` only peeks.
        None => {
            let spec = if args.print {
                crate::cli::fork::load_fork_spec(&repo, &label)?
            } else {
                crate::cli::fork::take_fork_spec(&repo, &label)?
            };
            match spec {
                Some(spec) => compose_fork_launch(&spec, &desc, &repo),
                None => compose_bootstrap_launch(&label, &desc)?,
            }
        }
        Some(session) => {
            let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
            let auto_mode =
                crate::cli::team::resolve_effective_auto_mode(cfg.as_ref(), home.as_deref());
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

/// Launch a FORKED copy of a source session (clank fork): both
/// tools fork cleanly — `claude --resume <src> --fork-session`
/// mints a diverged session; `codex fork -C <worktree> <src>
/// [prompt]` likewise. `worktree` is this agent's resolved repo (the
/// fork dest). codex's `-C/--cd` is REQUIRED: forking a session
/// whose recorded cwd differs from the launch cwd otherwise makes
/// codex interactively prompt "Choose working directory…" every time
/// (fork-codex-cd-flag); naming the worktree explicitly skips that
/// picker. claude takes the cwd from the pane and doesn't prompt.
/// The orientation prompt rides as the trailing positional on both.
fn compose_fork_launch(
    spec: &crate::cli::fork::ForkSpec,
    desc: &AgentDescription,
    worktree: &Path,
) -> ComposedLaunch {
    let program = desc
        .launch
        .as_ref()
        .and_then(|l| l.command.clone())
        .unwrap_or_else(|| spec.tool.as_str().to_string());
    let mut args = desc
        .launch
        .as_ref()
        .map(|l| l.args.clone())
        .unwrap_or_default();
    match spec.tool {
        Tool::Claude => {
            args.push("--resume".into());
            args.push(spec.from_session.clone());
            args.push("--fork-session".into());
        }
        Tool::Codex => {
            args.push("fork".into());
            args.push("-C".into());
            args.push(worktree.display().to_string());
            args.push(spec.from_session.clone());
        }
    }
    args.push(spec.prompt.clone());
    let env_overrides = desc
        .launch
        .as_ref()
        .map(|l| l.env.clone())
        .unwrap_or_default();
    ComposedLaunch {
        program,
        args,
        env_overrides,
    }
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
/// - `declaration`: `AgentDescription.initial_prompt` from the
///   resolved registered set. `Some("")` means "explicitly
///   disable" — falls back to None, NOT Some("") through. Lets a
///   user with auto_mode=On opt out of the prompt without
///   disabling auto_mode itself.
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

// ── clank agent add / remove (definition registry) ───────────
//
// `clank agent` is the agent DEFINITION registry. `--global`
// edits user-scope `~/.clank/config.json#/agents`; repo-scope
// (default) edits THIS repo's own `agents` map. NEITHER touches
// the team — composing the operating team is `clank team`'s job.
// Per-agent skeletons hold only state (session / auto_mode /
// wfw_timeout) — never declaration.

/// `clank agent add <label> --tool <...> [...]` — thin shell:
/// resolve env, build the launch profile, dispatch to a `pub`
/// core. `--tool` is required (a definition needs a tool).
fn add(args: AgentAddArgs) -> anyhow::Result<()> {
    let label = AgentLabel::parse(&args.label)
        .map_err(|e| anyhow::anyhow!("invalid agent label `{}`: {e}", args.label))?;
    let env = parse_env_overrides(&args.launch_envs)?;
    let launch = build_launch(args.launch_cmd, args.launch_args, env);
    let desc = AgentDescription {
        tool: args.tool.into(),
        launch,
        initial_prompt: args.initial_prompt.clone(),
    };

    if args.global {
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        let home_ref = home
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("--global requires $HOME"))?;
        declare_global_agent(home_ref, &label, desc)?;
    } else {
        let repo = resolve_repo(args.repo.as_deref())?;
        define_repo_agent(&repo, &label, desc)?;
    }
    Ok(())
}

// ── agent mutation cores ─────────────────────────────────────
//
// `pub`, env-free (explicit `home`/`repo`), args-free. Both
// `run()` and integration-test setup call these. Plan:
// dogfood-init-setup-in-tests (Phase A).

/// Declare an agent DESCRIPTION in user-scope `agents`. Errors if
/// the label already exists there.
pub fn declare_global_agent(
    home: &Path,
    label: &AgentLabel,
    desc: AgentDescription,
) -> anyhow::Result<()> {
    let mut file = read_user_config(home)?;
    if file.agents.contains_key(label) {
        anyhow::bail!(
            "agent `{}` already declared in user-scope `agents`",
            label.as_str()
        );
    }
    file.agents.insert(label.clone(), desc);
    write_user_config(home, &file)?;
    eprintln!("declared `{}` in user-scope `agents`", label.as_str());
    Ok(())
}

/// Write an agent DEFINITION into THIS repo's `agents` map.
/// Does NOT touch the team — composing the operating team is
/// `clank team`'s job. Errors if the label is already defined in
/// the repo (a definition is never silently overwritten).
pub fn define_repo_agent(
    repo: &Path,
    label: &AgentLabel,
    desc: AgentDescription,
) -> anyhow::Result<()> {
    let mut repo_cfg = read_repo_config(repo)?;
    if repo_cfg.agents.contains_key(label) {
        anyhow::bail!(
            "agent `{label}` is already defined in this repo's `agents`. \
             Remove it with `clank agent remove {label}` first, or pick a different name.",
            label = label.as_str()
        );
    }
    repo_cfg.agents.insert(label.clone(), desc);
    write_repo_config(repo, &repo_cfg)?;
    eprintln!("defined `{}` in this repo's `agents`", label.as_str());
    Ok(())
}

/// `clank agent remove <label> [--global]` — thin shell.
fn remove(args: AgentRemoveArgs) -> anyhow::Result<()> {
    let label = AgentLabel::parse(&args.label)
        .map_err(|e| anyhow::anyhow!("invalid agent label `{}`: {e}", args.label))?;

    if args.global {
        // User-scope op: needs only $HOME, never repo discovery (mirror
        // `agent add --global`) — codex ec37092.
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        let home_ref = home
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("--global requires $HOME"))?;
        remove_global_agent(home_ref, &label)?;
    } else {
        let repo = resolve_repo(args.repo.as_deref())?;
        remove_repo_agent(&repo, &label)?;
    }
    Ok(())
}

/// Remove an agent DESCRIPTION from user-scope `agents` and scrub
/// it from every team composition (master + both reviewer tiers)
/// so no dangling reference survives.
pub fn remove_global_agent(home: &Path, label: &AgentLabel) -> anyhow::Result<()> {
    let mut file = read_user_config(home)?;
    if file.agents.remove(label).is_none() {
        anyhow::bail!("agent `{}` not in user-scope `agents`", label.as_str());
    }
    let mut touched = Vec::new();
    for (name, team) in file.teams.iter_mut() {
        let before = team.commit_reviewers.len()
            + team.gate_reviewers.len()
            + usize::from(team.master.is_some());
        if team.master.as_ref() == Some(label) {
            team.master = None;
        }
        team.commit_reviewers.retain(|l| l != label);
        team.gate_reviewers.retain(|l| l != label);
        let after = team.commit_reviewers.len()
            + team.gate_reviewers.len()
            + usize::from(team.master.is_some());
        if before != after {
            touched.push(name.clone());
        }
    }
    write_user_config(home, &file)?;
    if touched.is_empty() {
        eprintln!("removed `{}` from user-scope `agents`", label.as_str());
    } else {
        eprintln!(
            "removed `{}` from user-scope `agents` (and from teams: {})",
            label.as_str(),
            touched.join(", ")
        );
    }
    Ok(())
}

/// Remove an agent DEFINITION from THIS repo's `agents` map.
/// Refuses while the label is still referenced by the repo team
/// (master or either reviewer list) — the team is the operating
/// roster, so the user must `clank team remove <label>` first.
/// Leaves the team untouched.
pub fn remove_repo_agent(repo: &Path, label: &AgentLabel) -> anyhow::Result<()> {
    let mut repo_cfg = read_repo_config(repo)?;
    let in_team = repo_cfg.team.master.as_ref() == Some(label)
        || repo_cfg.team.commit_reviewers.contains(label)
        || repo_cfg.team.gate_reviewers.contains(label);
    if in_team {
        anyhow::bail!(
            "`{label}` is in this repo's team; run `clank team remove {label}` first",
            label = label.as_str()
        );
    }
    if repo_cfg.agents.remove(label).is_none() {
        anyhow::bail!(
            "agent `{}` is not defined in this repo's `agents`",
            label.as_str()
        );
    }
    write_repo_config(repo, &repo_cfg)?;
    eprintln!("removed `{}` from this repo's `agents`", label.as_str());
    Ok(())
}

/// Set THIS repo's team master to `promoted`. The agent must
/// already be defined in `repo.agents` (the repo is
/// self-contained). The previous master, if different, is
/// demoted into `commit_reviewers`. Validated via the resolver
/// before persisting, so an unknown/invalid target errors before
/// the config is mutated.
///
/// Used by `clank team set-master` (the CLI entry moved there
/// from the removed `clank agent promote`).
///
/// `home` is unused (kept for signature stability with the
/// previous user-scope-aware version).
///
/// Returns:
/// - `Ok(true)` when written (or a no-op).
/// - `Ok(false)` when the repo has no config file at all (the
///   caller then errors with the no-config message).
/// - `Err(_)` on parse failure or an invalid promote target.
pub fn promote_repo_master(
    repo: &Path,
    _home: Option<&Path>,
    promoted: &AgentLabel,
) -> anyhow::Result<bool> {
    use crate::cli::teams_config::RepoConfigFile;
    use anyhow::Context;

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

    // No-op short-circuit: already the master.
    if repo_cfg.team.master.as_ref() == Some(promoted) {
        eprintln!(
            "note: `{}` is already this repo's master",
            promoted.as_str()
        );
        return Ok(true);
    }

    // Build the proposed team in a clone and validate via the
    // resolver before persisting. Demote the previous master (if
    // any, and different) into commit_reviewers; promote the
    // target out of whatever reviewer list it was in.
    let mut probe = repo_cfg.clone();
    if let Some(prev) = probe.team.master.take()
        && &prev != promoted
        && !probe.team.commit_reviewers.contains(&prev)
    {
        probe.team.commit_reviewers.push(prev);
    }
    probe.team.commit_reviewers.retain(|l| l != promoted);
    probe.team.gate_reviewers.retain(|l| l != promoted);
    probe.team.master = Some(promoted.clone());

    crate::cli::teams_config::resolve_registered_set(&probe)
        .with_context(|| format!("validating `promote {}`", promoted.as_str()))?;

    repo_cfg = probe;
    write_repo_config(repo, &repo_cfg)?;
    eprintln!("promoted `{}` to this repo's master", promoted.as_str());
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

    fn seed_repo_config(repo: &Path, body: &str) {
        std::fs::create_dir_all(repo.join(".clank")).unwrap();
        std::fs::write(repo.join(".clank/config.json"), body).unwrap();
    }

    #[test]
    fn promote_sets_team_master_and_demotes_previous() {
        // `clank promote codex` sets team.master = codex and
        // demotes the previous master (claude) into
        // commit_reviewers.
        use crate::cli::teams_config::RepoConfigFile;
        let repo = tempfile::tempdir().unwrap();
        seed_repo_config(
            repo.path(),
            r#"{
                "agents": {
                    "claude": { "tool": "claude" },
                    "codex": { "tool": "codex" }
                },
                "team": { "master": "claude", "commit_reviewers": ["codex"] }
            }"#,
        );

        let codex_label = AgentLabel::parse("codex").unwrap();
        let handled = promote_repo_master(repo.path(), None, &codex_label).unwrap();

        assert!(handled, "config exists → handled");
        let body = std::fs::read_to_string(repo.path().join(".clank/config.json")).unwrap();
        let parsed: RepoConfigFile = serde_json::from_str(&body).unwrap();
        assert_eq!(
            parsed.team.master.as_ref().map(|l| l.as_str()),
            Some("codex")
        );
        // claude (prev master) demoted to commit_reviewers; codex
        // removed from there.
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
    fn promote_returns_false_when_no_repo_config() {
        let repo = tempfile::tempdir().unwrap();
        let label = AgentLabel::parse("codex").unwrap();
        let handled = promote_repo_master(repo.path(), None, &label).unwrap();
        assert!(!handled);
    }

    #[test]
    fn promote_rejects_nonexistent_label_before_persisting() {
        // Probe-resolve-before-persist: a label not defined in
        // `agents` must error BEFORE the config is mutated.
        use crate::cli::teams_config::RepoConfigFile;
        let repo = tempfile::tempdir().unwrap();
        seed_repo_config(
            repo.path(),
            r#"{
                "agents": { "claude": { "tool": "claude" } },
                "team": { "master": "claude" }
            }"#,
        );

        let phantom = AgentLabel::parse("phantom").unwrap();
        let err = promote_repo_master(repo.path(), None, &phantom).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("validating") || msg.contains("phantom") || msg.contains("not defined"),
            "expected resolver-rejection error; got: {msg}"
        );
        // Repo config was NOT mutated — master still claude.
        let body = std::fs::read_to_string(repo.path().join(".clank/config.json")).unwrap();
        let parsed: RepoConfigFile = serde_json::from_str(&body).unwrap();
        assert_eq!(
            parsed.team.master.as_ref().map(|l| l.as_str()),
            Some("claude")
        );
    }

    #[test]
    fn promote_no_op_when_already_master() {
        use crate::cli::teams_config::RepoConfigFile;
        let repo = tempfile::tempdir().unwrap();
        seed_repo_config(
            repo.path(),
            r#"{
                "agents": { "claude": { "tool": "claude" } },
                "team": { "master": "claude" }
            }"#,
        );

        let claude = AgentLabel::parse("claude").unwrap();
        let handled = promote_repo_master(repo.path(), None, &claude).unwrap();
        assert!(handled, "no-op short-circuit should return true");
        let body = std::fs::read_to_string(repo.path().join(".clank/config.json")).unwrap();
        let parsed: RepoConfigFile = serde_json::from_str(&body).unwrap();
        assert_eq!(
            parsed.team.master.as_ref().map(|l| l.as_str()),
            Some("claude")
        );
    }

    fn desc(tool: Tool) -> AgentDescription {
        AgentDescription {
            tool,
            launch: None,
            initial_prompt: None,
        }
    }

    #[test]
    fn define_repo_agent_writes_agents_only_not_team() {
        // `clank agent add` (repo scope) writes the definition into
        // `agents` and leaves the team completely untouched.
        use crate::cli::teams_config::RepoConfigFile;
        let repo = tempfile::tempdir().unwrap();
        seed_repo_config(
            repo.path(),
            r#"{
                "agents": { "claude": { "tool": "claude" } },
                "team": { "master": "claude" }
            }"#,
        );
        let codex = AgentLabel::parse("codex").unwrap();
        define_repo_agent(repo.path(), &codex, desc(Tool::Codex)).unwrap();
        let body = std::fs::read_to_string(repo.path().join(".clank/config.json")).unwrap();
        let parsed: RepoConfigFile = serde_json::from_str(&body).unwrap();
        assert!(parsed.agents.contains_key(&codex));
        assert_eq!(parsed.agents[&codex].tool, Tool::Codex);
        // Team is untouched: master still claude, no reviewers added.
        assert_eq!(
            parsed.team.master.as_ref().map(|l| l.as_str()),
            Some("claude")
        );
        assert!(parsed.team.commit_reviewers.is_empty());
        assert!(parsed.team.gate_reviewers.is_empty());
    }

    #[test]
    fn define_repo_agent_rejects_duplicate_label() {
        let repo = tempfile::tempdir().unwrap();
        seed_repo_config(
            repo.path(),
            r#"{
                "agents": { "claude": { "tool": "claude" }, "codex": { "tool": "codex" } },
                "team": { "master": "claude", "commit_reviewers": ["codex"] }
            }"#,
        );
        let codex = AgentLabel::parse("codex").unwrap();
        let err = define_repo_agent(repo.path(), &codex, desc(Tool::Codex)).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("already defined"));
        assert!(msg.contains("clank agent remove"));
    }

    #[test]
    fn remove_repo_agent_drops_definition_when_not_in_team() {
        use crate::cli::teams_config::RepoConfigFile;
        let repo = tempfile::tempdir().unwrap();
        // `spare` is defined but NOT in the team → removable.
        seed_repo_config(
            repo.path(),
            r#"{
                "agents": { "claude": { "tool": "claude" }, "spare": { "tool": "codex" } },
                "team": { "master": "claude" }
            }"#,
        );
        let spare = AgentLabel::parse("spare").unwrap();
        remove_repo_agent(repo.path(), &spare).unwrap();
        let body = std::fs::read_to_string(repo.path().join(".clank/config.json")).unwrap();
        let parsed: RepoConfigFile = serde_json::from_str(&body).unwrap();
        assert!(!parsed.agents.contains_key(&spare));
        // master untouched.
        assert_eq!(
            parsed.team.master.as_ref().map(|l| l.as_str()),
            Some("claude")
        );
    }

    #[test]
    fn remove_repo_agent_refuses_when_referenced_by_team() {
        use crate::cli::teams_config::RepoConfigFile;
        let repo = tempfile::tempdir().unwrap();
        seed_repo_config(
            repo.path(),
            r#"{
                "agents": { "claude": { "tool": "claude" }, "codex": { "tool": "codex" } },
                "team": { "master": "claude", "commit_reviewers": ["codex"] }
            }"#,
        );
        // Reviewer is refused.
        let codex = AgentLabel::parse("codex").unwrap();
        let err = remove_repo_agent(repo.path(), &codex).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("is in this repo's team"));
        assert!(msg.contains("clank team remove codex"));

        // Master is refused too.
        let claude = AgentLabel::parse("claude").unwrap();
        let err = remove_repo_agent(repo.path(), &claude).unwrap_err();
        assert!(format!("{err:#}").contains("clank team remove claude"));

        // Config unchanged — both definitions survive.
        let body = std::fs::read_to_string(repo.path().join(".clank/config.json")).unwrap();
        let parsed: RepoConfigFile = serde_json::from_str(&body).unwrap();
        assert!(parsed.agents.contains_key(&codex));
        assert!(parsed.agents.contains_key(&claude));
    }

    #[test]
    fn remove_repo_agent_errors_when_not_defined() {
        let repo = tempfile::tempdir().unwrap();
        seed_repo_config(
            repo.path(),
            r#"{
                "agents": { "claude": { "tool": "claude" } },
                "team": { "master": "claude" }
            }"#,
        );
        let phantom = AgentLabel::parse("phantom").unwrap();
        let err = remove_repo_agent(repo.path(), &phantom).unwrap_err();
        assert!(format!("{err:#}").contains("not defined"));
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

    #[test]
    fn fork_launch_composes_per_tool_with_orientation_prompt() {
        // clank-fork-worktree-sessions: the seeded fork spec turns
        // into the per-tool fork argv with the orientation prompt
        // as the trailing positional. Both tools fork cleanly
        // (verified live 2026-06-10 — the stale "codex can't fork"
        // research claim is dead).
        let desc = AgentDescription {
            tool: Tool::Claude,
            launch: None,
            initial_prompt: None,
        };
        let spec = crate::cli::fork::ForkSpec {
            tool: Tool::Claude,
            from_session: "abc-123".into(),
            prompt: "You are `claude` in worktree `x`…".into(),
        };
        let wt = std::path::Path::new("/repo/.clank/worktrees/x");
        let c = compose_fork_launch(&spec, &desc, wt);
        assert_eq!(c.program, "claude");
        // claude takes cwd from the pane — no -C.
        assert_eq!(
            c.args,
            vec![
                "--resume",
                "abc-123",
                "--fork-session",
                "You are `claude` in worktree `x`…",
            ]
        );

        let desc = AgentDescription {
            tool: Tool::Codex,
            launch: None,
            initial_prompt: None,
        };
        let spec = crate::cli::fork::ForkSpec {
            tool: Tool::Codex,
            from_session: "def-456".into(),
            prompt: "orient".into(),
        };
        let c = compose_fork_launch(&spec, &desc, wt);
        assert_eq!(c.program, "codex");
        // codex gets -C <worktree> so it doesn't prompt for the cwd
        // (fork-codex-cd-flag).
        assert_eq!(
            c.args,
            vec![
                "fork",
                "-C",
                "/repo/.clank/worktrees/x",
                "def-456",
                "orient",
            ]
        );
    }
}
