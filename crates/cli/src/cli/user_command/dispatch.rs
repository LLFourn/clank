//! The one dispatcher: the monitor's evaluation of every waiting request
//! ([`pass`]) and the attempt itself ([`attempt`]).
//!
//! An attempt takes the request's id lock, then the pane's write lock,
//! and with both held finds the agent's pane again, which must still be the
//! one recorded at the first look and still run Claude Code, and reads the
//! input from one dump. Then comes its one commit point: the loop's
//! readiness answer, then the agent's state lock, and under it the clock
//! and the binding read again; then `dispatching` is persisted and the
//! command typed. Locks are released in reverse.
//!
//! No cycle: the loop only tries an agent's state lock, takes id locks only with a
//! try-lock, and never takes a pane lock (its own pane writers run off the
//! loop and defer on a reserved pane), so asking it while holding the id
//! and pane locks cannot wait on this attempt. State-lock holders take
//! no other lock, and ordinary writers hold nothing while they wait for a
//! pane.
//!
//! At most once: `dispatching` is synced before the first byte, and a
//! reader that takes the id lock and finds it knows its dispatcher died
//! and records `unconfirmed`. Nothing is ever retried.

use super::request::{Command, Id};
use super::store::{Asked, Held, PaneSeen, Record, Stamp, State, Store};
use crate::cli::nudge_log;
use crate::cli::open_zellij::PaneWrite;
use crate::cli::pane_input::{Terminal, UserCommandEvidence};
use clank_core::Tool;
use clank_core::ids::{AgentLabel, SessionId};
use std::collections::BTreeSet;

/// Why a `dispatching` record found with its lock free is `unconfirmed`:
/// its dispatcher died holding the lock, maybe after typing.
const DIED: &str = "its dispatcher stopped before recording the outcome; it is never retried";

/// Why a record an older clank filed is refused rather than typed.
pub(crate) const OLDER: &str =
    "it was filed by an older clank in a form this one does not type; it needs a new request";

/// What `record` asks to type, if it may still be typed at `now` for the
/// agent bound as `bound`: the session and command, or why it is refused.
fn runnable(
    record: &Record,
    now: u64,
    bound: Option<(Tool, SessionId)>,
) -> Result<(SessionId, Command), String> {
    let Asked::Run { session, command } = &record.asked else {
        return Err(OLDER.into());
    };
    if record.expired(now) {
        return Err("expired".into());
    }
    super::bound_to(bound, session)?;
    Ok((session.clone(), command.clone()))
}

/// What the monitor's loop says of an agent a request waits for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Look {
    /// Its latest turn completed, nothing is in flight or pending, and no
    /// hook is parked.
    Go,
    /// Only a parked hook stands in the way.
    Parked,
    /// Not idle yet, and why.
    Wait(String),
}

/// The loop's view of the agents, for a pass.
pub(crate) trait Agents {
    /// The tool and session now bound to `label`.
    fn binding(&self, label: &AgentLabel) -> Option<(Tool, SessionId)>;
    fn look(&mut self, label: &AgentLabel, session: &SessionId) -> Look;
}

/// One request an attempt is spawned for: to record its pane at the first
/// look, and to dispatch it when `dispatch`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Job {
    pub(crate) label: AgentLabel,
    pub(crate) id: Id,
    pub(crate) dispatch: bool,
}

/// What a pass decided.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Pass {
    pub(crate) jobs: Vec<Job>,
    /// Agents whose park a request waits on: their next Stop, or parked
    /// report, ends instead of parking.
    pub(crate) release: BTreeSet<AgentLabel>,
    /// Agents with a request waiting, whose records recovery must read for
    /// their turn's end.
    pub(crate) watch: BTreeSet<AgentLabel>,
    /// The earliest moment a waiting request expires.
    pub(crate) next_expiry: Option<u64>,
}

/// Where a pass or an attempt works, and the moment it judges by.
pub(crate) struct Ctx<'a> {
    pub(crate) store: &'a Store,
    pub(crate) tag: &'a str,
    pub(crate) now: u64,
    pub(crate) history: &'a (dyn Fn(&AgentLabel, nudge_log::Record) + Sync),
}

/// Evaluate every request filed for this repo whose lock is free and that
/// `skip` does not hold back: settle a dead dispatcher's attempt as
/// unconfirmed, refuse one an older clank filed, one past its lifetime or
/// one whose session has gone, and otherwise record why it waits, ask for
/// the park standing in its way to end, or hand it to an attempt. A
/// record's state and reason are written only when they change.
pub(crate) fn pass(
    ctx: &Ctx<'_>,
    agents: &mut impl Agents,
    skip: impl Fn(&AgentLabel, &Id) -> bool,
) -> Pass {
    let mut out = Pass::default();
    for (label, id) in ctx.store.list(ctx.tag) {
        if skip(&label, &id) {
            continue;
        }
        let held = match ctx.store.try_lock(ctx.tag, &label, &id) {
            Ok(Some(held)) => held,
            Ok(None) => continue,
            Err(e) => {
                eprintln!("monitor: user command `{label}/{}`: {e:#}", id.as_str());
                continue;
            }
        };
        let mut record = match held.load() {
            Ok(Some(record)) => record,
            Ok(None) => continue,
            Err(e) => {
                eprintln!("monitor: user command `{label}/{}`: {e:#}", id.as_str());
                continue;
            }
        };
        if record.state.terminal() {
            continue;
        }
        if record.state == State::Dispatching {
            settle(
                ctx,
                &held,
                &label,
                &id,
                &mut record,
                State::Unconfirmed,
                DIED.into(),
                None,
            );
            continue;
        }
        let session = match runnable(&record, ctx.now, agents.binding(&label)) {
            Ok((session, _)) => session,
            Err(why) => {
                settle(
                    ctx,
                    &held,
                    &label,
                    &id,
                    &mut record,
                    State::Refused,
                    why,
                    None,
                );
                continue;
            }
        };
        let expiry = record.expires_at() + 1;
        out.next_expiry = Some(out.next_expiry.map_or(expiry, |e| e.min(expiry)));
        out.watch.insert(label.clone());
        let reason = match agents.look(&label, &session) {
            Look::Go => {
                out.jobs.push(Job {
                    label,
                    id,
                    dispatch: true,
                });
                continue;
            }
            Look::Parked => {
                out.release.insert(label.clone());
                "releasing its parked turn".to_string()
            }
            Look::Wait(why) => why,
        };
        if record.pane.is_none() {
            out.jobs.push(Job {
                label: label.clone(),
                id: id.clone(),
                dispatch: false,
            });
        }
        if record.moved(State::WaitingForIdle, Some(reason), ctx.now)
            && let Err(e) = held.save(&record)
        {
            eprintln!("monitor: user command `{label}/{}`: {e:#}", id.as_str());
        }
    }
    out
}

/// Move `record` to a terminal `state`, save it, and add it to the agent's
/// nudge history. `typed` is the line when anything may have reached the
/// pane.
#[allow(clippy::too_many_arguments)]
fn settle(
    ctx: &Ctx<'_>,
    held: &Held,
    label: &AgentLabel,
    id: &Id,
    record: &mut Record,
    state: State,
    reason: String,
    typed: Option<String>,
) {
    let reason = (state != State::Delivered).then_some(reason);
    if !record.moved(state, reason.clone(), ctx.now) {
        return;
    }
    if let Err(e) = held.save(record) {
        eprintln!("monitor: user command `{label}/{}`: {e:#}", id.as_str());
    }
    let why = reason.unwrap_or_default();
    (ctx.history)(
        label,
        nudge_log::Record {
            at: ctx.now,
            attempt: format!("user-command:{}", id.as_str()),
            trigger: nudge_log::Trigger::UserCommand {
                id: id.as_str().to_string(),
            },
            text: typed.unwrap_or_default(),
            outcome: match state {
                State::Delivered => nudge_log::Outcome::Submitted,
                State::Unconfirmed => nudge_log::Outcome::Indeterminate { reason: why },
                _ => nudge_log::Outcome::Refused { reason: why },
            },
        },
    );
}

/// What an attempt needs from the world, supplied so tests drive it with a
/// fake terminal, a fake clock and a fake crash, never a real pane.
pub(crate) trait Seams: Sync {
    fn now(&self) -> u64;
    /// The tool and session bound to `label` now.
    fn binding(&self, label: &AgentLabel) -> Option<(Tool, SessionId)>;
    /// The agent's one pane, and the tool the roster gives it.
    async fn discover(&self, label: &AgentLabel) -> Result<(PaneSeen, Tool), String>;
    /// The loop's last look at the agent, asked before any lock but the id's.
    async fn last_look(&self, label: &AgentLabel, session: &SessionId) -> Result<(), String>;
    /// Hold `label`'s state lock until the guard is dropped.
    async fn hold_binding<'a>(
        &'a self,
        label: &'a AgentLabel,
    ) -> Result<Box<dyn Send + 'a>, String>;
    /// Hold `pane`'s write lock until the guard is dropped.
    async fn lock_pane(&self, pane: &PaneSeen) -> Result<Box<dyn Send>, String>;
    type Term<'a>: Terminal
    where
        Self: 'a;
    fn terminal<'a>(&'a self, pane: &'a PaneSeen) -> Self::Term<'a>;
    fn evidence(&self) -> Option<&UserCommandEvidence>;
    fn history(&self, label: &AgentLabel, record: nudge_log::Record);
}

/// How an attempt ended, for the loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Ended {
    /// Nothing more to do for it here.
    Done,
    /// It waits on the pane's input, which only a state change, a rewrite of
    /// its record (a repeat submit) or its expiry should make the loop look
    /// at again.
    OnScreen { stamp: Option<Stamp>, expires: u64 },
}

/// One attempt for `job`: record the pane at the first look, and, when
/// dispatching, run the final checks under the three locks and type.
pub(crate) async fn attempt(store: &Store, tag: &str, job: &Job, seams: &impl Seams) -> Ended {
    let (label, id) = (&job.label, &job.id);
    let held = match crate::cli::event_interrupt::blocking(|| store.lock(tag, label, id)) {
        Ok(held) => held,
        Err(e) => {
            eprintln!("monitor: user command `{label}/{}`: {e:#}", id.as_str());
            return Ended::Done;
        }
    };
    let Ok(Some(mut record)) = held.load() else {
        return Ended::Done;
    };
    let history = |label: &AgentLabel, r: nudge_log::Record| seams.history(label, r);
    let ctx = Ctx {
        store,
        tag,
        now: seams.now(),
        history: &history,
    };
    if record.state == State::Dispatching {
        settle(
            &ctx,
            &held,
            label,
            id,
            &mut record,
            State::Unconfirmed,
            DIED.into(),
            None,
        );
        return Ended::Done;
    }
    if !matches!(record.state, State::Admitted | State::WaitingForIdle) {
        return Ended::Done;
    }
    let refuse = |record: &mut Record, why: String| {
        settle(&ctx, &held, label, id, record, State::Refused, why, None);
        Ended::Done
    };
    let session = match runnable(&record, ctx.now, seams.binding(label)) {
        Ok((session, _)) => session,
        Err(why) => return refuse(&mut record, why),
    };
    if record.pane.is_none() {
        match seams.discover(label).await {
            Ok((seen, Tool::Claude)) => {
                record.pane = Some(seen);
                if let Err(e) = held.save(&record) {
                    eprintln!("monitor: user command `{label}/{}`: {e:#}", id.as_str());
                    return Ended::Done;
                }
            }
            Ok((_, tool)) => {
                return refuse(
                    &mut record,
                    format!("its pane runs {tool}, not Claude Code"),
                );
            }
            Err(why) => {
                return refuse(
                    &mut record,
                    format!("its pane could not be located with certainty: {why}"),
                );
            }
        }
    }
    if !job.dispatch {
        return Ended::Done;
    }
    let wait = |record: &mut Record, why: String| {
        if record.moved(State::WaitingForIdle, Some(why), ctx.now)
            && let Err(e) = held.save(record)
        {
            eprintln!("monitor: user command `{label}/{}`: {e:#}", id.as_str());
        }
    };
    let pane = record.pane.clone().expect("recorded above");
    let _pane_lock = match seams.lock_pane(&pane).await {
        Ok(guard) => guard,
        Err(why) => return refuse(&mut record, format!("its pane could not be locked: {why}")),
    };
    match seams.discover(label).await {
        Ok((now, Tool::Claude)) if now == pane => {}
        Ok((_, Tool::Claude)) => {
            return refuse(
                &mut record,
                "its pane was replaced since it was first found; it needs a new request".into(),
            );
        }
        Ok((_, tool)) => {
            return refuse(
                &mut record,
                format!("its pane runs {tool}, not Claude Code"),
            );
        }
        Err(why) => {
            return refuse(
                &mut record,
                format!("its pane could not be located with certainty: {why}"),
            );
        }
    }
    let term = seams.terminal(&pane);
    let read = term
        .dump()
        .await
        .map(|screen| crate::cli::composer::composer(Tool::Claude, &screen))
        .unwrap_or(crate::cli::composer::Composer::Unreadable);
    if read != crate::cli::composer::Composer::Empty {
        wait(
            &mut record,
            format!("its input is not an empty prompt ({read:?})"),
        );
        drop(held);
        return Ended::OnScreen {
            stamp: store.stamp(tag, label, id),
            expires: record.expires_at() + 1,
        };
    }
    // The commit point: the loop's readiness, then the clock and the binding
    // as they are now, with no await between them and the first byte but
    // taking the agent's state lock, whose holders take no other lock.
    if let Err(why) = seams.last_look(label, &session).await {
        wait(&mut record, why);
        return Ended::Done;
    }
    let _binding = match seams.hold_binding(label).await {
        Ok(guard) => guard,
        Err(why) => return refuse(&mut record, format!("its binding could not be held: {why}")),
    };
    let now = seams.now();
    let command = match runnable(&record, now, seams.binding(label)) {
        Ok((_, command)) => command,
        Err(why) => return refuse(&mut record, why),
    };
    let Some(row) = seams.evidence() else {
        return refuse(
            &mut record,
            "no committed evidence that Claude Code runs a typed `!` line as one command; a \
             human records it (README, \"User commands\")"
                .into(),
        );
    };
    if command.line().chars().count() > row.max_command {
        return refuse(
            &mut record,
            format!(
                "the command is longer than the {} characters observed to run whole",
                row.max_command
            ),
        );
    }
    let ctx = Ctx { now, ..ctx };
    if !record.moved(State::Dispatching, None, now) {
        return Ended::Done;
    }
    if let Err(e) = held.save(&record) {
        eprintln!(
            "monitor: user command `{label}/{}`: not typed, `dispatching` was not recorded: {e:#}",
            id.as_str()
        );
        return Ended::Done;
    }
    let typed = crate::cli::pane_input::type_user_command(&term, &pane.pane, &command).await;
    let line = Some(format!("!{}", command.line()));
    let (state, reason, line) = match typed {
        Ok(()) => (State::Delivered, String::new(), line),
        Err(PaneWrite::NotStarted(why)) => (
            State::FailedNotStarted,
            format!("nothing was typed: {why}"),
            None,
        ),
        Err(PaneWrite::Unknown(why)) => (
            State::Unconfirmed,
            format!("it may have been typed, and is never retried: {why}"),
            line,
        ),
    };
    settle(&ctx, &held, label, id, &mut record, state, reason, line);
    Ended::Done
}

#[cfg(test)]
#[path = "dispatch_tests.rs"]
mod tests;
