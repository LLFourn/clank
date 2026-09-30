//! `clank nudge`: type a message straight into an agent's pane.
//!
//! `clank send` is the right mechanism and has a failure mode it
//! cannot report: it writes a durable inbox row and trusts the agent's
//! wake path to deliver it. When that wake path is the broken thing,
//! the message lands on disk, the send succeeds, and nobody finds out.
//! An orphaned Claude Code mod once left an agent erroring at every
//! turn end for hours, unable to receive anything, while its commit sat
//! unreviewed.
//!
//! So this shares nothing with the machinery it exists to bypass. It
//! reads no hook, no inbox, no session binding and no wait — only the
//! live pane listing — and it writes nothing to disk.

use std::path::Path;
use std::time::Duration;

use clank_core::ids::AgentLabel;

use super::{NudgeArgs, resolve_repo};
use crate::cli::open_zellij::{self, SessionPane, SessionScan};

/// Matches `session_holder`'s: healthy servers answer in well under
/// 0.2 s, and a stale one must not hold up a person trying to reach an
/// agent that is right there.
const SCAN_DEADLINE: Duration = Duration::from_secs(1);

pub fn run(args: NudgeArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    let to = crate::cli::send::recipient(&repo, args.to.as_deref(), args.master)?;
    let body = crate::cli::send::body_of(&args.message, args.file.as_deref())?;

    open_zellij::require_zellij()?;
    let scan = open_zellij::panes_in_all_sessions(SCAN_DEADLINE);
    let found = locate(&scan, &repo, &to)?;

    let tool = crate::agent_store::resolve_tool(&repo, &to)?;
    open_zellij::say_to_pane_for_tool(&found.session, &found.pane, tool, &body)?;
    println!(
        "nudged `{to}` in {} (session {}, pane {})",
        repo.display(),
        found.session,
        found.pane
    );
    Ok(())
}

/// The one pane a nudge may type into.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Found {
    pub(crate) session: String,
    pub(crate) pane: String,
}

/// Pick the target from one scan, or explain why there isn't one.
///
/// Fails CLOSED on every doubt. A launch may treat a session that did
/// not answer as one fewer pane; here the same silence could be hiding
/// the target or a second copy of it, and typing a message to a human
/// into an arbitrary agent is the outcome this command exists to avoid.
pub(crate) fn locate(scan: &SessionScan, repo: &Path, label: &AgentLabel) -> anyhow::Result<Found> {
    let command = open_zellij::agent_start_command(label.as_str(), &repo.to_string_lossy());
    let launched: Vec<&SessionPane> = scan
        .panes
        .iter()
        .filter(|sp| sp.pane.launched_by(&command))
        .collect();
    let live: Vec<&&SessionPane> = launched.iter().filter(|sp| !sp.pane.has_exited()).collect();

    if scan.listing_failed {
        anyhow::bail!(
            "could not ask zellij what sessions exist, so nothing is known about \
             `{label}`'s pane — not even whether there is one. Check that `zellij` runs, \
             then retry; nothing was typed."
        );
    }

    let missed = || scan.unanswered.join(", ");
    match live.as_slice() {
        [one] if scan.unanswered.is_empty() => Ok(Found {
            session: one.session.clone(),
            pane: one.pane.pane_id(),
        }),
        [one] => anyhow::bail!(
            "`{label}` has a pane in session `{}`, but session(s) {} did not answer — one of \
             them may hold another `{label}` for this repo, and a nudge to the wrong one is \
             not recoverable. Close or fix the unresponsive session, then retry.",
            one.session,
            missed()
        ),
        [] if !launched.is_empty() => anyhow::bail!(
            "`{label}`'s pane in session `{}` has EXITED — its agent is not running, and \
             zellij would accept the keystrokes anyway. Restart it (Ctrl-a r in the \
             console) and retry.",
            launched[0].session
        ),
        [] if !scan.unanswered.is_empty() => anyhow::bail!(
            "no live pane for `{label}` in {}, and session(s) {} did not answer within \
             {SCAN_DEADLINE:?} — this is not the same as `{label}` having no pane, so \
             nothing was typed.",
            repo.display(),
            missed()
        ),
        [] => anyhow::bail!(
            "no pane for `{label}` in {} in any live zellij session — `clank open zellij` \
             starts the team.",
            repo.display()
        ),
        many => anyhow::bail!(
            "`{label}` in {} has live panes in {} sessions ({}) — close the duplicate, then \
             retry. Typing into an arbitrary one is exactly the misdelivery this command \
             exists to avoid.",
            repo.display(),
            many.len(),
            many.iter()
                .map(|sp| sp.session.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> &'static Path {
        Path::new("/w/proj")
    }

    fn label(name: &str) -> AgentLabel {
        AgentLabel::parse(name).unwrap()
    }

    /// The escape hatch must not consult what may be broken.
    ///
    /// A stale session binding is one of the ways `clank send` fails,
    /// and the web bridge's `say` sends to `load_agent_config(..)
    /// .session.id` over the app-server — so a binding that still
    /// names a loaded thread would deliver somewhere else entirely and
    /// report success. No runtime test can watch for a call that is
    /// not made, so this reads the module.
    #[test]
    fn the_nudge_path_reads_no_session_binding() {
        let src = include_str!("nudge.rs");
        let production = src
            .split_once("#[cfg(test)]")
            .expect("this module has tests")
            .0;
        let code: String = production
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        for forbidden in ["load_agent_config", "codex_chat", "inbox::"] {
            assert!(
                !code.contains(forbidden),
                "`{forbidden}` is part of what a nudge exists to bypass"
            );
        }
    }

    /// A pane as `list-panes --json` reports one, in `session`.
    fn pane(session: &str, id: u32, command: &str, exited: bool) -> SessionPane {
        SessionPane {
            session: session.into(),
            pane: serde_json::from_value(serde_json::json!({
                "id": id,
                "terminal_command": command,
                "exited": exited,
            }))
            .unwrap(),
        }
    }

    fn agent_pane(session: &str, id: u32, name: &str, exited: bool) -> SessionPane {
        let cmd = open_zellij::agent_start_command(name, &repo().to_string_lossy());
        pane(session, id, &cmd, exited)
    }

    fn scan(panes: Vec<SessionPane>, unanswered: &[&str]) -> SessionScan {
        SessionScan {
            panes,
            unanswered: unanswered.iter().map(|s| s.to_string()).collect(),
            listing_failed: false,
        }
    }

    /// The case the first draft of the plan got wrong: `clank open`
    /// inside an existing session puts the repo's panes in THAT
    /// session, so the conventional `clank-<basename>` name is not
    /// where they are.
    #[test]
    fn a_pane_is_found_in_a_session_the_convention_does_not_name() {
        let found = locate(
            &scan(
                vec![agent_pane("someones-own-session", 3, "rev", false)],
                &[],
            ),
            repo(),
            &label("rev"),
        )
        .expect("a live pane is a target wherever it lives");
        assert_eq!(
            found,
            Found {
                session: "someones-own-session".into(),
                pane: "terminal_3".into(),
            }
        );
    }

    /// `launched_by` ignores exit state on purpose, so the naive
    /// composition types into a corpse and zellij returns success.
    #[test]
    fn a_corpse_is_refused_and_named() {
        let err = locate(
            &scan(vec![agent_pane("clank-proj", 3, "rev", true)], &[]),
            repo(),
            &label("rev"),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("EXITED"), "{err}");
        assert!(err.contains("clank-proj"), "says where: {err}");
    }

    #[test]
    fn two_live_panes_refuse_rather_than_pick() {
        let err = locate(
            &scan(
                vec![
                    agent_pane("clank-proj", 3, "rev", false),
                    agent_pane("other", 9, "rev", false),
                ],
                &[],
            ),
            repo(),
            &label("rev"),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("clank-proj") && err.contains("other"), "{err}");
    }

    /// The listing itself failing is not "no sessions".
    ///
    /// `zellij list-sessions` exits non-zero when none exist, so the
    /// ordinary reader folds a failed command into "none" on purpose.
    /// For a nudge that would turn "I could not ask" into a confident
    /// "that agent has no pane" (codex on 1eec51b).
    #[test]
    fn a_failed_listing_is_not_an_empty_one() {
        let blind = open_zellij::assemble_scan(None);
        assert!(blind.listing_failed);
        let err = locate(&blind, repo(), &label("rev"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("not even whether there is one"), "{err}");
        assert!(
            !err.contains("no pane for"),
            "an unasked question is not a negative answer: {err}"
        );
    }

    /// A session whose `list-panes` could not be started used to
    /// disappear from the scan entirely, taking its possible duplicate
    /// with it.
    #[test]
    fn a_session_that_could_not_be_probed_counts_as_unanswered() {
        let scan = open_zellij::assemble_scan(Some(vec![
            ("healthy".to_string(), Some(Vec::new())),
            ("unprobeable".to_string(), None),
        ]));
        assert_eq!(scan.unanswered, vec!["unprobeable".to_string()]);
        assert!(!scan.listing_failed);
    }

    /// A silent session could hold a second copy of the target, so one
    /// visible match is not enough to act on.
    #[test]
    fn one_match_beside_a_silent_session_still_refuses() {
        let err = locate(
            &scan(vec![agent_pane("clank-proj", 3, "rev", false)], &["stale"]),
            repo(),
            &label("rev"),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("stale"), "names what went unread: {err}");
        assert!(err.contains("another"), "and why that matters: {err}");
    }

    /// "Nothing answered" must not read as "the agent is not there".
    #[test]
    fn nothing_found_reads_differently_when_a_session_went_unread() {
        let quiet = locate(&scan(Vec::new(), &[]), repo(), &label("rev"))
            .unwrap_err()
            .to_string();
        let unread = locate(&scan(Vec::new(), &["stale"]), repo(), &label("rev"))
            .unwrap_err()
            .to_string();
        assert!(quiet.contains("no pane"), "{quiet}");
        assert!(
            !quiet.contains("did not answer"),
            "nothing was missed here: {quiet}"
        );
        assert!(unread.contains("did not answer"), "{unread}");
        assert_ne!(quiet, unread, "the two must not be the same sentence");
    }

    /// Another repo's agent of the same name is not this one's: the
    /// launch command carries the repo path.
    #[test]
    fn another_repos_pane_with_the_same_label_is_not_a_target() {
        let theirs = pane(
            "clank-proj",
            3,
            &open_zellij::agent_start_command("rev", "/w/other"),
            false,
        );
        let err = locate(&scan(vec![theirs], &[]), repo(), &label("rev")).unwrap_err();
        assert!(err.to_string().contains("no pane"), "{err}");
    }
}
