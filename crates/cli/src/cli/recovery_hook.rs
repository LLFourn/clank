//! `clank stop-hook --observe | --recovery-helper | --recovery-reply`:
//! the transport recovery uses for a harness with no readable record of
//! its own (OpenCode's plugin). The monitor decides each attempt and keeps
//! every record; these carry a turn the harness reported, park for the one
//! authorized retry, and carry its answer, all on the monitor's
//! connection.

use std::path::Path;

use super::StopHookArgs;
use super::monitor::Control;
use crate::agent_env::{Process, resolve_identity_for_hook_in};
use crate::lifecycle::AgentLabel;
use clank_core::Tool;

/// The recovery modes: reporting an observed turn, parking as a recovery
/// helper, and answering an attempt. Each speaks for the bound session
/// named on stdin and stays silent for any other.
pub(crate) async fn run(args: StopHookArgs) -> anyhow::Result<()> {
    let tool: Tool = args.tool.into();
    let input = super::stop_hook::read_hook_stdin().map_err(|e| anyhow::anyhow!(e))?;
    let repo = super::stop_hook::resolve_hook_repo(args.repo.as_deref(), &input)
        .map_err(|e| anyhow::anyhow!(e))?;
    let label = resolve_identity_for_hook_in(&repo, tool, &input.session_id, &Process)?;
    let session = input.session_id.as_str().to_string();
    if !crate::agent_store::binding_matches(&repo, &label, tool, &session) {
        return Ok(());
    }
    let reply = |n, opened_by, answer, started| Control::RecoveryReply {
        author: label.to_string(),
        tool,
        reply: crate::cli::recovery::Reply {
            session: session.clone(),
            opened_by,
            n,
            answer,
            started,
        },
    };
    if let Some(turn) = observation(&args, tool, &session) {
        let heard = super::attend::submit(
            &repo,
            Control::Observe {
                author: label.to_string(),
                tool,
                session: session.clone(),
                turn,
            },
        )
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
        println!("{}", serde_json::json!({ "recovery": heard }));
        return Ok(());
    }
    if let (Some(answer), Some(n), Some(opened_by)) =
        (args.recovery_reply, args.attempt, args.opened_by.clone())
    {
        let answer = match answer {
            crate::cli::RecoveryReplyArg::Submitted => crate::cli::recovery::ReplyAnswer::Submitted,
            crate::cli::RecoveryReplyArg::Refused => crate::cli::recovery::ReplyAnswer::Refused,
            crate::cli::RecoveryReplyArg::Indeterminate => {
                crate::cli::recovery::ReplyAnswer::Indeterminate
            }
        };
        super::attend::submit(&repo, reply(n, opened_by, answer, args.started.clone()))
            .await
            .map_err(|e| anyhow::anyhow!(e))?;
        return Ok(());
    }
    let Some(dispatch) = await_recovery(&repo, &label, tool, &session).await else {
        return Ok(());
    };
    if tool == Tool::Claude {
        // Claude's asyncRewake wakes the session on exit 2 with the text on
        // stderr. Answered as submitted first: once this process exits it
        // cannot answer, and an unconfirmed submission escalates on its own.
        let n = dispatch["n"].as_u64().unwrap_or_default() as u32;
        let opened_by = dispatch["opened_by"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        super::attend::submit(
            &repo,
            reply(
                n,
                opened_by,
                crate::cli::recovery::ReplyAnswer::Submitted,
                None,
            ),
        )
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
        eprintln!("{}", dispatch["wake"].as_str().unwrap_or_default());
        crate::cli::source_group::exit(2);
    }
    println!("{dispatch}");
    Ok(())
}

/// The line `--observe` adds to the session's record, in the shape the
/// monitor's reader takes it.
fn observation(args: &StopHookArgs, tool: Tool, session: &str) -> Option<serde_json::Value> {
    let turn = match args.observe? {
        crate::cli::ObserveArg::Started => "started",
        crate::cli::ObserveArg::Completed => "completed",
        crate::cli::ObserveArg::Failed => "failed",
        crate::cli::ObserveArg::Interrupted => "interrupted",
        crate::cli::ObserveArg::Retry => "retry",
    };
    let class = args.class.as_deref().map(|name| match tool {
        Tool::OpenCode => crate::cli::recovery::opencode_class(name, args.status).to_string(),
        _ => name.to_string(),
    });
    Some(serde_json::json!({
        "session": session,
        "turn": turn,
        "id": args.turn_id,
        "class": class.unwrap_or_else(|| "unknown".into()),
        "at": crate::cli::recovery::unix_now(),
    }))
}

/// Wait on the monitor for this session's one authorized retry. Nothing
/// printed means none came: the monitor went, the binding moved, or the
/// helper was refused.
async fn await_recovery(
    repo: &Path,
    label: &AgentLabel,
    tool: Tool,
    session: &str,
) -> Option<serde_json::Value> {
    use crate::cli::monitor::{Attached, Holder, Note, Who};
    let Attached::Shared(mut conn) = crate::cli::monitor::attach_or_spawn_with(
        repo,
        Who::RecoveryHelper {
            author: label.to_string(),
            holder: Holder {
                tool,
                session: session.to_string(),
            },
        },
        crate::cli::monitor::spawn_monitor,
    )
    .await
    else {
        return None;
    };
    tokio::task::spawn_blocking(move || {
        loop {
            let deadline = std::time::Instant::now() + crate::cli::monitor::BEAT_GRACE;
            match conn.note(deadline) {
                Ok(Note::Recover {
                    opened_by,
                    n,
                    tag,
                    wake,
                }) => {
                    return Some(serde_json::json!({
                        "opened_by": opened_by,
                        "n": n,
                        "tag": tag,
                        "wake": wake,
                    }));
                }
                Ok(Note::Beat) => continue,
                _ => return None,
            }
        }
    })
    .await
    .ok()
    .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The argv the OpenCode plugin sends (its harness asserts these)
    /// becomes lines the monitor reads as that session's turns, against
    /// the turn each names.
    #[test]
    fn the_plugins_reports_are_read_as_the_sessions_turns() {
        use crate::cli::recovery::{Fact, Outcome, RecordReader};
        #[derive(clap::Parser)]
        struct T {
            #[command(flatten)]
            args: crate::cli::StopHookArgs,
        }
        let line = |argv: &[&str], session: &str| {
            let t =
                <T as clap::Parser>::try_parse_from(["t", "--tool", "opencode"].iter().chain(argv))
                    .unwrap();
            observation(&t.args, Tool::OpenCode, session)
                .unwrap()
                .to_string()
        };
        let mut reader = RecordReader::new(Tool::OpenCode, "ses_a");
        let facts: Vec<_> = [
            line(&["--observe", "started", "--turn-id", "user_1"], "ses_a"),
            line(
                &[
                    "--observe",
                    "failed",
                    "--turn-id",
                    "x",
                    "--class",
                    "ProviderAuthError",
                ],
                "ses_b",
            ),
            line(&["--observe", "retry"], "ses_a"),
            line(
                &[
                    "--observe",
                    "failed",
                    "--turn-id",
                    "user_1",
                    "--class",
                    "APIError",
                    "--status",
                    "529",
                ],
                "ses_a",
            ),
        ]
        .iter()
        .filter_map(|l| reader.read(l))
        .collect();
        assert_eq!(
            facts,
            [
                Fact::Started {
                    id: "user_1".into(),
                    text: None
                },
                Fact::NativeRetry,
                Fact::Ended {
                    id: Some("user_1".into()),
                    outcome: Outcome::Failed("overloaded".into())
                },
            ]
        );
    }
}
