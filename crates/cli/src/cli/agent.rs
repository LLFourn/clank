//! `clank agent list` — enumerate registered agents in this repo
//! with role and bind state. Read-only; never mutates state.
//!
//! Reads from `.clank/agents/<label>/config.json` (the existing
//! source of truth used by the all-reviewers gate). Uses strict
//! loading so a malformed agent config surfaces as an error rather
//! than being silently dropped — same policy as the gate-input
//! path, since `clank agent list` is the user's window onto the
//! gate's reviewer set.

use serde::Serialize;

use crate::agent_store::load_all_agent_configs;
use clank_core::agent_config::AgentConfig;
use clank_core::ids::AgentLabel;

use super::{AgentArgs, AgentCmd, AgentListArgs, resolve_repo};

pub async fn run(args: AgentArgs) -> anyhow::Result<()> {
    match args.command {
        AgentCmd::List(a) => list(a),
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
