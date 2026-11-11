//! `clank run --desc "two words" -- <cmd>` — run a command under clank
//! so it knows exactly when the work ends.
//!
//! Clank registers this run with the monitor, and then `exec`s the
//! command, replacing itself. One process does both jobs, which is what
//! keeps every property aligned: the pid in the registration is the pid
//! the work runs under, so the run ends exactly when the work does, and
//! stdio, exit status and signals are the command's own because nothing
//! is wrapping them. The agent reads output and status wherever it
//! launched the run; clank captures neither.
//!
//! A spawn-and-wait supervisor would break the middle one. Killed while
//! its child kept running, the registered process would be gone with the
//! work still going — liveness lying in the direction that puts an agent
//! to sleep.

use super::{RunArgs, resolve_repo};
use crate::cli::monitor::Control;

/// `exec` is the whole design — same pid, same stdio, same exit status —
/// and it has no portable equivalent.
#[cfg(not(unix))]
pub async fn run(_args: RunArgs) -> anyhow::Result<()> {
    anyhow::bail!(
        "`clank run` needs `exec` to replace itself with the command, which this platform does \
         not provide."
    )
}

#[cfg(unix)]
pub async fn run(args: RunArgs) -> anyhow::Result<()> {
    use crate::proc_run::{Out, Spec, Stdin};
    let repo = resolve_repo(args.repo.as_deref())?;
    let (register, clear) = requests(&args, &repo)?;
    let (program, rest) = args
        .command
        .split_first()
        .expect("clap requires at least one");
    let submit = |control: Control| -> Submitted<'_> {
        Box::pin(crate::cli::attend::submit(&repo, control))
    };
    let not_run = launch(register, clear, &submit, || {
        crate::proc_run::exec(
            Spec::new(program)
                .args(rest)
                .stdin(Stdin::Inherit)
                .stdout(Out::Inherit)
                .stderr(Out::Inherit),
        )
    })
    .await;
    Err(match not_run {
        NotRun::Unattended(why) => anyhow::anyhow!(
            "{why}; `{program}` was not run. Retry, or run it some other way knowing clank \
             will not attend it"
        ),
        NotRun::Refused(why) => anyhow::anyhow!("{why}; `{program}` was not run"),
        NotRun::Exec(err) => anyhow::anyhow!("cannot run `{program}`: {err}"),
    })
}

/// A request on its way to the monitor, and its answer.
type Submitted<'a> = std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<String, crate::cli::attend::NotDone>> + 'a>,
>;

/// Why the command is not running.
#[derive(Debug)]
enum NotRun {
    /// No monitor took the registration, so nothing could attend the run,
    /// and the command was never run.
    Unattended(crate::cli::attend::NotDone),
    /// The monitor refused the registration, so the command was never run.
    Refused(crate::cli::attend::NotDone),
    Exec(std::io::Error),
}

/// Register, then become the command. Only a FAILED exec returns once the
/// command was tried, and then nothing is running: the registration is
/// taken back by its own process proof, so nothing else the agent attends
/// is touched. A command runs only once a monitor holds its registration:
/// only a monitor can attend it.
async fn launch<'a>(
    register: Control,
    clear: Control,
    submit: &'a dyn Fn(Control) -> Submitted<'a>,
    exec: impl FnOnce() -> std::io::Error,
) -> NotRun {
    match submit(register).await {
        Ok(_) => {}
        Err(why @ crate::cli::attend::NotDone::NotTaken(_)) => return NotRun::Unattended(why),
        Err(refused) => return NotRun::Refused(refused),
    }
    let err = exec();
    if let Err(why) = submit(clear).await {
        eprintln!("clank: the failed run's registration could not be taken back: {why}");
    }
    NotRun::Exec(err)
}

/// The requests a run makes: to register it, and to take back exactly
/// that registration.
///
/// The process identity is THIS process's, and it stays correct across
/// the exec: `exec` replaces the process image, not the process.
fn requests(args: &RunArgs, repo: &std::path::Path) -> anyhow::Result<(Control, Control)> {
    let label = match args.author.as_deref() {
        Some(raw) => clank_core::ids::AgentLabel::parse(raw)
            .map_err(|e| anyhow::anyhow!("invalid --author `{raw}`: {e}"))?,
        None => crate::agent_env::resolve_identity_from_env(repo)?,
    };
    let desc = crate::cli::attend::normalise_desc(&args.desc);
    if desc.is_empty() {
        anyhow::bail!(
            "--desc is empty; give TWO WORDS for what you are running, e.g. \
             `--desc \"test run\"`"
        );
    }
    let pid = std::process::id() as i32;
    let Some(token) = crate::proc_identity::token_for(pid) else {
        anyhow::bail!(
            "cannot take a process identity for pid {pid} on this platform, so nothing could \
             prove this run is still going"
        );
    };
    let session = crate::cli::attend::session_of(repo, &label)?;
    let proof = crate::cli::turn_custody::shell_proof(pid, &token);
    Ok((
        Control::Run {
            author: label.to_string(),
            session: session.clone(),
            desc,
            expect_ms: args.expect.as_millis() as u64,
            proof: proof.clone(),
        },
        Control::Clear {
            author: label.to_string(),
            session,
            which: clank_core::turn_policy::Clearing::Run(proof),
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser, Debug)]
    struct T {
        #[command(flatten)]
        a: RunArgs,
    }

    #[test]
    fn a_description_and_a_command_are_both_required() {
        let t = T::try_parse_from(["t", "--desc", "test run", "--", "echo", "hi"]).unwrap();
        assert_eq!(
            t.a.expect.as_secs(),
            300,
            "five minutes unless the caller says otherwise"
        );
        let t = T::try_parse_from(["t", "--desc", "test run", "--expect", "45m", "--", "make"])
            .unwrap();
        assert_eq!(t.a.expect, std::time::Duration::from_secs(2700));
        // The description is what the run is shown, renewed and cleared by.
        assert!(
            T::try_parse_from(["t", "--", "echo", "hi"]).is_err(),
            "--desc required"
        );
        assert!(
            T::try_parse_from(["t", "--desc", "test run"]).is_err(),
            "a command is required — there is nothing to attend otherwise"
        );
    }

    /// Everything after `--` belongs to the command, flags included.
    /// Without this a run of `cargo test --release` would have its
    /// flags eaten by clank's own parser.
    #[test]
    fn the_command_keeps_its_own_flags() {
        let t = T::try_parse_from([
            "t",
            "--desc",
            "test run",
            "--",
            "cargo",
            "test",
            "--release",
            "--desc",
            "not-ours",
        ])
        .expect("parses");
        assert_eq!(
            t.a.command,
            vec!["cargo", "test", "--release", "--desc", "not-ours"]
        );
        assert_eq!(
            t.a.desc, "test run",
            "clank's own --desc is the one before --"
        );
    }

    fn bound_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        crate::agent_store::bind_session_to_agent(
            dir.path(),
            &clank_core::ids::AgentLabel::parse("claude").unwrap(),
            clank_core::Tool::Claude,
            &clank_core::SessionId::parse("sess-run").unwrap(),
        )
        .unwrap();
        dir
    }

    fn run_args(desc: &str, repo: &std::path::Path) -> RunArgs {
        RunArgs {
            desc: desc.to_string(),
            expect: std::time::Duration::from_secs(300),
            author: Some("claude".into()),
            repo: Some(repo.to_path_buf()),
            command: vec!["echo".into()],
        }
    }

    /// The registration names THIS process — the pid the command will run
    /// under, because `exec` replaces the image and not the process — with
    /// the identity that proves it.
    #[test]
    fn the_request_names_this_process_and_its_description() {
        let dir = bound_repo();
        let (register, clear) =
            requests(&run_args("  test run  ", dir.path()), dir.path()).unwrap();
        let Control::Run {
            desc,
            expect_ms,
            proof: shell,
            session,
            ..
        } = register
        else {
            panic!("{register:?}");
        };
        assert_eq!(desc, "test run");
        assert_eq!(expect_ms, 300_000);
        assert_eq!(session.as_str(), "sess-run");
        assert_eq!(shell.pid, std::process::id() as i32);
        assert_eq!(
            clear,
            Control::Clear {
                author: "claude".into(),
                session,
                which: clank_core::turn_policy::Clearing::Run(shell),
            },
            "the rollback names this run's entry and no other"
        );
    }

    #[test]
    fn a_blank_description_asks_for_nothing() {
        let dir = bound_repo();
        assert!(requests(&run_args("   ", dir.path()), dir.path()).is_err());
    }

    fn run_controls() -> (Control, Control) {
        let session = clank_core::SessionId::parse("sess-run").unwrap();
        let proof = clank_core::turn_policy::ShellProof {
            pid: 7003,
            token: "mac1:7003:1".into(),
        };
        (
            Control::Run {
                author: "claude".into(),
                session: session.clone(),
                desc: "test run".into(),
                expect_ms: 1,
                proof: proof.clone(),
            },
            Control::Clear {
                author: "claude".into(),
                session,
                which: clank_core::turn_policy::Clearing::Run(proof),
            },
        )
    }

    /// Drive `launch` against a monitor answering the registration with
    /// `answer`: what was sent to it, whether the command was tried, and
    /// why it is not running.
    async fn launched(
        answer: Result<String, crate::cli::attend::NotDone>,
    ) -> (Vec<Control>, bool, NotRun) {
        let sent = std::cell::RefCell::new(Vec::new());
        let execs = std::cell::Cell::new(false);
        let submit = |c: Control| -> Submitted<'_> {
            let first = sent.borrow().is_empty();
            sent.borrow_mut().push(c);
            let reply = if first {
                answer.clone()
            } else {
                Ok("cleared".to_string())
            };
            Box::pin(async move { reply })
        };
        let (attend, clear) = run_controls();
        let not_run = launch(attend, clear, &submit, || {
            execs.set(true);
            std::io::Error::from(std::io::ErrorKind::NotFound)
        })
        .await;
        (sent.into_inner(), execs.get(), not_run)
    }

    /// A command that cannot start takes back exactly the entry it
    /// registered, by its process proof, and the error names the exec.
    #[tokio::test]
    async fn a_failed_exec_clears_only_the_entry_it_registered() {
        let (attend, clear) = run_controls();
        let (sent, execed, not_run) = launched(Ok("attending".into())).await;
        assert!(execed);
        assert!(
            matches!(&not_run, NotRun::Exec(e) if e.kind() == std::io::ErrorKind::NotFound),
            "{not_run:?}"
        );
        assert_eq!(sent, vec![attend, clear]);
    }

    /// No monitor took the registration: the command is refused and never
    /// runs, since nothing could attend it. A monitor's refusal stops
    /// it the same way.
    #[tokio::test]
    async fn without_a_monitor_the_command_is_refused_and_never_runs() {
        let (sent, execed, not_run) = launched(Err(crate::cli::attend::NotDone::NotTaken(
            "no monitor".into(),
        )))
        .await;
        assert!(!execed, "the command is not run");
        assert!(matches!(not_run, NotRun::Unattended(_)), "{not_run:?}");
        assert_eq!(sent.len(), 1, "nothing was registered, so nothing to clear");

        let (sent, execed, not_run) = launched(Err(crate::cli::attend::NotDone::Refused(
            "this session no longer holds the label".into(),
        )))
        .await;
        assert!(!execed, "a monitor's refusal stops the run");
        assert!(matches!(not_run, NotRun::Refused(_)), "{not_run:?}");
        assert_eq!(sent.len(), 1);
    }
}
