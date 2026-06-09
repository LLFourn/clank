//! `clank auto on|off|status` — manage this agent's auto-mode
//! (the per-agent Stop-hook behavior) for the current repo.
//!
//! Resolves the calling agent's label via `agent_env::
//! resolve_identity_from_env`. Errors with an actionable message
//! if the session isn't bound — `clank auto` is NOT a bootstrap
//! path (that's `clank as`).
//!
//! `auto_mode` is per-agent STATE. Role is NOT — under
//! `teams-based-agent-registration` roles are team-derived, so
//! the `--role` flag here is an accepted no-op (kept only so
//! older invocations don't hard-error; it prints a note). Change
//! roles via `clank team set-master` / `clank team add` /
//! `clank promote`.

use super::{AutoArgs, AutoCmd, AutoOffArgs, AutoOnArgs, AutoStatusArgs, resolve_repo};
use crate::agent_env::resolve_identity_from_env;
use crate::agent_store::{load_agent_config, save_agent_config};
use clank_core::vocab::AutoMode;

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
    save_agent_config(&repo, &label, &cfg)?;

    println!(
        "auto-mode for `{}` set to {}",
        label.as_str(),
        cfg.auto_mode.as_str()
    );
    if args.role.is_some() {
        // Plan: teams-based-agent-registration — role is now a
        // per-team property, not per-agent state. `--role` no
        // longer writes anything; change roles via
        // `clank team set-master` / `clank team add` /
        // `clank promote`.
        eprintln!(
            "note: `--role` is ignored; roles are team-derived now (use `clank team set-master` / `clank promote`)"
        );
    }
    Ok(())
}

async fn run_off(args: AutoOffArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    let label = resolve_identity_from_env(&repo)?;

    let mut cfg = load_agent_config(&repo, &label)?.unwrap_or_default();
    cfg.auto_mode = AutoMode::Off;
    save_agent_config(&repo, &label, &cfg)?;

    println!("auto-mode for `{}` set to off", label.as_str());
    if args.role.is_some() {
        eprintln!(
            "note: `--role` is ignored; roles are team-derived now (use `clank team set-master` / `clank promote`)"
        );
    }
    Ok(())
}

async fn run_status(args: AutoStatusArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    let label = resolve_identity_from_env(&repo)?;

    let cfg = load_agent_config(&repo, &label)?.unwrap_or_default();
    // Role is team-derived; best-effort (a repo with no team set
    // has no resolvable role — show `unknown` rather than error).
    let role = crate::agent_store::resolve_role(&repo, &label)
        .map(|r| r.as_str().to_string())
        .unwrap_or_else(|_| "unknown (no team configured)".to_string());

    if args.json {
        let payload = serde_json::json!({
            "label": label.as_str(),
            "auto_mode": cfg.auto_mode.as_str(),
            "wfw_timeout": cfg.wfw_timeout,
            "role": role,
        });
        println!("{}", serde_json::to_string(&payload)?);
    } else {
        println!("agent: {}", label.as_str());
        println!("  auto_mode:   {}", cfg.auto_mode.as_str());
        println!(
            "  wfw_timeout: {}",
            cfg.wfw_timeout.as_deref().unwrap_or("(indefinite)")
        );
        println!("  role:        {role}");
    }
    Ok(())
}
