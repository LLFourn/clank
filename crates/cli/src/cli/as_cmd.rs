//! `clank as <label>` — bind the calling agent's session to a
//! clank label.
//!
//! Reads `CLAUDE_CODE_SESSION_ID` / `CODEX_THREAD_ID` from env,
//! writes `.clank/agents/<label>/config.json` with the session
//! field set, and clears the same session id from any OTHER
//! agent's config (one session can't be bound to two labels at
//! once).
//!
//! The bind operation itself lives in
//! [`crate::agent_store::bind_session_to_agent`] — shared with
//! `clank init` phase 2 so both callers preserve the same
//! uniqueness invariant without duplicating the stale-clear
//! logic.

use super::{AsArgs, resolve_repo};
use crate::agent_env::detect_session_from_env;
use crate::agent_store::bind_session_to_agent;
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

    // Codex 38e105e catch: respect the .empty sentinel. Writing a
    // skeleton here while sentinel is active would be a hidden
    // skeleton that a subsequent `clank agent add` (which clears
    // the sentinel as the 0→1 transition) would resurrect. Refuse
    // with guidance to the explicit two-step registration path.
    if crate::cli::config::empty_sentinel_path(&repo).is_file() {
        anyhow::bail!(
            "explicit-empty agents declaration is active \
             (`.clank/agents/.empty` present); refusing to bind \
             `{label}`. To register this agent, run `clank agent add \
             {label} --tool {tool}` first (which clears the sentinel \
             as the 0→1 transition), then re-run `clank as {label}`.",
            label = label.as_str(),
            tool = tool.as_str(),
        );
    }

    let outcome = bind_session_to_agent(&repo, &label, tool, &session_id)?;

    println!(
        "bound {tool} session {sid} to agent `{label}`",
        tool = tool.as_str(),
        sid = session_id.as_str(),
        label = outcome.label.as_str(),
    );
    for other in outcome.cleared_from {
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
