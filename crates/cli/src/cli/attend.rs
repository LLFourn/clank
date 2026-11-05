//! `clank attend <name>` — tell the monitor which subagent this agent is
//! waiting on, so work it has already been shown does not wake it again
//! while that work runs. New work still wakes it.
//!
//! An agent may attend several things at once; each request adds an entry
//! or renews the one already attending the same work, and never touches
//! another. Shell work is attended by launching it through `clank run`,
//! which registers itself: a shell task's description is the tool's, not a
//! handle, so a name that matches only shell work is refused.
//!
//! The request is an INTENT. The monitor resolves it against the next
//! turn-end's listing of live background work. Until a listing shows
//! exactly one live match, nothing is quiet. Quiet lasts only while the
//! work is listed running and the `--expect` deadline has not passed; at
//! that deadline the agent is checked in on once.

use super::{AttendArgs, resolve_repo};
use crate::cli::monitor::Control;
use clank_core::turn_policy::{Clearing, Selector};

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
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
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
/// made.
pub(crate) fn request(
    args: &AttendArgs,
    label: &clank_core::ids::AgentLabel,
    session: &clank_core::SessionId,
) -> anyhow::Result<Control> {
    let author = label.to_string();
    let name = args.selector.as_deref().map(str::trim);
    if args.clear {
        let which = match name {
            None => Clearing::All,
            Some("") => anyhow::bail!("the name to clear is empty"),
            Some(name) => Clearing::Named(name.to_string()),
        };
        return Ok(Control::Clear {
            author,
            session: session.clone(),
            which,
        });
    }
    let Some(name) = name else {
        anyhow::bail!("name the subagent to attend, or `--clear` to stop attending");
    };
    if name.is_empty() {
        anyhow::bail!("the name to attend is empty");
    }
    let desc = args
        .desc
        .as_deref()
        .map(normalise_desc)
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| name.to_string());
    Ok(Control::Attend {
        author,
        session: session.clone(),
        selector: Selector::Name(name.to_string()),
        desc: Some(desc),
        expect_ms: args.expect.as_millis() as u64,
        shell: None,
    })
}

/// Why the monitor did not carry out a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NotDone {
    /// No monitor took the request, so nothing was recorded.
    NotTaken(String),
    /// Another attended `clank run` holds this `--desc`.
    DescInUse(String),
    /// The monitor refused it, and why.
    Refused(String),
}

impl std::fmt::Display for NotDone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotTaken(why) => write!(
                f,
                "no monitor took the request ({why}), so nothing was recorded"
            ),
            Self::DescInUse(desc) => write!(
                f,
                "another `clank run` is already attended as `{desc}`; give this one a different \
                 --desc"
            ),
            Self::Refused(why) => f.write_str(why),
        }
    }
}

/// Hand `control` to the repo's monitor, starting one if none is running,
/// and give its answer.
pub(crate) async fn submit(repo: &std::path::Path, control: Control) -> Result<String, NotDone> {
    let attached = crate::cli::monitor::attach_or_spawn_with(
        repo,
        crate::cli::monitor::Who::Control { request: control },
        crate::cli::monitor::spawn_monitor,
    )
    .await;
    let mut conn = match attached {
        crate::cli::monitor::Attached::Shared(conn) => conn,
        crate::cli::monitor::Attached::Alone(why) => return Err(NotDone::NotTaken(why)),
    };
    let deadline = std::time::Instant::now() + crate::cli::monitor::ATTACH_BUDGET;
    match conn.note(deadline) {
        Ok(crate::cli::monitor::Note::Done { what }) => Ok(what),
        Ok(crate::cli::monitor::Note::Refused { why }) => Err(NotDone::Refused(why)),
        Ok(crate::cli::monitor::Note::DescInUse { desc }) => Err(NotDone::DescInUse(desc)),
        Ok(other) => Err(NotDone::NotTaken(format!("the monitor answered {other:?}"))),
        Err(e) => Err(NotDone::NotTaken(format!(
            "the monitor did not answer: {e}"
        ))),
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

    fn label() -> clank_core::ids::AgentLabel {
        clank_core::ids::AgentLabel::parse("claude").unwrap()
    }

    fn session() -> clank_core::SessionId {
        clank_core::SessionId::parse("sess-attend").unwrap()
    }

    fn args(name: Option<&str>, desc: Option<&str>) -> AttendArgs {
        AttendArgs {
            selector: name.map(str::to_string),
            expect: std::time::Duration::from_secs(300),
            desc: desc.map(str::to_string),
            clear: false,
            author: Some("claude".into()),
            repo: None,
        }
    }

    fn attend(control: Control) -> (Selector, Option<String>, u64) {
        match control {
            Control::Attend {
                selector,
                desc,
                expect_ms,
                shell: None,
                ..
            } => (selector, desc, expect_ms),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_subagent_is_attended_by_its_name() {
        let (selector, desc, expect) = attend(
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
        let (_, desc, _) = attend(
            request(
                &args(Some("capture-probe sleeper"), Some("  probe run \n more")),
                &label(),
                &session(),
            )
            .unwrap(),
        );
        assert_eq!(desc.as_deref(), Some("probe run"), "normalised, not raw");
    }

    #[test]
    fn clearing_clears_everything_or_the_one_it_names() {
        let clear = |name: Option<&str>| {
            request(
                &AttendArgs {
                    clear: true,
                    ..args(name, None)
                },
                &label(),
                &session(),
            )
        };
        assert_eq!(
            clear(None).unwrap(),
            Control::Clear {
                author: "claude".into(),
                session: session(),
                which: Clearing::All,
            }
        );
        assert_eq!(
            clear(Some(" test run ")).unwrap(),
            Control::Clear {
                author: "claude".into(),
                session: session(),
                which: Clearing::Named("test run".into()),
            }
        );
        assert!(clear(Some("  ")).is_err());
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
