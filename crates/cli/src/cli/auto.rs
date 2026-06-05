//! `clank auto on|off|status` — manage this agent's auto-mode
//! and role for the current repo.
//!
//! Resolves the calling agent's label via `agent_env::
//! resolve_identity_from_env`. Errors with an actionable message
//! if the session isn't bound — `clank auto` is NOT a bootstrap
//! path (that's `clank as` or `clank init` phase 2).
//!
//! Role is a per-user preference stored on the agent's own
//! config. Setting `--role master` doesn't make any repo-wide
//! assertion — it just changes what `wfw` / `stop-hook` default
//! to for THIS agent.

use super::{AutoArgs, AutoCmd, AutoOffArgs, AutoOnArgs, AutoStatusArgs, resolve_repo};
use crate::agent_env::resolve_identity_from_env;
use crate::agent_store::{load_agent_config, save_agent_config};
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

    let mut cfg = load_agent_config(&repo, &label)?.unwrap_or_default();
    cfg.auto_mode = AutoMode::On;
    if let Some(t) = args.wfw_timeout.as_deref() {
        cfg.wfw_timeout = Some(t.to_string());
    }
    if let Some(role_arg) = args.role {
        cfg.role = role_arg.into();
    }
    save_agent_config(&repo, &label, &cfg)?;

    println!(
        "auto-mode for `{}` set to {}",
        label.as_str(),
        cfg.auto_mode.as_str()
    );
    if args.role.is_some() {
        println!("  role: {}", cfg.role.as_str());
    }
    Ok(())
}

async fn run_off(args: AutoOffArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    let label = resolve_identity_from_env(&repo)?;

    let mut cfg = load_agent_config(&repo, &label)?.unwrap_or_default();
    cfg.auto_mode = AutoMode::Off;
    if let Some(role_arg) = args.role {
        cfg.role = role_arg.into();
    }
    save_agent_config(&repo, &label, &cfg)?;

    println!("auto-mode for `{}` set to off", label.as_str());
    if args.role.is_some() {
        println!("  role: {}", cfg.role.as_str());
    }
    Ok(())
}

async fn run_status(args: AutoStatusArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    let label = resolve_identity_from_env(&repo)?;

    let cfg = load_agent_config(&repo, &label)?.unwrap_or_default();
    // Role resolution prefers the merged declaration (codex review
    // of da71c84). Fall back to the skeleton's role for pre-Phase-1
    // repos.
    let role = crate::agent_store::resolve_role(&repo, &label)
        .unwrap_or_else(|_| role_for(&label, Some(&cfg)));

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

impl From<crate::cli::RoleArg> for Role {
    fn from(r: crate::cli::RoleArg) -> Self {
        match r {
            crate::cli::RoleArg::Master => Role::Master,
            crate::cli::RoleArg::Reviewer => Role::Reviewer,
        }
    }
}
