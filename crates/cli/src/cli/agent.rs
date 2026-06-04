//! `clank agent list` — enumerate registered agents in this repo
//! with role and bind state. Read-only; never mutates state.
//!
//! Reads from `.clank/agents/<label>/config.json` (the existing
//! source of truth used by the all-reviewers gate). Uses strict
//! loading so a malformed agent config surfaces as an error rather
//! than being silently dropped — same policy as the gate-input
//! path, since `clank agent list` is the user's window onto the
//! gate's reviewer set.

use std::path::Path;

use serde::Serialize;

use crate::agent_store::{load_agent_config, load_all_agent_configs};
use clank_core::agent_config::{AgentConfig, LaunchConfig, Session};
use clank_core::ids::AgentLabel;
use clank_core::vocab::Tool;

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
    fn from_config(label: &AgentLabel, cfg: &AgentConfig) -> Self {
        let (bound, tool, session_id) = match &cfg.session {
            Some(s) => (
                true,
                Some(s.tool.as_str().to_string()),
                Some(s.id.as_str().to_string()),
            ),
            None => (false, None, None),
        };
        Self {
            label: label.as_str().to_string(),
            role: cfg.role.as_str().to_string(),
            bound,
            tool,
            session_id,
        }
    }
}

fn list(args: AgentListArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    let raw = load_all_agent_configs(&repo)?;
    let mut rows: Vec<AgentRow> = raw
        .iter()
        .map(|(label, cfg)| AgentRow::from_config(label, cfg))
        .collect();
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
/// to bare tool — see plan rationale at Phase B step 2).
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

    let composed = compose_launch(&repo, session, cfg.launch.as_ref());

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

    use clank_core::vocab::Role;

    fn label(s: &str) -> AgentLabel {
        AgentLabel::parse(s).unwrap()
    }

    fn cfg(role: Role, bound: bool) -> AgentConfig {
        let mut c = AgentConfig::default();
        c.role = role;
        if bound {
            c.session = Some(clank_core::agent_config::Session {
                id: clank_core::ids::SessionId::parse("11111111-1111-1111-1111-111111111111")
                    .unwrap(),
                tool: clank_core::vocab::Tool::Claude,
                updated_at: "2026-06-04T12:00:00Z".to_string(),
            });
        }
        c
    }

    #[test]
    fn agent_row_from_config_bound() {
        let row = AgentRow::from_config(&label("alice"), &cfg(Role::Reviewers, true));
        assert_eq!(row.label, "alice");
        assert_eq!(row.role, "reviewers");
        assert!(row.bound);
        assert_eq!(row.tool.as_deref(), Some("claude"));
        assert!(row.session_id.is_some());
    }

    #[test]
    fn agent_row_from_config_unbound() {
        let row = AgentRow::from_config(&label("alice"), &cfg(Role::Reviewers, false));
        assert!(!row.bound);
        assert!(row.tool.is_none());
        assert!(row.session_id.is_none());
    }
}
