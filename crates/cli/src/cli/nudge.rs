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
//! roster and the live pane listing. Its one write is the agent's nudge history, made
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
    /// Nothing reached the terminal: no zellij, or no one pane named for
    /// the agent.
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
    let target = super::pane_target::discover(repo.to_path_buf(), to.clone())
        .await
        .map_err(Failed::Refused)?;
    let body = crate::cli::pane_input::Admitted::new(body);
    crate::cli::pane_input::interrupt_and_submit_async(target.input(), &body)
        .await
        .map_err(Failed::Unconfirmed)?;
    Ok(format!("session {}, pane {}", target.session, target.pane))
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
        Err(Failed::Unconfirmed(Unconfirmed::Refused(why))) => Outcome::Refused {
            reason: why.clone(),
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

    /// A terminal pane as `list-panes --json --tab` reports one: `title`
    /// in `tab` of `session`, running `command`.
    fn pane(
        session: &str,
        id: u32,
        tab: u32,
        title: &str,
        command: Option<&str>,
        exited: bool,
    ) -> SessionPane {
        SessionPane {
            session: session.into(),
            pane: serde_json::from_value(serde_json::json!({
                "id": id,
                "title": title,
                "terminal_command": command,
                "tab_id": tab,
                "tab_name": format!("tab{tab}"),
                "exited": exited,
            }))
            .unwrap(),
        }
    }

    /// `repo`'s TUI pane: the anchor of its workspace.
    fn tui(session: &str, id: u32, tab: u32, repo: &Path) -> SessionPane {
        let command = format!("clank tui --repo {}", repo.display());
        pane(session, id, tab, "", Some(&command), false)
    }

    fn named(session: &str, id: u32, tab: u32, title: &str, exited: bool) -> SessionPane {
        pane(session, id, tab, title, None, exited)
    }

    fn scan(panes: Vec<SessionPane>, unanswered: &[&str]) -> SessionScan {
        SessionScan {
            panes,
            unanswered: unanswered.iter().map(|s| s.to_string()).collect(),
            listing_failed: false,
        }
    }

    fn candidates(labels: &[&str]) -> std::collections::BTreeSet<String> {
        labels.iter().map(|l| l.to_string()).collect()
    }

    fn find(scan: &SessionScan, name: &str) -> anyhow::Result<Found> {
        locate(scan, repo(), &label(name), candidates(&["claude", "rev"]))
    }

    /// `clank open` inside an existing session puts the repo's tab in THAT
    /// session, so the conventional `clank-<basename>` name is not where
    /// it is: the TUI pane says where.
    #[test]
    fn a_pane_is_found_in_a_session_the_convention_does_not_name() {
        let found = find(
            &scan(
                vec![
                    tui("someones-own-session", 1, 0, repo()),
                    named("someones-own-session", 3, 0, "👀 rev", false),
                ],
                &[],
            ),
            "rev",
        )
        .expect("a live pane named for the agent is a target wherever it lives");
        assert_eq!(
            found,
            Found {
                session: "someones-own-session".into(),
                pane: "terminal_3".into(),
            }
        );
    }

    /// A pane is its name: one started by hand as `rev` is found, and one
    /// running rev's exact launch command under another name is not.
    #[test]
    fn a_pane_is_its_name_not_its_command() {
        let launch = format!("clank agent start rev --repo {}", repo().display());
        let renamed = scan(
            vec![
                tui("s", 1, 0, repo()),
                pane("s", 3, 0, "editor", Some(&launch), false),
            ],
            &[],
        );
        let err = find(&renamed, "rev").unwrap_err().to_string();
        assert!(err.contains("no pane named `rev`"), "{err}");
        let by_hand = scan(
            vec![
                tui("s", 1, 0, repo()),
                pane("s", 4, 0, "rev", Some("clank agent start rev"), false),
            ],
            &[],
        );
        assert_eq!(find(&by_hand, "rev").unwrap().pane, "terminal_4");
    }

    /// One session holds two repos' tabs, each with a `rev`: each repo
    /// finds only the `rev` in its own tab.
    #[test]
    fn two_repos_in_one_session_each_find_their_own_pane() {
        let other = Path::new("/w/other");
        let both = scan(
            vec![
                tui("shared", 1, 0, repo()),
                named("shared", 2, 0, "rev", false),
                tui("shared", 5, 1, other),
                named("shared", 6, 1, "rev", false),
            ],
            &[],
        );
        assert_eq!(find(&both, "rev").unwrap().pane, "terminal_2");
        let theirs = locate(&both, other, &label("rev"), candidates(&["rev"])).unwrap();
        assert_eq!(theirs.pane, "terminal_6");
    }

    /// An exited pane is still the agent's, so it is named, not typed into:
    /// zellij would accept the keystrokes anyway.
    #[test]
    fn a_corpse_is_refused_and_named() {
        let err = find(
            &scan(
                vec![
                    tui("clank-proj", 1, 0, repo()),
                    named("clank-proj", 3, 0, "rev", true),
                ],
                &[],
            ),
            "rev",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("EXITED"), "{err}");
        assert!(err.contains("clank-proj"), "says where: {err}");
    }

    #[test]
    fn two_live_panes_with_one_name_refuse_rather_than_pick() {
        let err = find(
            &scan(
                vec![
                    tui("clank-proj", 1, 0, repo()),
                    named("clank-proj", 3, 0, "rev", false),
                    named("clank-proj", 9, 0, "💤 rev", false),
                ],
                &[],
            ),
            "rev",
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("terminal_3") && err.contains("terminal_9"),
            "{err}"
        );
    }

    /// A title that reads as two candidates is refused as unknowable, not
    /// read as bob being absent: it names the pane and what it reads as.
    #[test]
    fn an_ambiguous_title_is_no_target() {
        let s = scan(
            vec![tui("s", 1, 0, repo()), named("s", 3, 0, "💤 bob", false)],
            &[],
        );
        let err = locate(&s, repo(), &label("bob"), candidates(&["bob", "💤 bob"]))
            .unwrap_err()
            .to_string();
        assert!(!err.contains("no pane named"), "{err}");
        assert!(
            err.contains("cannot be known")
                && err.contains("terminal_3")
                && err.contains("`💤 bob`"),
            "{err}"
        );
    }

    /// A plainly-bob pane beside one that may also be bob is not a unique
    /// target: the second may be another copy.
    #[test]
    fn a_unique_match_beside_an_ambiguous_pane_is_refused() {
        let s = scan(
            vec![
                tui("s", 1, 0, repo()),
                named("s", 3, 0, "bob", false),
                named("s", 4, 0, "💤 bob", false),
            ],
            &[],
        );
        let err = locate(&s, repo(), &label("bob"), candidates(&["bob", "💤 bob"]))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("terminal_4") && err.contains("nothing was typed"),
            "{err}"
        );
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
        let err = find(&blind, "rev").unwrap_err().to_string();
        assert!(err.contains("not even whether there is one"), "{err}");
        assert!(
            !err.contains("no pane named"),
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

    /// A silent session could hold this repo's other tab, so one anchor
    /// and one match are not enough to act on.
    #[test]
    fn one_workspace_beside_a_silent_session_still_refuses() {
        let err = find(
            &scan(
                vec![
                    tui("clank-proj", 1, 0, repo()),
                    named("clank-proj", 3, 0, "rev", false),
                ],
                &["stale"],
            ),
            "rev",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("stale"), "names what went unread: {err}");
        assert!(err.contains("nothing was typed"), "{err}");
    }

    /// No TUI pane, or two, is no workspace: a pane named `rev` is then
    /// nobody's target, and the refusal is not "unanswered".
    #[test]
    fn no_anchor_or_two_anchors_is_no_workspace() {
        let none = find(&scan(vec![named("s", 3, 0, "rev", false)], &[]), "rev")
            .unwrap_err()
            .to_string();
        assert!(
            none.contains("no session shows this repo's TUI pane"),
            "{none}"
        );
        assert!(!none.contains("did not answer"), "{none}");
        let two = find(
            &scan(
                vec![
                    tui("a", 1, 0, repo()),
                    named("a", 3, 0, "rev", false),
                    tui("b", 1, 0, repo()),
                ],
                &[],
            ),
            "rev",
        )
        .unwrap_err()
        .to_string();
        assert!(two.contains("2 places"), "{two}");
    }

    /// "Nothing answered" must not read as "the agent is not there".
    #[test]
    fn nothing_found_reads_differently_when_a_session_went_unread() {
        let quiet = find(&scan(vec![tui("s", 1, 0, repo())], &[]), "rev")
            .unwrap_err()
            .to_string();
        let unread = find(&scan(Vec::new(), &["stale"]), "rev")
            .unwrap_err()
            .to_string();
        assert!(quiet.contains("no pane named"), "{quiet}");
        assert!(
            !quiet.contains("did not answer"),
            "nothing was missed here: {quiet}"
        );
        assert!(unread.contains("did not answer"), "{unread}");
    }
}
