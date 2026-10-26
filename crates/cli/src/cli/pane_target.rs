//! The pane a typed write goes to, shared by every writer: nudges, event
//! interruption, recovery, check-ins and user commands. A pane is its
//! agent's by its name alone, within the repo's TUI-anchored workspace;
//! the harness in it is the one the roster gives that agent. No hook,
//! inbox or session binding is read, and nothing runs between lookups.

use clank_core::{Tool, ids::AgentLabel};
use std::path::{Path, PathBuf};

use super::open_zellij::{self, Lookup, SessionPane, SessionScan, Unresolved};

/// One explicit batch lookup; healthy servers respond well below this deadline.
const SCAN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(1);

/// The one live pane named by an agent in its repo's workspace.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Found {
    pub(crate) session: String,
    pub(crate) pane: String,
}

/// Pick the target from one scan, or explain why there isn't one: the
/// one live pane named `label` in the repo's workspace, its panes named
/// against `candidates`.
///
/// Fails CLOSED on every doubt. A launch may treat a session that did
/// not answer as one fewer pane; here the same silence could be hiding
/// the repo's workspace or a second copy of it, and typing a message to
/// a human into an arbitrary agent is the outcome this command exists to
/// avoid.
pub(crate) fn locate(
    scan: &SessionScan,
    repo: &Path,
    label: &AgentLabel,
    candidates: std::collections::BTreeSet<String>,
) -> anyhow::Result<Found> {
    let (session, ws) = match open_zellij::scan_workspace(scan, repo, candidates) {
        Ok(found) => found,
        Err(Unresolved::ListingFailed) => anyhow::bail!(
            "could not ask zellij what sessions exist, so nothing is known about \
             `{label}`'s pane — not even whether there is one. Check that `zellij` runs, \
             then retry; nothing was typed."
        ),
        Err(Unresolved::Unanswered(missed)) => anyhow::bail!(
            "session(s) {} did not answer within {SCAN_DEADLINE:?}, and any of them may \
             hold {}'s tab or a second one, so where `{label}` is cannot be known — this \
             is not the same as `{label}` having no pane. Close or fix the unresponsive \
             session, then retry; nothing was typed.",
            missed.join(", "),
            repo.display()
        ),
        Err(why @ (Unresolved::NoAnchor | Unresolved::ManyAnchors(_))) => anyhow::bail!(
            "{why} ({}), so there is no one workspace to find `{label}` in; nothing was \
             typed. A repo's workspace is the tab holding its TUI pane: `clank open` opens \
             one.",
            repo.display()
        ),
    };
    let named: Vec<&SessionPane> = match ws.lookup(
        scan.panes.iter().filter(|sp| sp.session == session),
        label.as_str(),
    ) {
        Lookup::Known(named) => named,
        Lookup::Uncertain(doubts) => anyhow::bail!(
            "which pane is `{label}` in {}'s tab (session `{session}`) cannot be known: {}. \
             Rename that pane to one agent's label, then retry; nothing was typed.",
            repo.display(),
            open_zellij::describe_ambiguities(&doubts)
        ),
    };
    let live: Vec<&&SessionPane> = named.iter().filter(|sp| !sp.pane.has_exited()).collect();
    match live.as_slice() {
        [one] => Ok(Found {
            session: one.session.clone(),
            pane: one.pane.pane_id(),
        }),
        [] if !named.is_empty() => anyhow::bail!(
            "`{label}`'s pane in session `{session}` has EXITED — its agent is not running, \
             and zellij would accept the keystrokes anyway. Restart it (Ctrl-a r in the \
             console) and retry."
        ),
        [] => anyhow::bail!(
            "no pane named `{label}` in {}'s tab (session `{session}`) — `clank open` \
             starts the team.",
            repo.display()
        ),
        many => anyhow::bail!(
            "`{label}` has {} live panes in {}'s tab (session `{session}`: {}) — close the \
             duplicate, then retry. Typing into an arbitrary one is exactly the \
             misdelivery this command exists to avoid.",
            many.len(),
            repo.display(),
            many.iter()
                .map(|sp| sp.pane.pane_id())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// The one live pane named by an agent in its repo's workspace, and the
/// tool the roster gives that agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Target {
    pub(crate) session: String,
    pub(crate) pane: String,
    pub(crate) tool: Tool,
}

impl Target {
    pub(crate) fn input(&self) -> super::pane_input::Target<'_> {
        super::pane_input::Target {
            session: &self.session,
            pane: &self.pane,
            tool: self.tool,
        }
    }
}

/// `label`'s target, from one scan of every zellij session taken now.
pub(crate) async fn discover(repo: PathBuf, label: AgentLabel) -> anyhow::Result<Target> {
    let scan = open_zellij::panes_in_all_sessions_async(SCAN_DEADLINE).await;
    super::event_interrupt::blocking(move || target_in(&scan, &repo, &label))
}

/// [`discover`] on a scan already taken.
pub(crate) fn target_in(
    scan: &SessionScan,
    repo: &Path,
    label: &AgentLabel,
) -> anyhow::Result<Target> {
    let tool = crate::agent_store::resolve_tool(repo, label)?;
    let candidates = super::pane_owners::candidates(repo, label.as_str());
    let Found { session, pane } = locate(scan, repo, label, candidates)?;
    Ok(Target {
        session,
        pane,
        tool,
    })
}
