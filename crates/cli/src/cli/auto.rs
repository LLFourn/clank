//! `clank auto on|off|status` — manage this agent's auto-mode
//! and role for the current repo.
//!
//! Resolves the calling agent's label via `agent_env::
//! resolve_identity_from_env`. Errors with an actionable message
//! if the session isn't bound — `clank auto` is NOT a bootstrap
//! path (that's `clank as` or `clank init` phase 2).

use anyhow::Context;

use super::{AutoArgs, AutoCmd, AutoOffArgs, AutoOnArgs, AutoStatusArgs, RoleArg, resolve_repo};
use crate::agent_env::resolve_identity_from_env;
use crate::agent_store::{
    load_agent_config, load_repo_config, save_agent_config, save_repo_config,
};
use clank_core::ids::AgentLabel;
use clank_core::role_for;
use clank_core::vocab::{AutoMode, Role};

pub async fn run(args: AutoArgs) -> anyhow::Result<()> {
    match args.command {
        AutoCmd::On(a) => run_on(a).await,
        AutoCmd::Off(a) => run_off(a).await,
        AutoCmd::Status(a) => run_status(a).await,
    }
}

async fn run_on(args: AutoOnArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    let label = resolve_identity_from_env(&repo)?;
    let mode: AutoMode = args.mode.into();

    let mut cfg = load_agent_config(&repo, &label)?.unwrap_or_default();
    cfg.auto_mode = mode;
    if let Some(t) = args.wfw_timeout.as_deref() {
        cfg.wfw_timeout = Some(t.to_string());
    }
    save_agent_config(&repo, &label, &cfg)?;

    let role_note = if let Some(role_arg) = args.role {
        Some(apply_role(&repo, &label, role_arg)?)
    } else {
        None
    };

    println!(
        "auto-mode for `{}` set to {}",
        label.as_str(),
        mode.as_str()
    );
    if let Some(note) = role_note {
        println!("  {note}");
    }
    Ok(())
}

async fn run_off(args: AutoOffArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    let label = resolve_identity_from_env(&repo)?;

    let mut cfg = load_agent_config(&repo, &label)?.unwrap_or_default();
    cfg.auto_mode = AutoMode::Off;
    save_agent_config(&repo, &label, &cfg)?;

    let role_note = if let Some(role_arg) = args.role {
        Some(apply_role(&repo, &label, role_arg)?)
    } else {
        None
    };

    println!("auto-mode for `{}` set to off", label.as_str());
    if let Some(note) = role_note {
        println!("  {note}");
    }
    Ok(())
}

async fn run_status(args: AutoStatusArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    let label = resolve_identity_from_env(&repo)?;

    let cfg = load_agent_config(&repo, &label)?.unwrap_or_default();
    let repo_cfg = load_repo_config(&repo)?;
    let role = role_for(&label, repo_cfg.as_ref());

    if args.json {
        let payload = serde_json::json!({
            "label": label.as_str(),
            "auto_mode": cfg.auto_mode.as_str(),
            "wfw_timeout": cfg.wfw_timeout,
            "role": role.as_str(),
        });
        println!("{}", serde_json::to_string(&payload)?);
    } else {
        println!("agent: {}", label.as_str());
        println!("  auto_mode:   {}", cfg.auto_mode.as_str());
        println!(
            "  wfw_timeout: {}",
            cfg.wfw_timeout.as_deref().unwrap_or("(indefinite)")
        );
        println!("  role:        {}", role.as_str());
    }
    Ok(())
}

/// Update `.clank/config.json`'s `master` field per `--role`,
/// returning a human note for the caller to print. Master claims
/// take precedence: `--role master` writes this label even if a
/// different one was master before (with a "changed from X" note);
/// `--role reviewers` clears master ONLY if this label currently
/// holds it.
fn apply_role(repo: &std::path::Path, label: &AgentLabel, role: RoleArg) -> anyhow::Result<String> {
    let mut repo_cfg = load_repo_config(repo)?.unwrap_or_default();
    let prior_master = repo_cfg.master.clone();
    match role {
        RoleArg::Master => {
            repo_cfg.master = Some(label.clone());
            save_repo_config(repo, &repo_cfg).context("writing .clank/config.json")?;
            match prior_master {
                Some(prev) if &prev == label => Ok(format!(
                    "role: master (already designated; .clank/config.json unchanged)"
                )),
                Some(prev) => Ok(format!(
                    "role: master (changed from `{}` to `{}` in .clank/config.json)",
                    prev.as_str(),
                    label.as_str()
                )),
                None => Ok(format!("role: master (written to .clank/config.json)")),
            }
        }
        RoleArg::Reviewers => {
            if prior_master.as_ref() == Some(label) {
                repo_cfg.master = None;
                save_repo_config(repo, &repo_cfg).context("writing .clank/config.json")?;
                Ok(format!(
                    "role: reviewers (cleared `{}` from .clank/config.json master)",
                    label.as_str()
                ))
            } else {
                Ok(format!(
                    "role: reviewers (this agent was not the master; .clank/config.json unchanged)"
                ))
            }
        }
    }
}

impl From<crate::cli::AutoModeArg> for AutoMode {
    fn from(m: crate::cli::AutoModeArg) -> Self {
        match m {
            crate::cli::AutoModeArg::Hint => AutoMode::Hint,
            crate::cli::AutoModeArg::Wait => AutoMode::Wait,
        }
    }
}

impl From<crate::cli::RoleArg> for Role {
    fn from(r: crate::cli::RoleArg) -> Self {
        match r {
            crate::cli::RoleArg::Master => Role::Master,
            crate::cli::RoleArg::Reviewers => Role::Reviewers,
        }
    }
}
