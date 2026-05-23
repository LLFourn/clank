//! `clank as <label>` — bind the calling agent's session to a
//! clank label.
//!
//! Reads `CLAUDE_CODE_SESSION_ID` / `CODEX_THREAD_ID` from env,
//! writes `.clank/agents/<label>/config.json` with the session
//! field set, and clears the same session id from any OTHER
//! agent's config (one session can't be bound to two labels at
//! once).

use anyhow::Context;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::{AsArgs, resolve_repo};
use crate::agent_env::detect_session_from_env;
use crate::agent_store::{load_agent_config, load_all_agent_configs, save_agent_config};
use clank_core::agent_config::Session;
use clank_core::ids::AgentLabel;

pub async fn run(args: AsArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    let label = AgentLabel::parse(&args.label)
        .map_err(|e| anyhow::anyhow!("invalid label `{}`: {e}", args.label))?;

    let (tool, session_id) = detect_session_from_env()?.ok_or_else(|| {
        anyhow::anyhow!(
            "no session detected in env (CLAUDE_CODE_SESSION_ID / \
             CODEX_THREAD_ID not set). `clank as` must be run from \
             inside a claude or codex session."
        )
    })?;

    // Load all existing configs FIRST (strict — fail on any parse
    // error so we don't silently keep a stale binding we couldn't
    // see). Identify which other agents currently hold this
    // session id so we can clear them.
    let all = load_all_agent_configs(&repo)?;
    let stale: Vec<(AgentLabel, clank_core::agent_config::AgentConfig)> = all
        .into_iter()
        .filter(|(other_label, cfg)| {
            other_label != &label && cfg.session.as_ref().is_some_and(|s| s.id == session_id)
        })
        .collect();

    // Bind the new label FIRST. Order matters: if the clear-stale
    // step below partially fails, the worst case is that an old
    // agent still holds a ghost binding, but the user's intended
    // new binding is recorded. The reverse (clear-then-bind)
    // could leave them with neither.
    let mut cfg = load_agent_config(&repo, &label)?.unwrap_or_default();
    let now = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .context("formatting timestamp")?;
    cfg.session = Some(Session {
        id: session_id.clone(),
        tool,
        updated_at: now,
    });
    save_agent_config(&repo, &label, &cfg)?;

    let mut cleared_from: Vec<AgentLabel> = Vec::new();
    for (other_label, mut other_cfg) in stale {
        other_cfg.session = None;
        save_agent_config(&repo, &other_label, &other_cfg)
            .with_context(|| format!("clearing stale binding on `{}`", other_label.as_str()))?;
        cleared_from.push(other_label);
    }

    println!(
        "bound {tool} session {sid} to agent `{label}`",
        tool = tool.as_str(),
        sid = session_id.as_str(),
        label = label.as_str(),
    );
    for other in cleared_from {
        println!("  (cleared stale binding on `{}`)", other.as_str());
    }
    Ok(())
}

// Concurrency note: two `clank as` runs in two shells using the
// SAME session id could race past each other's stale-scan and
// both succeed. Result: two labels hold the same session id; the
// resolver picks whichever it scans first. Acceptable for v1 —
// the race window is tiny and the recovery is `clank as <real>`
// once you notice. Revisit (with a `.clank/.lock` advisory file)
// if it becomes a real problem.
