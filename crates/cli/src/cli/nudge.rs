//! `clank nudge`: interrupt the agent's current turn, then submit a message.
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
//! live pane listing. Its one write is the agent's nudge history, made
//! after the attempt's outcome is fixed and unable to change it.

use super::{NudgeArgs, resolve_repo};
use crate::cli::nudge_log::{Outcome, Record, Trigger};
use crate::cli::open_zellij;
use crate::cli::pane_input::Unconfirmed;

pub async fn run(args: NudgeArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    let to = crate::cli::send::recipient(&repo, args.to.as_deref(), args.master)?;
    let body = crate::cli::send::body_of(&args.message, args.file.as_deref())?;
    let agent_dir = crate::agent_store::agents_root(&repo).join(to.as_str());
    let where_ = nudge_with(&agent_dir, &body, attempt(&repo, &to, &body))
        .await
        .map_err(|failed| match failed {
            Failed::Refused(e) => e,
            Failed::Unconfirmed(e) => e.into(),
        })?;
    println!("nudged `{to}` in {} ({where_})", repo.display());
    Ok(())
}

/// Run `attempt`, then add it to the history. The record comes after the
/// attempt's outcome is fixed and cannot change it.
async fn nudge_with(
    agent_dir: &std::path::Path,
    body: &str,
    attempt: impl std::future::Future<Output = Result<String, Failed>>,
) -> Result<String, Failed> {
    let tried = attempt.await;
    record(agent_dir, body, &tried);
    tried
}

/// Why a nudge did not complete.
enum Failed {
    /// Nothing reached the terminal: no zellij, no pane, no provenance.
    Refused(anyhow::Error),
    Unconfirmed(Unconfirmed),
}

/// The nudge itself; on success, where it went.
async fn attempt(
    repo: &std::path::Path,
    to: &crate::lifecycle::AgentLabel,
    body: &str,
) -> Result<String, Failed> {
    open_zellij::require_zellij().map_err(Failed::Refused)?;
    let (found, tool) = super::pane_target::discover(repo.to_path_buf(), to.clone())
        .await
        .map_err(Failed::Refused)?;
    crate::cli::pane_input::interrupt_and_submit_async(&found.session, &found.pane, tool, body)
        .await
        .map_err(Failed::Unconfirmed)?;
    Ok(format!("session {}, pane {}", found.session, found.pane))
}

/// What the attempt's result lets the history say.
fn outcome<T>(tried: &Result<T, Failed>) -> Outcome {
    match tried {
        Ok(_) => Outcome::Submitted,
        Err(Failed::Refused(e)) => Outcome::Refused {
            reason: format!("{e:#}"),
        },
        Err(Failed::Unconfirmed(Unconfirmed::Empty)) => Outcome::Refused {
            reason: "the message is empty".into(),
        },
        Err(Failed::Unconfirmed(Unconfirmed::Interrupt(_))) => Outcome::Indeterminate {
            reason: "the interrupt was not confirmed".into(),
        },
        Err(Failed::Unconfirmed(Unconfirmed::Submit(_))) => Outcome::Indeterminate {
            reason: "the message was not confirmed".into(),
        },
    }
}

/// Add the attempt to `agent_dir`'s nudge history. Its failure is
/// ignored: the nudge has already happened, or not, either way.
fn record<T>(agent_dir: &std::path::Path, body: &str, tried: &Result<T, Failed>) {
    let _ = crate::cli::nudge_log::append(
        agent_dir,
        &Record {
            at: crate::cli::recovery::unix_now(),
            attempt: crate::cli::nudge_log::manual_attempt(),
            trigger: Trigger::Manual,
            text: body.to_string(),
            outcome: outcome(tried),
        },
    );
}

#[cfg(test)]
mod tests {

    /// The history is written after the attempt, says what its result was,
    /// and cannot change that result, even when it cannot be written.
    #[tokio::test]
    async fn a_nudge_is_recorded_after_its_attempt_and_unchanged_by_it() {
        let dir = tempfile::tempdir().unwrap();
        let journal = crate::cli::nudge_log::path(dir.path());
        let tried = nudge_with(dir.path(), "carry on", async {
            assert!(
                !journal.exists(),
                "nothing is recorded before the attempt ends"
            );
            Ok("pane 7".to_string())
        })
        .await;
        assert_eq!(tried.ok().as_deref(), Some("pane 7"));
        let records = crate::cli::nudge_log::read(dir.path());
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].outcome, Outcome::Submitted);
        assert_eq!(records[0].text, "carry on");

        let unwritable = dir.path().join("not-a-dir");
        std::fs::write(&unwritable, "").unwrap();
        let tried = nudge_with(&unwritable, "x", async {
            Err(Failed::Unconfirmed(Unconfirmed::Submit(anyhow::anyhow!(
                "Enter refused"
            ))))
        })
        .await;
        assert!(matches!(
            tried,
            Err(Failed::Unconfirmed(Unconfirmed::Submit(_)))
        ));
    }

    /// Each way a nudge can end is recorded as what it can claim: a
    /// refusal before the terminal, an unconfirmed interrupt or message as
    /// possibly delivered, and only success as submitted.
    #[test]
    fn each_result_is_recorded_as_what_it_can_claim() {
        let of = |r: Result<(), Failed>| outcome(&r);
        assert_eq!(of(Ok(())), Outcome::Submitted);
        assert!(matches!(
            of(Err(Failed::Refused(anyhow::anyhow!("no pane")))),
            Outcome::Refused { reason } if reason == "no pane"
        ));
        assert!(matches!(
            of(Err(Failed::Unconfirmed(Unconfirmed::Empty))),
            Outcome::Refused { .. }
        ));
        for unconfirmed in [
            Unconfirmed::Interrupt(anyhow::anyhow!("timeout")),
            Unconfirmed::Submit(anyhow::anyhow!("timeout")),
        ] {
            assert!(matches!(
                of(Err(Failed::Unconfirmed(unconfirmed))),
                Outcome::Indeterminate { .. }
            ));
        }
    }

    /// Two nudges are two attempts, even sent at once with the same text.
    #[tokio::test]
    async fn two_nudges_are_two_attempts() {
        let dir = tempfile::tempdir().unwrap();
        for _ in 0..2 {
            let _ = nudge_with(dir.path(), "same", async { Ok(String::new()) }).await;
        }
        assert_eq!(crate::cli::nudge_log::read(dir.path()).len(), 2);
    }
    use super::*;
    use crate::cli::open_zellij::{SessionPane, SessionScan};
    use crate::cli::pane_target::{Found, locate};
    use clank_core::ids::AgentLabel;
    use std::path::Path;

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
