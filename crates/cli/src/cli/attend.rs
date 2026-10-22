//! `clank attend <name>` — tell the monitor which background work this
//! agent is waiting on, so work it has already been shown does not wake it
//! again while that work runs. New work still wakes it.
//!
//! The request is an INTENT. The monitor resolves it against the next
//! turn-end's listing of live background work: a subagent by its name, or
//! shell work by its task id with the `--pid` of its process (or, simpler,
//! work launched through `clank run`). Until a listing shows exactly one
//! live match, nothing is quiet. Quiet lasts only while the work is listed
//! running, its process (for shell work) still holds its identity, and the
//! `--expect` deadline has not passed; at that deadline the agent is
//! checked in on once. `clank attending` is the old spelling.

use super::{AttendArgs, resolve_repo};
use crate::cli::monitor::Control;

pub async fn run(args: AttendArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    let label = match args.author.as_deref() {
        Some(raw) => clank_core::ids::AgentLabel::parse(raw)
            .map_err(|e| anyhow::anyhow!("invalid --author `{raw}`: {e}"))?,
        None => crate::agent_env::resolve_identity_from_env(&repo)?,
    };
    let session = session_of(&repo, &label)?;
    let control = request(&args, &label, &session)?;
    let said = submit(&repo, control)
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    println!("{said}");
    Ok(())
}

/// The session the request speaks for: this process's own, or the
/// label's binding when it runs outside one.
pub(crate) fn session_of(
    repo: &std::path::Path,
    label: &clank_core::ids::AgentLabel,
) -> anyhow::Result<clank_core::SessionId> {
    if let Some((_, session)) = crate::agent_env::detect_session_from_env()? {
        return Ok(session);
    }
    crate::agent_store::load_agent_config(repo, label)?
        .and_then(|c| c.session)
        .map(|s| s.id)
        .ok_or_else(|| {
            anyhow::anyhow!("no session is bound to `{label}`, so there is nothing to attend for")
        })
}

/// The request `args` make for `label`'s `session`, or why it cannot be
/// made. Shell work names its task id and the process that runs it: a pid
/// alone is reused, so its identity is taken now, while it is known to be
/// the process meant.
pub(crate) fn request(
    args: &AttendArgs,
    label: &clank_core::ids::AgentLabel,
    session: &clank_core::SessionId,
) -> anyhow::Result<Control> {
    let author = label.to_string();
    if args.clear {
        return Ok(Control::Clear {
            author,
            session: session.clone(),
        });
    }
    let Some(name) = args.selector.as_deref().map(str::trim) else {
        anyhow::bail!("name the background work to attend, or `--clear` to stop attending");
    };
    if name.is_empty() {
        anyhow::bail!("the name to attend is empty");
    }
    let desc = args
        .desc
        .as_deref()
        .map(normalise_desc)
        .filter(|d| !d.is_empty());
    let (selector, shell) = match args.pid {
        None => (
            clank_core::turn_policy::Selector::Name(name.to_string()),
            None,
        ),
        Some(pid) => {
            if pid <= 0 {
                anyhow::bail!("--pid must be a real process id");
            }
            let Some(token) = crate::proc_identity::token_for(pid) else {
                anyhow::bail!(
                    "cannot identify pid {pid}: it may have already exited, or this platform \
                     cannot answer. A pid alone is not an identity — the OS reuses them — so it \
                     would prove nothing."
                );
            };
            if desc.is_none() {
                anyhow::bail!(
                    "--desc is empty; give TWO WORDS for what you are waiting on, e.g. \
                     `--desc \"test run\"`"
                );
            }
            (
                clank_core::turn_policy::Selector::TaskId(name.to_string()),
                Some(crate::cli::turn_custody::shell_proof(pid, &token)),
            )
        }
    };
    Ok(Control::Attend {
        author,
        session: session.clone(),
        desc: desc.or_else(|| match &selector {
            clank_core::turn_policy::Selector::Name(name) => Some(name.clone()),
            _ => None,
        }),
        selector,
        expect_ms: args.expect.as_millis() as u64,
        shell,
    })
}

/// Hand `control` to the repo's monitor, starting one if none is running,
/// and give its answer.
pub(crate) async fn submit(repo: &std::path::Path, control: Control) -> Result<String, String> {
    let attached = crate::cli::monitor::attach_or_spawn_with(
        repo,
        crate::cli::monitor::Who::Control { request: control },
        crate::cli::monitor::spawn_monitor,
    )
    .await;
    let mut conn = match attached {
        crate::cli::monitor::Attached::Shared(conn) => conn,
        crate::cli::monitor::Attached::Alone(why) => {
            return Err(format!(
                "no monitor took the request ({why}), so nothing was recorded"
            ));
        }
    };
    let deadline = std::time::Instant::now() + crate::cli::monitor::ATTACH_BUDGET;
    match conn.note(deadline) {
        Ok(crate::cli::monitor::Note::Done { what }) => Ok(what),
        Ok(crate::cli::monitor::Note::Refused { why }) => Err(why),
        Ok(other) => Err(format!("the monitor answered {other:?}")),
        Err(e) => Err(format!("the monitor did not answer: {e}")),
    }
}

/// The value of `--expect`: clank's one duration grammar, and never
/// none. `0` reads as "no bound" in that grammar, and there is no such
/// attendance — an agent that wants to sleep through a hang can say
/// `24h` and be honest about it.
pub(crate) fn parse_expect(raw: &str) -> Result<std::time::Duration, String> {
    match crate::cli::wait::parse_duration_str(raw) {
        Ok(Some(d)) => Ok(d),
        Ok(None) => Err(
            "there is always a check-in; give a longer expectation instead (e.g. `--expect 2h`)"
                .into(),
        ),
        Err(e) => Err(e.to_string()),
    }
}

/// `--expect`'s own form of a duration, for messages that name one.
pub(crate) fn duration_arg(secs: u64) -> String {
    if secs > 0 && secs.is_multiple_of(3600) {
        format!("{}h", secs / 3600)
    } else if secs > 0 && secs.is_multiple_of(60) {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

/// Collapse a description to the single terminal row it will be drawn
/// as: first line only, control bytes dropped, trimmed.
pub(crate) fn normalise_desc(raw: &str) -> String {
    raw.lines()
        .next()
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clank_core::turn_policy::Selector;

    fn label() -> clank_core::ids::AgentLabel {
        clank_core::ids::AgentLabel::parse("claude").unwrap()
    }

    fn session() -> clank_core::SessionId {
        clank_core::SessionId::parse("sess-attend").unwrap()
    }

    fn args(name: Option<&str>, desc: Option<&str>) -> AttendArgs {
        AttendArgs {
            selector: name.map(str::to_string),
            pid: None,
            expect: std::time::Duration::from_secs(300),
            desc: desc.map(str::to_string),
            clear: false,
            author: Some("claude".into()),
            repo: None,
        }
    }

    fn me() -> i32 {
        std::process::id() as i32
    }

    fn attend(
        control: Control,
    ) -> (
        Selector,
        Option<String>,
        u64,
        Option<clank_core::turn_policy::ShellProof>,
    ) {
        match control {
            Control::Attend {
                selector,
                desc,
                expect_ms,
                shell,
                ..
            } => (selector, desc, expect_ms, shell),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_subagent_is_attended_by_its_name_with_no_pid() {
        let (selector, desc, expect, shell) = attend(
            request(
                &args(Some("capture-probe sleeper"), None),
                &label(),
                &session(),
            )
            .unwrap(),
        );
        assert_eq!(selector, Selector::Name("capture-probe sleeper".into()));
        assert_eq!(
            desc.as_deref(),
            Some("capture-probe sleeper"),
            "its name is its subject"
        );
        assert_eq!(expect, 300_000);
        assert_eq!(shell, None);
    }

    #[test]
    fn the_old_spelling_and_its_flags_still_parse() {
        use clap::Parser;
        let cli = crate::cli::command::Cli::try_parse_from([
            "clank",
            "attending",
            "br9711ewy",
            "--desc",
            "test run",
            "--pid",
            "1",
        ])
        .expect("the hidden alias parses");
        let crate::cli::command::Command::Attend(a) = cli.command else {
            panic!("not attend");
        };
        assert_eq!(a.selector.as_deref(), Some("br9711ewy"));
        assert_eq!(a.pid, Some(1));
    }

    /// Shell work names its task id and the process it runs as, with the
    /// identity taken now: the shape `clank attending <task> --desc --pid`
    /// always had.
    #[test]
    fn shell_work_is_attended_by_task_id_with_its_process_identity() {
        let (selector, desc, _, shell) = attend(
            request(
                &AttendArgs {
                    pid: Some(me()),
                    ..args(Some("br9711ewy"), Some("  test run  "))
                },
                &label(),
                &session(),
            )
            .unwrap(),
        );
        assert_eq!(selector, Selector::TaskId("br9711ewy".into()));
        assert_eq!(desc.as_deref(), Some("test run"), "normalised, not raw");
        let shell = shell.expect("the process proof rides along");
        assert_eq!(shell.pid, me());
        let token: crate::proc_identity::ProcToken = serde_json::from_str(&shell.token).unwrap();
        assert!(token.still_holds(me()));
    }

    #[test]
    fn shell_work_needs_a_description_that_survives_normalising() {
        for blank in [
            None,
            Some("   "),
            Some("\u{7}\u{1b}"),
            Some("\n first line lost"),
        ] {
            let err = request(
                &AttendArgs {
                    pid: Some(me()),
                    ..args(Some("br9711ewy"), blank)
                },
                &label(),
                &session(),
            )
            .expect_err("refused");
            assert!(err.to_string().contains("--desc is empty"), "{err}");
        }
    }

    #[test]
    fn a_pid_that_cannot_be_identified_is_refused() {
        let err = request(
            &AttendArgs {
                pid: Some(999_999_998),
                ..args(Some("br9711ewy"), Some("test run"))
            },
            &label(),
            &session(),
        )
        .expect_err("unidentifiable pid: refused");
        assert!(err.to_string().contains("identity"), "{err}");
    }

    #[test]
    fn an_over_long_description_is_kept_whole() {
        let long = "cargo nextest run across every crate in the workspace ".repeat(8);
        let (_, desc, _, _) = attend(
            request(
                &AttendArgs {
                    pid: Some(me()),
                    ..args(Some("br9711ewy"), Some(&long))
                },
                &label(),
                &session(),
            )
            .unwrap(),
        );
        assert_eq!(desc.as_deref(), Some(long.trim()));
    }

    #[test]
    fn clearing_names_nothing() {
        let control = request(
            &AttendArgs {
                clear: true,
                ..args(None, None)
            },
            &label(),
            &session(),
        )
        .unwrap();
        assert_eq!(
            control,
            Control::Clear {
                author: "claude".into(),
                session: session(),
            }
        );
        assert!(request(&args(None, None), &label(), &session()).is_err());
    }

    #[test]
    fn expect_parses_the_shared_grammar_and_defaults_to_five_minutes() {
        use clap::Parser;
        #[derive(Parser, Debug)]
        struct T {
            #[command(flatten)]
            a: AttendArgs,
        }
        let t = T::try_parse_from(["t", "br9711ewy"]).unwrap();
        assert_eq!(t.a.expect.as_secs(), 300);
        let t = T::try_parse_from(["t", "br9711ewy", "--expect", "2h"]).unwrap();
        assert_eq!(t.a.expect, std::time::Duration::from_secs(7200));
        let err = T::try_parse_from(["t", "br9711ewy", "--expect", "0"]).expect_err("refused");
        assert!(
            err.to_string().contains("--expect"),
            "names the flag: {err}"
        );
        assert!(
            err.to_string().contains("longer"),
            "and says what to do: {err}"
        );
        let err = T::try_parse_from(["t", "br9711ewy", "--expect", "soon"]).expect_err("refused");
        assert!(err.to_string().contains("--expect"), "{err}");
    }

    #[test]
    fn duration_arg_is_what_expect_parses_back() {
        for secs in [1, 59, 60, 90, 300, 3600, 7200, 86_400 * 2] {
            let arg = duration_arg(secs);
            assert_eq!(
                parse_expect(&arg).unwrap(),
                std::time::Duration::from_secs(secs),
                "{arg} round-trips"
            );
        }
    }
}
