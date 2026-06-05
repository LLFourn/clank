//! `clank agent list` — enumerate registered agents in this repo
//! with role and bind state. Read-only; never mutates state.
//!
//! Source of truth: the **merged agent declaration** (repo-scope
//! `<repo>/.clank/config.json#/agents` if present, else user-scope
//! `~/.clank/config.json#/default_agents`). Per
//! `agent-add-cli-and-repo-scope`, the declaration drives role +
//! tool + launch; per-agent skeleton supplies session binding
//! state. List joins both. Declared agents without skeletons
//! render as unbound; orphan skeletons (skeleton without
//! declaration) are NOT listed — `clank doctor` surfaces them
//! separately.

use std::path::Path;

use serde::Serialize;

use crate::agent_store::load_agent_config;
use clank_core::agent_config::{LaunchConfig, Session};
use clank_core::ids::AgentLabel;
use clank_core::vocab::{Role, Tool};

use super::{AgentArgs, AgentCmd, AgentListArgs, AgentStartArgs, resolve_repo};

pub async fn run(args: AgentArgs) -> anyhow::Result<()> {
    match args.command {
        AgentCmd::List(a) => list(a),
        AgentCmd::Start(a) => start(a),
    }
}

#[derive(Debug, Serialize)]
struct AgentRow {
    label: String,
    role: String,
    bound: bool,
    tool: Option<String>,
    session_id: Option<String>,
}

impl AgentRow {
    fn from_join(
        label: &AgentLabel,
        role: Role,
        declared_tool: Option<Tool>,
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
            None => (false, declared_tool.map(|t| t.as_str().to_string()), None),
        };
        Self {
            label: label.as_str().to_string(),
            role: role.as_str().to_string(),
            bound,
            tool: tool_str,
            session_id,
        }
    }
}

fn list(args: AgentListArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    // Declaration is the source of truth for which agents are
    // registered; skeleton supplies session state (codex review of
    // eef4c49 — was scanning skeleton dirs).
    let declared = crate::cli::config::load_merged_agents(&repo, home.as_deref())?;
    let mut rows: Vec<AgentRow> = Vec::with_capacity(declared.len());
    for entry in &declared {
        let skeleton = load_agent_config(&repo, &entry.label)?;
        rows.push(AgentRow::from_join(
            &entry.label,
            entry.role,
            entry.tool,
            skeleton.as_ref().and_then(|c| c.session.as_ref()),
        ));
    }
    // Stable ordering: master first, then alphabetic by label.
    rows.sort_by(|a, b| {
        let role_key = |r: &str| match r {
            "master" => 0,
            _ => 1,
        };
        role_key(&a.role)
            .cmp(&role_key(&b.role))
            .then_with(|| a.label.cmp(&b.label))
    });
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

/// `clank agent start <name>`: exec into the agent's CLI tool
/// with session restored and the configured launch profile
/// applied. Requires a bound session (one policy: no fallback
/// to bare tool — see plan rationale at Phase B step 2 of
/// `agent-config-and-start`).
///
/// Launch profile source: the merged declaration's `launch`
/// field (per `agent-add-cli-and-repo-scope` — declaration is
/// the source of truth for role + tool + launch). Session
/// binding still comes from the per-agent skeleton.
fn start(args: AgentStartArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    let label = AgentLabel::parse(&args.name)
        .map_err(|e| anyhow::anyhow!("invalid agent label `{}`: {e}", args.name))?;

    let cfg = match load_agent_config(&repo, &label)? {
        Some(c) => c,
        None => {
            anyhow::bail!(
                "no agent `{name}` in this repo. Create `.clank/agents/{name}/config.json` \
                 (or hand-edit one from the seed) before running `clank agent start`.",
                name = args.name
            );
        }
    };

    let session = cfg.session.as_ref().ok_or_else(|| {
        anyhow::anyhow!(
            "agent `{name}` has no bound session. Run `clank as {name}` from inside the agent's CLI to bind.",
            name = args.name
        )
    })?;

    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    // Propagate parse errors — silently falling back to an empty
    // declaration would drop the configured launch profile + leave
    // the user puzzled why `--skill X` wasn't applied. Codex caught
    // the unwrap_or_default on eef4c49.
    let merged = crate::cli::config::load_merged_agents(&repo, home.as_deref())?;
    let declaration_launch = merged
        .iter()
        .find(|e| e.label == label)
        .and_then(|e| e.launch.as_ref());

    let composed = compose_launch(&repo, session, declaration_launch);

    if args.print {
        print_composed(&composed);
        return Ok(());
    }

    exec_composed(composed)
}

/// Composed launch line: executable + argv + env additions.
#[derive(Debug, Clone)]
struct ComposedLaunch {
    program: String,
    args: Vec<String>,
    env_overrides: std::collections::BTreeMap<String, String>,
}

fn compose_launch(repo: &Path, session: &Session, launch: Option<&LaunchConfig>) -> ComposedLaunch {
    let tool = session.tool;
    let program = launch
        .and_then(|l| l.command.clone())
        .unwrap_or_else(|| tool.as_str().to_string());

    let launch_args = launch.map(|l| l.args.clone()).unwrap_or_default();

    let session_restore = session_restore_args(tool, session.id.as_str(), repo);

    let mut args = launch_args;
    args.extend(session_restore);

    let env_overrides = launch.map(|l| l.env.clone()).unwrap_or_default();

    ComposedLaunch {
        program,
        args,
        env_overrides,
    }
}

/// Tool-specific session-restore suffix. Goes AFTER `launch.args`
/// so codex's `resume` subcommand doesn't capture them (see Phase
/// B step 3 of the plan).
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

    #[test]
    fn agent_row_bound_uses_session_tool() {
        let session = make_session();
        let row = AgentRow::from_join(
            &label("alice"),
            Role::Reviewers,
            Some(Tool::Codex), // declared tool (overridden by session)
            Some(&session),
        );
        assert_eq!(row.label, "alice");
        assert_eq!(row.role, "reviewers");
        assert!(row.bound);
        assert_eq!(row.tool.as_deref(), Some("claude")); // from session
        assert!(row.session_id.is_some());
    }

    #[test]
    fn agent_row_unbound_uses_declared_tool() {
        let row = AgentRow::from_join(&label("alice"), Role::Reviewers, Some(Tool::Claude), None);
        assert!(!row.bound);
        assert_eq!(row.tool.as_deref(), Some("claude"));
        assert!(row.session_id.is_none());
    }

    #[test]
    fn agent_row_unbound_no_declared_tool_shows_none() {
        let row = AgentRow::from_join(&label("alice"), Role::Reviewers, None, None);
        assert!(!row.bound);
        assert!(row.tool.is_none());
    }
}
