//! `-m -` takes the message from stdin, the way `-F -` already does.
//!
//! Agents compose messages in heredocs. Without this, a body carrying
//! backticks, `$(…)` or quotes has to survive a shell round-trip, and
//! the shapes that do not survive it are exactly the ones worth
//! sending: reviews, plan bodies, commit rationale. `clank send -m -`
//! used to deliver a literal "-", losing two messages before anyone
//! noticed — silently, because "-" is a valid message to an inbox.
//!
//! ONE pass, before dispatch, rather than a call in each command's
//! `run`. Seven commands take a free-text `-m` and they disagree about
//! emptiness — `block create` writes the text with no check at all,
//! while `feedback write` accepts an empty body as a verdict-only
//! note. Substitution is therefore all that happens here; what a
//! command already does with the text it is handed, it keeps doing.

use crate::cli::command::Command;
use crate::cli::{
    BlockArgs, BlockCmd, FeedbackArgs, FeedbackCmd, PrReviewArgs, PrReviewCmd, QueueArgs, QueueCmd,
};

/// The `-m` of the command being run, in the three shapes clap parses
/// it into.
enum Field<'a> {
    Many(&'a mut Vec<String>),
    One(&'a mut String),
    Maybe(&'a mut Option<String>),
}

/// Where the free-text `-m` lives for each command that has one.
///
/// Adding an eighth is meant to land here; `every_free_text_m_is_wired`
/// fails if one is declared and not routed.
fn field_of(cmd: &mut Command) -> Option<Field<'_>> {
    Some(match cmd {
        Command::Send(a) => Field::Many(&mut a.message),
        Command::Nudge(a) => Field::Many(&mut a.message),
        Command::Finish(a) => Field::Many(&mut a.message),
        Command::Unblock(a) => Field::One(&mut a.message),
        Command::Queue(QueueArgs {
            command: Some(QueueCmd::Add(a)),
            ..
        }) => Field::Maybe(&mut a.message),
        Command::Feedback(FeedbackArgs {
            command: FeedbackCmd::Write(a),
        }) => Field::One(&mut a.message),
        Command::PrReview(PrReviewArgs {
            command: PrReviewCmd::Note(a),
            ..
        }) => Field::One(&mut a.message),
        Command::Block(BlockArgs {
            command: BlockCmd::Create(a),
        }) => Field::One(&mut a.message),
        _ => return None,
    })
}

fn is_dash(value: &str) -> bool {
    value == "-"
}

/// `values` with a lone `-` replaced by what `read` yields. Stdin is
/// read once, and only when a `-` is present.
fn substitute(
    values: &[String],
    read: impl FnOnce() -> std::io::Result<String>,
) -> anyhow::Result<Vec<String>> {
    let dashes = values.iter().filter(|v| is_dash(v)).count();
    if dashes == 0 {
        return Ok(values.to_vec());
    }
    if dashes > 1 {
        anyhow::bail!(
            "`-m -` more than once: stdin can be read only once, so the second `-` has \
             nothing to read"
        );
    }
    let text = read().map_err(|e| anyhow::anyhow!("reading the message from stdin: {e}"))?;
    Ok(values
        .iter()
        .map(|v| if is_dash(v) { text.clone() } else { v.clone() })
        .collect())
}

pub(crate) fn resolve_with(
    cmd: &mut Command,
    read: impl FnOnce() -> std::io::Result<String>,
) -> anyhow::Result<()> {
    let Some(field) = field_of(cmd) else {
        return Ok(());
    };
    match field {
        Field::Many(values) => *values = substitute(&values.clone(), read)?,
        Field::One(value) => {
            *value = one(substitute(std::slice::from_ref(&value.clone()), read)?);
        }
        Field::Maybe(Some(value)) => {
            *value = one(substitute(std::slice::from_ref(&value.clone()), read)?);
        }
        Field::Maybe(None) => {}
    }
    Ok(())
}

fn one(values: Vec<String>) -> String {
    values.into_iter().next().expect("one in, one out")
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn parse(args: &[&str]) -> Command {
        crate::cli::command::Cli::try_parse_from(
            std::iter::once("clank").chain(args.iter().copied()),
        )
        .expect("argv parses")
        .command
    }

    /// Resolve `args` against a canned stdin and read the `-m` back.
    fn resolved(args: &[&str], stdin: &str) -> Vec<String> {
        let mut cmd = parse(args);
        resolve_with(&mut cmd, || Ok(stdin.to_string())).expect("resolves");
        match field_of(&mut cmd) {
            Some(Field::Many(v)) => v.clone(),
            Some(Field::One(s)) => vec![s.clone()],
            Some(Field::Maybe(o)) => o.clone().into_iter().collect(),
            None => panic!("{args:?} has no -m"),
        }
    }

    /// The wiring, per command — a helper test would pass against a
    /// dispatch that never called it.
    #[test]
    fn every_command_with_a_free_text_m_reads_stdin_for_a_dash() {
        let body = "BODY";
        for args in [
            vec!["send", "--master", "-m", "-"],
            vec!["nudge", "--master", "-m", "-"],
            vec!["finish", "-m", "-"],
            vec!["queue", "add", "some-plan", "-m", "-"],
            vec![
                "feedback",
                "write",
                "--commit",
                "abc1234",
                "--verdict",
                "continue",
                "--author",
                "rev",
                "-m",
                "-",
            ],
            vec!["pr-review", "note", "--verdict", "continue", "-m", "-"],
            vec!["block", "create", "q", "-m", "-"],
            vec!["unblock", "rev", "q", "-m", "-"],
        ] {
            assert_eq!(
                resolved(&args, body),
                vec![body.to_string()],
                "{args:?} did not take its message from stdin"
            );
        }
    }

    /// The tripwire for an eighth. `field_of` has a wildcard arm, so a
    /// new `-m` would silently keep sending a literal "-" — which is
    /// the exact bug this plan fixes.
    #[test]
    fn every_free_text_m_is_wired() {
        let declared = include_str!("mod.rs").matches("short = 'm'").count();
        assert_eq!(
            declared, 8,
            "a `-m` was added or removed in mod.rs; route it in `field_of` and list it in \
             `every_command_with_a_free_text_m_reads_stdin_for_a_dash`, then update this count"
        );
    }

    /// The regression, end to end: `clank send -m -` through the real
    /// dispatch, and the DELIVERED body read back byte for byte.
    ///
    /// Every other test here stops at the parsed args. This one is the
    /// path that silently delivered a literal "-", so it runs
    /// `dispatch_with` -> `send::run` -> `inbox::deliver` and reads the
    /// inbox. In process, not a subprocess: spawning the clank binary
    /// in tests is banned in this repo.
    #[tokio::test]
    async fn a_heredoc_body_survives_dispatch_into_the_inbox() {
        let repo = tempfile::tempdir().unwrap();
        assert!(
            std::process::Command::new("git")
                .arg("-C")
                .arg(repo.path())
                .args(["init", "--quiet", "--initial-branch=main"])
                .status()
                .unwrap()
                .success()
        );
        std::fs::create_dir_all(repo.path().join(".clank")).unwrap();
        std::fs::write(
            repo.path().join(".clank/config.json"),
            r#"{"agents":{"claude":{"tool":"claude","role":"master"}}}"#,
        )
        .unwrap();

        let raw = "`backticks` and $(cmd) and \"quotes\" and 'single'\n\nsecond paragraph\n";
        let cli = crate::cli::command::Cli::try_parse_from([
            "clank",
            "send",
            "--repo",
            repo.path().to_str().unwrap(),
            "--master",
            "-m",
            "-",
        ])
        .expect("argv parses");
        crate::cli::command::dispatch_with(cli, || Ok(raw.to_string()))
            .await
            .expect("the send runs");

        let to = clank_core::ids::AgentLabel::parse("claude").unwrap();
        let here = dunce::canonicalize(repo.path()).unwrap();
        let pending = crate::inbox::pending(&here, &to);
        assert_eq!(pending.len(), 1, "one message was sent");
        let clank_core::wait::WaitItem::Message { body, .. } = &pending[0] else {
            panic!("a send delivers a message: {pending:?}");
        };
        assert_eq!(
            body, raw,
            "the delivered body must be what stdin held, byte for byte"
        );
        assert_ne!(body, "-", "the bug: a literal dash was delivered");
    }

    #[test]
    fn a_dash_becomes_stdin_in_its_own_position() {
        assert_eq!(
            resolved(
                &["send", "--master", "-m", "one", "-m", "-", "-m", "three"],
                "TWO"
            ),
            vec!["one", "TWO", "three"]
        );
    }

    #[test]
    fn stdin_is_not_read_without_a_dash() {
        let mut cmd = parse(&["send", "--master", "-m", "hi"]);
        resolve_with(&mut cmd, || panic!("stdin must not be read without a `-`")).unwrap();
    }

    /// `-m -` and `-F -` would both want stdin. clap's `body` group
    /// already makes them exclusive; pin it, because the pass above
    /// assumes only one of the two can ask.
    #[test]
    fn a_message_and_a_file_cannot_both_claim_stdin() {
        let parsed = crate::cli::command::Cli::try_parse_from([
            "clank", "send", "--master", "-m", "-", "-F", "-",
        ]);
        assert!(parsed.is_err(), "-m - and -F - must not parse together");
    }

    #[test]
    fn a_second_dash_is_refused_because_stdin_is_consumed() {
        let mut cmd = parse(&["send", "--master", "-m", "-", "-m", "-"]);
        let err = resolve_with(&mut cmd, || Ok("BODY".into())).unwrap_err();
        assert!(err.to_string().contains("only once"), "{err}");
    }

    /// The reason the change exists: a heredoc body survives byte for
    /// byte, including what a shell would otherwise eat.
    #[test]
    fn a_shell_hostile_body_round_trips() {
        let raw = "`backticks` $(cmd) \"quotes\" 'single' \\slash\n\ntrailing\n";
        assert_eq!(resolved(&["send", "--master", "-m", "-"], raw), vec![raw]);
    }

    /// Only an exact `-` asks for stdin.
    #[test]
    fn a_message_that_merely_contains_a_dash_is_a_message() {
        let mut cmd = parse(&["send", "--master", "-m", "a-b", "-m", "x-"]);
        resolve_with(&mut cmd, || panic!("neither is a lone dash")).unwrap();
    }

    /// Emptiness is each command's own question: `block create` writes
    /// the text unchecked, so substituting empty here must not become a
    /// new rejection that changes what an empty `-m ""` already does.
    #[test]
    fn empty_stdin_substitutes_empty_rather_than_failing_here() {
        assert_eq!(resolved(&["block", "create", "q", "-m", "-"], ""), vec![""]);
    }

    /// `queue add` with no `-m` reads the drafts dir, and that must
    /// survive the pass untouched.
    #[test]
    fn an_absent_message_stays_absent() {
        let mut cmd = parse(&["queue", "add", "some-plan"]);
        resolve_with(&mut cmd, || panic!("nothing asked for stdin")).unwrap();
        assert!(matches!(field_of(&mut cmd), Some(Field::Maybe(None))));
    }
}
