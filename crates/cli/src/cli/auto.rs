//! `clank auto on|off|status` — manage this agent's auto-mode
//! (the per-agent Stop-hook behavior) for the current repo.
//!
//! Resolves the calling agent's label via `agent_env::
//! resolve_identity_from_env`. Errors with an actionable message
//! if the session isn't bound — `clank auto` is NOT a bootstrap
//! path (that's `clank as`).
//!
//! `auto_mode` is per-agent STATE. Role is NOT — roles are
//! roster-derived (an agent's entry in the repo roster), so
//! the `--role` flag here is an accepted no-op (kept only so
//! older invocations don't hard-error; it prints a note). Change
//! roles via `clank agent promote` / `clank agent add`.

use super::{AutoArgs, AutoCmd, AutoOffArgs, AutoOnArgs, AutoStatusArgs, resolve_repo};
use crate::agent_env::resolve_identity_from_env;
use crate::agent_store::{load_agent_config, set_auto_mode, update_agent_config};
use clank_core::vocab::AutoMode;

/// `clank auto status --json` wire shape. `auto_mode` is the
/// EFFECTIVE mode; `auto_mode_explicit` is `null` when no per-agent
/// override is set. Borrowed fields; consumers parse JSON so key
/// order is free (typed-json-not-json-macro).
#[derive(serde::Serialize)]
struct AutoStatusJson<'a> {
    label: &'a str,
    auto_mode: &'a str,
    auto_mode_explicit: Option<&'a str>,
    wait_timeout: Option<&'a str>,
    role: &'a str,
}

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

    update_agent_config(&repo, &label, |cfg| {
        cfg.auto_mode = Some(AutoMode::On);
        if let Some(t) = args.wait_timeout.as_deref() {
            cfg.wait_timeout = Some(t.to_string());
        }
    })?;

    println!("auto-mode for `{}` set to on", label.as_str());
    if args.role.is_some() {
        // Plan: teams-based-agent-registration — role is now a
        // per-team property, not per-agent state. `--role` no
        // longer writes anything; change roles via
        // `clank agent promote` / `clank agent add`.
        eprintln!(
            "note: `--role` is ignored; roles are roster-derived now (use `clank agent promote`)"
        );
    }
    Ok(())
}

async fn run_off(args: AutoOffArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    let label = resolve_identity_from_env(&repo)?;

    set_auto_mode(&repo, &label, AutoMode::Off)?;

    println!("auto-mode for `{}` set to off", label.as_str());
    if args.role.is_some() {
        eprintln!(
            "note: `--role` is ignored; roles are roster-derived now (use `clank agent promote`)"
        );
    }
    Ok(())
}

async fn run_status(args: AutoStatusArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    let label = resolve_identity_from_env(&repo)?;

    let cfg = load_agent_config(&repo, &label)?.unwrap_or_default();
    // Report the EFFECTIVE auto-mode (explicit per-agent, else the
    // ~/.clank default, else off) — the same value the stop hook
    // acts on (auto-mode-default-on).
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let effective = crate::cli::team::resolve_effective_auto_mode(Some(&cfg), home.as_deref());
    // Role is roster-derived; best-effort (a repo with no master
    // has no resolvable role — show `unknown` rather than error).
    let role = crate::agent_store::resolve_role(&repo, &label)
        .map(|r| r.as_str().to_string())
        .unwrap_or_else(|_| "unknown (no master configured)".to_string());

    if args.json {
        let payload = AutoStatusJson {
            label: label.as_str(),
            auto_mode: effective.as_str(),
            auto_mode_explicit: cfg.auto_mode.map(|m| m.as_str()),
            wait_timeout: cfg.wait_timeout.as_deref(),
            role: &role,
        };
        println!("{}", serde_json::to_string(&payload)?);
    } else {
        println!("agent: {}", label.as_str());
        println!("  auto_mode:   {}", effective.as_str());
        println!(
            "  wait_timeout: {}",
            cfg.wait_timeout.as_deref().unwrap_or("(indefinite)")
        );
        println!("  role:        {role}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_status_json_matches_prior_shape() {
        // typed-json-not-json-macro: AutoStatusJson must serialize to
        // the same keys+values the old `json!` produced. `json!` here
        // expresses the expected value; key order is irrelevant
        // (`to_value` equality is order-independent).
        let with_explicit = AutoStatusJson {
            label: "codex",
            auto_mode: "on",
            auto_mode_explicit: Some("on"),
            wait_timeout: Some("30s"),
            role: "reviewer",
        };
        assert_eq!(
            serde_json::to_value(&with_explicit).unwrap(),
            serde_json::json!({
                "label": "codex",
                "auto_mode": "on",
                "auto_mode_explicit": "on",
                "wait_timeout": "30s",
                "role": "reviewer",
            })
        );

        // Unset explicit + timeout serialize to `null`, matching the
        // old `Option` values.
        let unset = AutoStatusJson {
            label: "claude",
            auto_mode: "off",
            auto_mode_explicit: None,
            wait_timeout: None,
            role: "master",
        };
        assert_eq!(
            serde_json::to_value(&unset).unwrap(),
            serde_json::json!({
                "label": "claude",
                "auto_mode": "off",
                "auto_mode_explicit": null,
                "wait_timeout": null,
                "role": "master",
            })
        );
    }
}
