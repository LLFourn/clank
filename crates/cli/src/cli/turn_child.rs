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
//! It never decides anything about work. A monitor that goes away is
//! followed, every time, through the ordinary attach-or-spawn. When none
//! can be had within [`super::monitor::OUTAGE_BUDGET`] or the report's
//! deadline, it wakes the agent with a notice that says so and carries no
//! work: nothing is presented and no receipt is earned. An outage is
//! announced to a session once: until a monitor answers it, later
//! children go on trying and end as an uneventful park does.
//!
//! A child of a build that is no longer the installed one says none of
//! that. Its hook is about to become the installed build and start the
//! child that can follow the new monitor, so it goes on trying quietly
//! until its pipe closes: an upgrade costs the agent no turn.

use std::io::Write;
use std::path::Path;

use clank_core::AgentLabel;

use crate::agent_store::Announcement;
use clank_core::turn_policy::{Notice, PresentationId};

use super::monitor::{Attempt, Note, OUTAGE_RETRY_CAP, Outage, Unserved, Who};
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

    /// Resolves once the hook is gone, whatever it says before then.
    async fn gone(&mut self) {
        while self.next().await != Said::Gone {}
    }
}

/// How a monitor is attached to, injected so a test can supply its own.
pub(crate) type Attach<'a> = &'a (
        dyn Fn(Who) -> std::pin::Pin<Box<dyn std::future::Future<Output = Attempt> + Send + 'a>>
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
        |who: Who| -> std::pin::Pin<Box<dyn std::future::Future<Output = Attempt> + Send + '_>> {
            Box::pin(super::monitor::attempt_with(
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
        &crate::upgrade::process(),
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
    authority: &crate::upgrade::Authority,
) {
    let report = match owner.next().await {
        Said::Line(line) => match serde_json::from_str::<Report>(&line) {
            Ok(report) => report,
            Err(e) => {
                emit_final(
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
    let holder = session
        .as_ref()
        .filter(|_| report.holds_session())
        .map(|session| super::monitor::Holder {
            tool: report.tool(),
            session: session.clone(),
        });
    let by = last_word(&report);
    // A parked report's deadline is the monitor's to answer first, and its
    // hook waits past it for that. One that never parks has only the
    // child's own last word.
    let answer_by = match report.parks() {
        true => by.and_then(|by| by.checked_add(DEADLINE_MARGIN * 2)),
        false => by,
    };
    let mut outage = Outage::default();
    let mut stale = false;
    // The last attempt found a monitor on its way out. A report that
    // cannot wait that out ends quietly: its agent's next Stop asks the
    // monitor that follows.
    let mut retiring = false;
    let mut why = "no monitor answered before the wait's deadline".to_string();
    let answered = || {
        if let Err(e) = crate::agent_store::outage_served(repo, author, report.session()) {
            tracing::debug!(%e, "the outage's notice could not be cleared");
        }
    };
    loop {
        let who = Who::Agent {
            author: author.to_string(),
            holder: holder.clone(),
            has_cli_sources: false,
            turn_end: Some(report.clone()),
        };
        let attempt = tokio::select! {
            // The deadline last: it ends an attempt that is still out,
            // never one that would have attached.
            biased;
            () = owner.gone() => return,
            attempt = attach(who) => attempt,
            () = passes(by) => break,
        };
        let unserved = match attempt.served(repo) {
            Ok(conn) => {
                let hand = shared(
                    conn,
                    &report,
                    &mut owner,
                    out,
                    &mut outage,
                    &answered,
                    answer_by,
                );
                match hand.await {
                    Hand::Done => return,
                    Hand::Lost(why) => Unserved::Failed(why),
                    Hand::Unanswered if report.parks() => {
                        emit_final(
                            out,
                            &super::turn_custody::Turns::deadline_frame(report.tool()),
                        );
                        return;
                    }
                    Hand::Unanswered => {
                        // A monitor of a replaced build answers nothing
                        // new while it settles what it handed out: that
                        // silence is the upgrade, and no news to an agent.
                        if !report.reaches_agent()
                            || super::monitor::outgoing(repo)
                            || !told_once(repo, author, &report, Notice::MonitorUnanswered, out)
                        {
                            emit_final(out, &still("the monitor has not answered"));
                        }
                        return;
                    }
                }
            }
            Err(unserved) => unserved,
        };
        why = unserved.why().to_string();
        retiring = matches!(unserved, Unserved::Retiring(_));
        tracing::debug!(%why, "no monitor holds this report");
        // Nobody else can say so while there is no monitor: a session that
        // no longer holds the label is owed nothing, not even the notice.
        if !crate::agent_store::binding_matches(
            repo,
            author,
            report.tool(),
            report.session().as_str(),
        ) {
            return;
        }
        // A monitor lost or an attach refused is when a stale child finds
        // out it is one.
        stale = authority.look().stale().is_some();
        let pause = match outage.lost(tokio::time::Instant::now(), &unserved) {
            Some(pause) => pause,
            None if stale || !report.reaches_agent() => OUTAGE_RETRY_CAP,
            None => match told_once(repo, author, &report, outage_of(&why), out) {
                true => return,
                false => OUTAGE_RETRY_CAP,
            },
        };
        tokio::select! {
            biased;
            () = owner.gone() => return,
            () = passes(by) => break,
            () = tokio::time::sleep(pause) => {}
        }
    }
    // A park's deadline is not an outage: its hook arms the next park,
    // whose child follows with a budget of its own. Only a report whose
    // hook gives up here has this as its one chance to be told.
    if report.parks() {
        emit_final(
            out,
            &super::turn_custody::Turns::deadline_frame(report.tool()),
        );
    } else if stale
        || retiring
        || !report.reaches_agent()
        || !told_once(repo, author, &report, outage_of(&why), out)
    {
        emit_final(out, &still(&format!("clank still has no monitor: {why}")));
    }
}

fn outage_of(why: &str) -> Notice {
    Notice::MonitorOutage {
        why: why.to_string(),
    }
}

/// What a report that never parks ends with when its session was already
/// told of the trouble: a diagnostic, which wakes nobody.
fn still(what: &str) -> Frame {
    Frame::bare(WireVerdict::Diagnostic {
        message: format!("hook: {what}"),
    })
}

/// What the hook is told in place of a monitor's verdict: `notice`, or
/// nothing for a session clank does not drive.
fn unmonitored(repo: &Path, author: &AgentLabel, report: &Report, notice: Notice) -> Frame {
    let gathered = super::turn_custody::Gathered::of(repo, author, None);
    super::turn_custody::frame_of(gathered.unmonitored_verdict(report.session(), notice))
}

/// Give `notice` of having no monitor's verdict, unless this session was
/// already told of this outage. `false` when it was, and nothing was said.
fn told_once(
    repo: &Path,
    author: &AgentLabel,
    report: &Report,
    notice: Notice,
    out: &mut impl Write,
) -> bool {
    let frame = unmonitored(repo, author, report, notice);
    if frame.verdict != WireVerdict::Notify {
        emit_final(out, &frame);
        return true;
    }
    let announced = crate::agent_store::announce_outage(
        repo,
        author,
        report.session(),
        time::OffsetDateTime::now_utc(),
        &mut || emit(out, &frame),
    );
    match announced {
        Ok(Announcement::Suppressed) => false,
        Ok(Announcement::Delivered) => true,
        Ok(Announcement::Undelivered(e)) => {
            tracing::debug!(%e, "the hook was gone before its notice");
            true
        }
        // Whether it was told cannot be known, and waking twice is the
        // cheaper mistake.
        Err(e) => {
            tracing::debug!(%e, "the outage's notice could not be recorded");
            emit_final(out, &frame);
            true
        }
    }
}

/// How long before a report's deadline the child gives its own last word,
/// so the hook reads it before framing a deadline of its own: Grok's
/// handoff hook gives up at the very instant its report names.
pub(crate) const DEADLINE_MARGIN: std::time::Duration = std::time::Duration::from_secs(2);

/// When the child stops trying for `report`, on this runtime's clock:
/// [`DEADLINE_MARGIN`] before the hook can no longer hold its wait.
/// `None` when that is further off than the clock can say.
fn last_word(report: &Report) -> Option<tokio::time::Instant> {
    let left = std::time::Duration::from_millis(
        report
            .deadline()
            .saturating_sub(super::turn_custody::now_ms()),
    );
    tokio::time::Instant::now().checked_add(left.saturating_sub(DEADLINE_MARGIN))
}

async fn passes(by: Option<tokio::time::Instant>) {
    match by {
        Some(by) => tokio::time::sleep_until(by).await,
        None => std::future::pending().await,
    }
}

/// How a shared exchange ended.
enum Hand {
    /// Answered, or the hook is gone: nothing is left to do.
    Done,
    /// The monitor went before a terminal verdict, and why.
    Lost(String),
    /// The monitor is there and had given no terminal verdict by the time
    /// the report had to be answered.
    Unanswered,
}

/// Print the child's last frame. The hook is its only reader, so a write
/// that fails means the hook is gone, and there is nobody left to tell.
fn emit_final(out: &mut impl Write, frame: &Frame) {
    if let Err(e) = emit(out, frame) {
        tracing::debug!(%e, "the hook was gone before its last frame");
    }
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
    outage: &mut Outage,
    answered: &(dyn Fn() + Sync),
    by: Option<tokio::time::Instant>,
) -> Hand {
    let socket = match conn.cancellation_socket() {
        Ok(socket) => socket,
        Err(e) => return Hand::Lost(e.to_string()),
    };
    let mut notes = match super::wait::bridge_notes(conn) {
        Ok(notes) => notes,
        Err(e) => return Hand::Lost(e.to_string()),
    };
    let mut why = "its connection closed".to_string();
    let mut heard = false;
    loop {
        tokio::select! {
            // A verdict already in hand outranks the deadline it beat.
            biased;
            said = owner.next() => match said {
                Said::Gone => return Hand::Done,
                Said::Line(_) => {}
            },
            note = notes.recv() => {
                if matches!(note, Some(Ok(_))) {
                    outage.answered();
                }
                // Any note proves the monitor there, not that it answers:
                // only a verdict ends what the session was told of.
                if matches!(note, Some(Ok(Note::Verdict(_))))
                    && !std::mem::replace(&mut heard, true)
                {
                    answered();
                }
                match note {
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
                    emit_final(
                        out,
                        &Frame::bare(WireVerdict::Silent {
                            why: clank_core::SilentReason::StaleBinding,
                        }),
                    );
                    return Hand::Done;
                }
                Some(Ok(Note::Refused { why })) => {
                    emit_final(
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
                }
            }
            () = passes(by) => return Hand::Unanswered,
        }
    }
}

/// How long the hook has to say what became of a presentation: the
/// monitor's own bound, less the margin that lets this child's word reach
/// the monitor before the monitor gives the presentation up by itself.
pub(crate) const RESULT_WITHIN: std::time::Duration = std::time::Duration::from_millis(
    clank_core::turn_policy::DELIVERY_RESULT_WITHIN - DEADLINE_MARGIN.as_millis() as u64,
);

/// The hook's one result for presentation `id`. A pipe that closes before
/// a result, a result for anything else, and a hook that says nothing
/// within [`RESULT_WITHIN`] are each a failed emit.
async fn result_of(id: &PresentationId, owner: &mut Owner) -> TurnResult {
    let failed = |why: &str| TurnResult::EmitFailed {
        presentation: id.clone(),
        why: why.into(),
    };
    match tokio::time::timeout(RESULT_WITHIN, owner.next()).await {
        Ok(Said::Line(line)) => match serde_json::from_str::<TurnResult>(&line) {
            Ok(result) if result.presentation() == id => result,
            _ => failed("the hook answered for something else"),
        },
        Ok(Said::Gone) => failed("the hook closed its pipe before it answered"),
        Err(_) => failed("the hook gave no result in time"),
    }
}

/// Forward the hook's one result for presentation `id` on the socket that
/// carried it.
async fn forward_result(
    socket: &std::os::unix::net::UnixStream,
    id: &PresentationId,
    owner: &mut Owner,
) {
    let result = result_of(id, owner).await;
    let Ok(line) = serde_json::to_string(&result) else {
        return;
    };
    let deadline = std::time::Instant::now() + super::monitor::ATTACH_BUDGET;
    if let Err(e) = super::monitor::write_frame(socket, format!("{line}\n").as_bytes(), deadline) {
        tracing::debug!(%e, "the result could not be forwarded; the monitor reads its loss as a failure");
    }
}

#[cfg(test)]
#[path = "turn_child_tests.rs"]
pub(crate) mod tests;
