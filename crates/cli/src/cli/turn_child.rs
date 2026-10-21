//! `clank wait --turn-end`: the child a provider hook spawns to carry its
//! report to the monitor.
//!
//! Its stdin is the hook's pipe: the report on the first line, then at
//! most one delivery result, then EOF. EOF is also how it learns the hook
//! is gone, killed or not, and it then exits. Its stdout carries each
//! verdict as one JSON line, as soon as the monitor sends it. The result
//! goes back on the connection that carried the presentation, which is
//! the only one the monitor will credit with it.
//!
//! With no monitor to decide, it falls back to the local fold and the
//! Alone contract ([`clank_core::turn_policy::alone_verdict`]): owed work
//! wakes the agent with a notice, never with the work, so nothing is
//! presented and no receipt is earned. Grok's handoff needs no fold: it is
//! answered as the monitor would answer it.

use std::io::Write;
use std::path::{Path, PathBuf};

use clank_core::AgentLabel;
use clank_core::turn_policy::PresentationId;

use super::monitor::{Attached, Note, Who};
use super::turn_wire::{Frame, Report, TurnResult, WireVerdict};

/// What the hook says on the child's stdin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Said {
    Line(String),
    /// Every write end closed: the hook exited, or was killed.
    Gone,
}

/// The hook's side of the pipe, read on a thread of its own so EOF is seen
/// while everything else waits.
pub(crate) struct Owner {
    rx: tokio::sync::mpsc::UnboundedReceiver<Said>,
}

impl Owner {
    pub(crate) fn reading(input: impl std::io::Read + Send + 'static) -> Self {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        std::thread::spawn(move || {
            use std::io::BufRead;
            let mut lines = std::io::BufReader::new(input);
            loop {
                let mut line = String::new();
                match lines.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) if !line.ends_with('\n') => break,
                    Ok(_) => {
                        if tx.send(Said::Line(line.trim_end().to_string())).is_err() {
                            return;
                        }
                    }
                }
            }
            let _ = tx.send(Said::Gone);
        });
        Owner { rx }
    }

    async fn next(&mut self) -> Said {
        self.rx.recv().await.unwrap_or(Said::Gone)
    }
}

/// How a monitor is attached to, injected so a test can supply its own.
pub(crate) type Attach<'a> = &'a (
        dyn Fn(Who) -> std::pin::Pin<Box<dyn std::future::Future<Output = Attached> + Send + 'a>>
            + Sync
    );

pub(crate) async fn run(args: super::WaitArgs) -> anyhow::Result<()> {
    let repo = super::resolve_repo(args.repo.as_deref())?;
    let author = match args.author.as_deref() {
        Some(raw) => AgentLabel::parse(raw)?,
        None => crate::agent_env::resolve_identity_from_env(&repo)?,
    };
    let owner = Owner::reading(std::io::stdin());
    let attach =
        |who: Who| -> std::pin::Pin<Box<dyn std::future::Future<Output = Attached> + Send + '_>> {
            Box::pin(super::monitor::attach_or_spawn_with(
                &repo,
                who,
                super::monitor::spawn_monitor,
            ))
        };
    let mut out = std::io::stdout();
    carry(
        &repo,
        &author,
        args.deliver_to_session.clone(),
        owner,
        &mut out,
        &attach,
    )
    .await;
    Ok(())
}

/// Carry one report: read it, take it to the monitor, print what comes
/// back, and answer for any presentation on the connection that brought
/// it. `session` is the session the wait is bound to deliver to.
pub(crate) async fn carry(
    repo: &Path,
    author: &AgentLabel,
    session: Option<String>,
    mut owner: Owner,
    out: &mut impl Write,
    attach: Attach<'_>,
) {
    let report = match owner.next().await {
        Said::Line(line) => match serde_json::from_str::<Report>(&line) {
            Ok(report) => report,
            Err(e) => {
                let _ = emit(
                    out,
                    &Frame::bare(WireVerdict::Diagnostic {
                        message: format!("hook: its report did not parse: {e}"),
                    }),
                );
                return;
            }
        },
        Said::Gone => return,
    };
    // A handoff holds nothing for its session: holding it would take the
    // place of the very wait whose running it reports.
    let holder = session
        .as_ref()
        .filter(|_| !matches!(report, Report::Handoff { .. }))
        .map(|session| super::monitor::Holder {
            tool: report.tool(),
            session: session.clone(),
        });
    // A monitor that goes away mid-park is followed once more, through
    // the ordinary attach-or-spawn, before the local fold takes over.
    for _ in 0..2 {
        let who = Who::Agent {
            author: author.to_string(),
            holder: holder.clone(),
            has_cli_sources: false,
            turn_end: Some(report.clone()),
        };
        match attach(who).await {
            Attached::Shared(conn) => match shared(conn, &report, &mut owner, out).await {
                Hand::Done => return,
                Hand::Lost(why) => tracing::debug!(%why, "the monitor went mid-report"),
            },
            Attached::Alone(why) => {
                tracing::debug!(%why, "no monitor for this report");
                break;
            }
        }
    }
    alone(repo, author, session, &report, &mut owner, out).await;
}

/// How a shared exchange ended.
enum Hand {
    /// Answered, or the hook is gone: nothing is left to do.
    Done,
    /// The monitor went before a terminal verdict, and why.
    Lost(String),
}

fn emit(out: &mut impl Write, frame: &Frame) -> std::io::Result<()> {
    let line = serde_json::to_string(frame).map_err(std::io::Error::other)?;
    writeln!(out, "{line}")?;
    out.flush()
}

async fn shared(
    conn: super::monitor::Conn,
    report: &Report,
    owner: &mut Owner,
    out: &mut impl Write,
) -> Hand {
    let socket = match conn.cancellation_socket() {
        Ok(socket) => socket,
        Err(e) => return Hand::Lost(e.to_string()),
    };
    let mut notes = match super::wait::bridge_notes(conn, super::monitor::BEAT_GRACE) {
        Ok(notes) => notes,
        Err(e) => return Hand::Lost(e.to_string()),
    };
    let mut why = "its connection closed".to_string();
    loop {
        tokio::select! {
            said = owner.next() => match said {
                Said::Gone => return Hand::Done,
                Said::Line(_) => {}
            },
            note = notes.recv() => match note {
                Some(Ok(Note::Verdict(frame))) => {
                    if emit(out, &frame).is_err() {
                        return Hand::Done;
                    }
                    if !frame.terminal(report.parks()) {
                        continue;
                    }
                    if let Some(id) = frame.presentation() {
                        forward_result(&socket, id, owner).await;
                    }
                    return Hand::Done;
                }
                Some(Ok(Note::Superseded)) => {
                    let _ = emit(
                        out,
                        &Frame::bare(WireVerdict::Silent {
                            why: clank_core::SilentReason::StaleBinding,
                        }),
                    );
                    return Hand::Done;
                }
                Some(Ok(Note::Refused { why })) => {
                    let _ = emit(
                        out,
                        &Frame::bare(WireVerdict::Diagnostic {
                            message: format!("hook: the monitor refused the wait: {why}"),
                        }),
                    );
                    return Hand::Done;
                }
                Some(Ok(_)) => {}
                Some(Err(e)) => why = e,
                None => return Hand::Lost(why),
            },
        }
    }
}

/// Forward the hook's one result for presentation `id` on the socket that
/// carried it. A pipe that closes before a result, or a result for
/// anything else, is a failed emit.
async fn forward_result(
    socket: &std::os::unix::net::UnixStream,
    id: &PresentationId,
    owner: &mut Owner,
) {
    let result = match owner.next().await {
        Said::Line(line) => match serde_json::from_str::<TurnResult>(&line) {
            Ok(result) if result.presentation() == id => result,
            _ => TurnResult::EmitFailed {
                presentation: id.clone(),
                why: "the hook answered for something else".into(),
            },
        },
        Said::Gone => TurnResult::EmitFailed {
            presentation: id.clone(),
            why: "the hook closed its pipe before it answered".into(),
        },
    };
    let Ok(line) = serde_json::to_string(&result) else {
        return;
    };
    let deadline = std::time::Instant::now() + super::monitor::ATTACH_BUDGET;
    if let Err(e) = super::monitor::write_frame(socket, format!("{line}\n").as_bytes(), deadline) {
        tracing::debug!(%e, "the result could not be forwarded; the monitor reads its loss as a failure");
    }
}

/// No monitor can decide: the local fold says only whether work is owed,
/// and owed work wakes the agent with a notice. Nothing is presented.
async fn alone(
    repo: &Path,
    author: &AgentLabel,
    session: Option<String>,
    report: &Report,
    owner: &mut Owner,
    out: &mut impl Write,
) {
    if let Report::Handoff { handoff, .. } = report {
        let g = super::turn_custody::Gathered::of(repo, author, None, Err(String::new()));
        let _ = emit(
            out,
            &super::turn_custody::frame_of(g.handoff_verdict(handoff)),
        );
        return;
    }
    let bound = report.session();
    let verdict = |owed: &[clank_core::wait::WaitItem]| {
        super::turn_custody::Gathered::of(repo, author, Some((owed, None)), Err(String::new()))
            .alone_verdict(bound)
    };
    let gated = verdict(&[]);
    if gated != clank_core::turn_policy::Verdict::Park {
        let _ = emit(out, &super::turn_custody::frame_of(gated));
        return;
    }
    let args = |peek: bool| super::WaitArgs {
        repo: Some(repo.to_path_buf()),
        author: Some(author.to_string()),
        die_with_owner: false,
        deliver_to_session: session.clone(),
        turn_end: false,
        json: true,
        events: Vec::new(),
        r#for: None,
        peek,
        no_cache: false,
        poll: false,
        no_poll: false,
    };
    if !report.parks() {
        let owed = super::wait::items(args(true)).await;
        let frame = match owed {
            Ok(owed) => super::turn_custody::frame_of(verdict(&owed.unwrap_or_default())),
            Err(e) => failure(author, &e),
        };
        let _ = emit(out, &frame);
        return;
    }
    let left = report
        .deadline()
        .saturating_sub(super::turn_custody::now_ms());
    let by = tokio::time::Instant::now() + std::time::Duration::from_millis(left);
    let mut accept = |items: &[clank_core::wait::WaitItem]| {
        verdict(items) != clank_core::turn_policy::Verdict::Park
    };
    let mut cwd: Option<PathBuf> = None;
    let waiting = tokio::time::timeout_at(
        by,
        super::wait::items_alone(args(false), &mut cwd, &mut accept),
    );
    tokio::pin!(waiting);
    let frame = loop {
        tokio::select! {
            said = owner.next() => match said {
                Said::Gone => return,
                Said::Line(_) => {}
            },
            waited = &mut waiting => break match waited {
                Err(_) => super::turn_custody::Turns::deadline_frame(report.tool()),
                Ok(Ok(owed)) => super::turn_custody::frame_of(verdict(&owed.unwrap_or_default())),
                Ok(Err(e)) => failure(author, &e),
            },
        }
    };
    let _ = emit(out, &frame);
}

/// A local wait that could not run, as the hook reports it.
fn failure(author: &AgentLabel, e: &anyhow::Error) -> Frame {
    if super::wait::superseded(e) {
        return Frame::bare(WireVerdict::Silent {
            why: clank_core::SilentReason::StaleBinding,
        });
    }
    let message = match e.downcast_ref::<crate::agent_store::RoleResolutionError>() {
        Some(_) => super::turn_custody::role_failure(author, e),
        None => format!("hook: the wait failed: {e:#}"),
    };
    Frame::bare(WireVerdict::Diagnostic { message })
}

#[cfg(test)]
#[path = "turn_child_tests.rs"]
mod tests;
