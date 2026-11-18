//! `clank monitor`: one watcher per repo, and the registry waiters join.
//!
//! A repo was being watched once per WAITER. Every parked `clank wait`
//! attached its own `RepoStateWatcher` and refolded the whole repo on
//! each change, as did every `clank tui` — measured at ~170 watchers
//! for 71 agents, with one repo folding 8 times per commit. The fold is
//! per repo; only delivery is per session, because an adapter's wake is
//! its own process's exit or stdout.
//!
//! This module is the wire between them. It carries no delivery: a
//! client receives the same `WaitItem`s it would have derived itself
//! and then speaks its own adapter's protocol, unchanged.

pub(crate) mod keeper;
pub(crate) mod outage;
pub mod upgrade;
pub(crate) mod watchdog;

mod clients;
mod connect;
mod control;
mod custody;
mod fold;
mod hosted;
mod lease;
mod published;
mod releases;
mod retire;
mod runtime;
mod socket;
mod spawn;
mod user_commands;
mod wire;

#[cfg(test)]
mod fixtures;
#[cfg(test)]
mod loop_tests;

pub(crate) use self::connect::{
    ATTACH_BUDGET, Attached, Attempt, Conn, OUTAGE_BUDGET, RETIREMENT_BUDGET, SHARE_ATTEMPTS,
    SPAWN_SETTLE, Seen, connect_within, greet_at, outgoing, write_frame,
};
#[cfg(test)]
pub(crate) use self::control::question_control;
#[cfg(test)]
pub(crate) use self::fold::publish_one_fold;
pub(crate) use self::lease::{LeaseWatch, Retiring, acquire_lease};
#[cfg(test)]
pub(crate) use self::published::read_published;
pub(crate) use self::published::{
    Liveness, Observed, Published, liveness, published, published_in,
};
#[cfg(test)]
pub(crate) use self::published::{describe, publish, publish_in};
pub(crate) use self::retire::{Drain, Shutdown};
#[cfg(test)]
pub(crate) use self::runtime::TempRepo;
#[cfg(any(test, feature = "test-runtime"))]
pub(crate) use self::runtime::test_runtime_root;
#[cfg(test)]
pub(crate) use self::runtime::{Repair, take_repair_permit};
pub(crate) use self::runtime::{
    ensure_runtime_dir, repo_tag, runtime_dir, runtime_path, runtime_path_in,
};
#[cfg(test)]
pub(crate) use self::socket::socket_path;
pub(crate) use self::socket::{Trust, ensure_dir, socket_dirs, trusted_dir};
#[cfg(test)]
pub(crate) use self::spawn::attempt_by;
pub(crate) use self::spawn::{
    CoreWakes, Hangup, Started, attach_or_spawn_until, attach_or_spawn_with, attempt_with,
    core_wakes, request, spawn_monitor, subscribe,
};
pub(crate) use self::wire::{Control, Holder, Note, Who};
#[cfg(test)]
pub(crate) use self::wire::{DescribedBuild, Descriptor, Hello, PROTO, Welcome};

use std::time::Duration;

use clank_core::ids::AgentLabel;

use crate::cli::watch_store::Stream;

use self::clients::{CLIENT_GONE, Clients, Greeted, Heard};
use self::custody::{Authorize, Custody, RecoveryAnswer, RunExits};
#[cfg(test)]
use self::fixtures::*;
use self::fold::Folding;
use self::hosted::{Reading, Sources};
use self::lease::Hold;
use self::published::observation_path;
use self::releases::{Releases, unavailable_somewhere};
use self::retire::{
    DRAIN_GRACE, HANDOFF_DRAIN, LEASE_RETIREMENT, OwnedTasks, RETIRE_BUDGET, SETTLE_BUDGET,
    exit_holding_the_lease, stand_down,
};
use self::user_commands::UserCommandLoop;

/// `clank monitor start`.
pub async fn run(args: crate::cli::MonitorArgs) -> anyhow::Result<()> {
    match args.command {
        crate::cli::MonitorCmd::Start(a) => start(a).await,
        crate::cli::MonitorCmd::Upgrade => upgrade::run().await,
    }
}

async fn start(args: crate::cli::MonitorStartArgs) -> anyhow::Result<()> {
    use futures_util::StreamExt;
    let repo = crate::cli::resolve_repo(args.repo.as_deref())?;
    let repo = std::sync::Arc::new(dunce::canonicalize(&repo).unwrap_or(repo));
    let Some(mut hold) = Hold::take(&repo, &args)? else {
        return Ok(());
    };
    let mut clients = Clients::new();
    let mut folding = Folding::start(&repo);
    let (run_ended, runs_ended) =
        tokio::sync::mpsc::unbounded_channel::<Result<AgentLabel, String>>();
    let vigil = watchdog::Vigil::start(hold.watch(), run_ended.clone())?;
    let run_exits = RunExits::new(run_ended);
    let (mut sources, departing) = Sources::start(&repo);
    // Collected by the owner loop below as they end, and handed whole
    // to the retirement: `abort` only asks, and a producer or a
    // handshake live at the cutoff still owns its resources.
    let mut owned = OwnedTasks::default();
    owned.depart(departing);
    owned.interrupt_roster(&repo);
    let mut custody = Custody::new(&repo, run_exits, runs_ended);
    let mut user_commands = UserCommandLoop::new(&repo);
    let mut releases = Releases::of(&repo);
    let ended = loop {
        if hold.standing_down() {
            hold.begin_retiring(LEASE_RETIREMENT + SETTLE_BUDGET);
            vigil.busy().poke();
            let decision = stand_down_now(
                &repo,
                &mut clients,
                &mut sources,
                &folding,
                &mut owned,
                DRAIN_GRACE,
                LEASE_RETIREMENT,
            )
            .await;
            match decision {
                Shutdown::ReadyToRelease(_) => break Ok(()),
                Shutdown::RequiresProcessExit(_) => exit_holding_the_lease(),
            }
        }
        // Before every wait, so a run registered in the last turn is
        // watched before the loop sleeps.
        custody.track_runs();
        let timer_deadline = sources.timer_deadline();
        let recovery_due = custody.recovery_due();
        let turns_due = custody.turns_due(&clients);
        let uc_due = user_commands.next_due();
        releases.arm()?;
        let waiter_retry = releases.retry_at();
        debug_assert!(
            !unavailable_somewhere(&sources, &clients, &custody)
                || releases.waiting()
                || waiter_retry.is_some(),
            "an operation is unsettled by a held lock nobody waits on"
        );
        // Once: an agent with no waiter is said to have none only after
        // the monitor has served long enough for its waits to arrive.
        let settles = custody.settles();
        let quiesced = hold.quiesced();
        let fold_retry = folding.retry_at();
        // Receive, then look at the lease, then act: whatever woke the
        // loop, nothing it leads to runs as owner on a lease not
        // verified since.
        let wake = tokio::select! {
            // The watchdog saw the lease's path change.
            _ = hold.lease_changed().notified() => Wake::LeaseChanged,
            // Each wake looks at the installation; this one only has to
            // happen.
            _ = hold.installs().notified() => Wake::Installed,
            _ = crate::cli::timer_source::deadline(recovery_due) => Wake::RecoveryDue,
            _ = crate::cli::timer_source::deadline(turns_due) => Wake::TurnsDue,
            Some((conn, said)) = clients.heard.recv() => Wake::Heard(conn, said),
            Some(a) = custody.authorizations.recv() => Wake::Authorize(a),
            Some(answer) = custody.answered.recv() => Wake::Answered(answer),
            Some(event) = custody.changed_records.recv() => {
                let mut events = std::collections::BTreeSet::from([event]);
                while let Ok(more) = custody.changed_records.try_recv() {
                    events.insert(more);
                }
                Wake::Records(events)
            }
            _ = crate::cli::timer_source::clock::sleep_until(timer_deadline) => Wake::TimersDue,
            Some(_) = releases.released() => Wake::Released,
            _ = crate::cli::timer_source::deadline(fold_retry) => Wake::Changed(true),
            _ = crate::cli::timer_source::deadline(settles) => Wake::Settled,
            _ = crate::cli::timer_source::deadline(quiesced) => Wake::Settled,
            Some(ended) = custody.runs_ended.recv() => match ended {
                Ok(author) => Wake::RunEnded(author),
                Err(why) => anyhow::bail!("a process watch failed: {why}"),
            },
            // A client's writer ended, which is how a closed client is
            // seen: `tend` below finds it.
            _ = CLIENT_GONE.notified() => Wake::Settled,
            Some(()) = user_commands.changed.recv() => {
                while user_commands.changed.try_recv().is_ok() {}
                Wake::UserCommands
            }
            Some(ask) = user_commands.asks.recv() => Wake::UserCommandAsk(ask),
            Some(done) = user_commands.done.recv() => Wake::UserCommandDone(done),
            _ = crate::cli::timer_source::deadline(uc_due) => Wake::UserCommandsDue,
            _ = crate::cli::timer_source::deadline(waiter_retry) => Wake::Settled,
            accepted = hold.accept() => Wake::Accepted(accepted?),
            Some(greeted) = clients.arrivals.recv() => Wake::Arrived(greeted),
            Some((author, emission)) = sources.from_sources.recv() => Wake::Emitted(author, emission),
            Some(report) = sources.lifecycles_in.recv() => Wake::Lifecycle(report),
            // A cancelled producer finished winding down. Nothing to
            // do but stop owning it — which is the point: this set is
            // what a retirement would otherwise have to wait for, and
            // collecting here keeps it from growing with the number of
            // config changes a long-lived monitor sees.
            Some(author) = owned.interrupting.next(), if !owned.interrupting.is_empty() => Wake::Interrupted(author),
            Some(_) = owned.departing.next(), if !owned.departing.is_empty() => Wake::Collected,
            // A handshake finished. Whatever it decided has already
            // gone to `arrivals`; this only stops us owning the task.
            Some(_) = owned.greeting.next(), if !owned.greeting.is_empty() => Wake::Collected,
            Some(_) = owned.hearing.next(), if !owned.hearing.is_empty() => Wake::Collected,
            // Receiving from a channel is cancel-safe and carries no
            // state, which is the point — see `changes`.
            changed = folding.changes.recv() => Wake::Changed(changed.is_some()),
        };
        let _turn = vigil.busy().turn();
        let owner = hold.may_act();
        if owner {
            clients.tend(&repo, folding.state(), &mut sources, &mut custody);
        }
        // Which wakes are state changes a waiting user command is looked at
        // again on; its own store's changes are not, since a monitor writing
        // a record wakes itself.
        let uc_round = match &wake {
            Wake::Heard(..) | Wake::Arrived(..) | Wake::Records(..) | Wake::UserCommandsDue => {
                Some(true)
            }
            Wake::UserCommands => Some(false),
            _ => None,
        };
        match wake {
            Wake::Installed => {
                hold.installed()?;
                if owner {
                    custody.installed(&repo, &clients, &mut owned);
                }
            }
            Wake::LeaseChanged | Wake::Collected | Wake::Settled => {}
            Wake::Released => {
                if owner {
                    released(
                        &repo,
                        folding.state(),
                        &mut clients,
                        &mut sources,
                        &mut custody,
                        &mut owned,
                    );
                }
            }
            Wake::RunEnded(author) => {
                if owner {
                    retire_gone_runs(
                        &repo,
                        &author,
                        folding.state(),
                        &mut clients,
                        &mut sources,
                        &mut custody,
                    );
                }
            }
            Wake::UserCommands | Wake::UserCommandsDue => {}
            Wake::UserCommandDone(done) => user_commands.finished(done),
            Wake::UserCommandAsk(ask) => {
                user_commands.ask(ask, owner, &repo, &clients, &mut custody)
            }
            Wake::RecoveryDue => {
                if owner {
                    custody.due(&repo, folding.state(), &clients, &mut sources, &mut owned);
                }
            }
            // Dropped unanswered when retiring, which the asker reads as
            // a veto.
            Wake::Authorize(a) => {
                if owner {
                    custody.authorize(&repo, &clients, &sources, a);
                }
            }
            Wake::TurnsDue => {
                deadlines(
                    owner,
                    &repo,
                    folding.state(),
                    &mut clients,
                    &mut sources,
                    &mut custody,
                );
            }
            Wake::Heard(conn, said) => heard(
                &repo,
                conn,
                said,
                folding.state(),
                &mut clients,
                &mut sources,
                &mut custody,
                &mut owned,
            ),
            Wake::Answered(answer) => custody.answered(owner, &repo, answer, &clients, &mut owned),
            Wake::Records(events) => {
                custody.records_changed(owner, &repo, events, &clients, &mut owned)
            }
            Wake::TimersDue => {
                if owner {
                    sources.timers_due();
                }
            }
            // Dropped when retiring: the peer sees it hang up and finds
            // the successor.
            Wake::Accepted(stream) => {
                if owner {
                    clients.accept(stream, &repo, &mut owned);
                }
            }
            Wake::Arrived(greeted) => match greeted {
                Greeted::Client(client) => {
                    let Some(client) = clients.arrived(client, owner, &mut owned) else {
                        continue;
                    };
                    clients.admit(client, &repo, folding.state(), &mut sources, &mut custody);
                }
                Greeted::Control {
                    request,
                    out,
                    writer,
                } => {
                    let note = control::answer(
                        &request,
                        owner,
                        &repo,
                        folding.state(),
                        &clients,
                        &mut sources,
                        &mut custody,
                        &mut owned,
                    )
                    .await;
                    clients.answer(out, writer, &note);
                    continue;
                }
                // Already acknowledged on its own connection, so
                // nothing is owed to the successor here.
                //
                // The drain is one presentation opportunity for
                // the clients attached right now, each bounded by
                // `WRITE_BUDGET`, and then this process goes
                // whether or not anyone read it. Waiting for
                // quiescence instead would never finish: an
                // observer holds its connection for as long as its
                // panel is open. The ones that get nothing read
                // EOF, which is the recovery door they already
                // have.
                // A monitor already retiring (a stale build, a lost
                // lease) leaves by its own retirement, with the
                // longer producer drain, not this short one.
                Greeted::StandDown if !owner => continue,
                Greeted::StandDown => {
                    let decision = stand_down_now(
                        &repo,
                        &mut clients,
                        &mut sources,
                        &folding,
                        &mut owned,
                        HANDOFF_DRAIN,
                        RETIRE_BUDGET,
                    )
                    .await;
                    match decision {
                        // The lease is held for the life of this
                        // process, so returning IS the handoff —
                        // and it is only safe once nothing this
                        // monitor owned is still running.
                        Shutdown::ReadyToRelease(_) => break Ok(()),
                        Shutdown::RequiresProcessExit(_) => exit_holding_the_lease(),
                    }
                }
            },
            Wake::Emitted(source_owner, emission) => {
                let author = sources.emitted(owner, &repo, source_owner, emission, &mut owned);
                if let Some(state) = folding.state() {
                    clients.present(
                        &repo,
                        state,
                        author.as_ref(),
                        &mut sources,
                        &mut Reading::cached(),
                    );
                }
                if owner && let Some(state) = folding.state() {
                    clients.serve_reports(
                        &repo,
                        Some(state),
                        author.as_ref(),
                        &mut Reading::cached(),
                        &mut sources,
                        &mut custody,
                    );
                }
            }
            Wake::Lifecycle(report) => {
                if sources.lifecycle(report) && owner && sources.publish(&repo) {
                    clients.beat_observers();
                }
            }
            Wake::Interrupted(author) => owned.interrupted(&repo, author),
            Wake::Changed(open) => {
                let dirty = folding.dirtied(open)?;
                if dirty.timers {
                    sources.invalidate_backlogs();
                }
                // Fold BEFORE claiming one. The counter means a fold
                // completed, so moving it first and then failing the
                // rebuild would show the watchdog progress that never
                // happened — the invariant this plan set for itself
                // (codex on 13e9fa7).
                match folding.refold(&repo, vigil.busy()).await {
                    Some(state) => {
                        // Looked at again: the fold was a long await.
                        let owner = owner && hold.may_act();
                        if dirty.events && owner {
                            owned.interrupt_roster(&repo);
                        }
                        // `.clank/` is a wake dir, so an edited
                        // `wait_events` takes effect here rather than
                        // on a restart.
                        if owner && sources.reconcile(&repo, dirty.timers, &mut owned) {
                            clients.beat_observers();
                        }
                        // ONE presentation, carrying the projection,
                        // the durable rows and anything held from
                        // before there was a state to present it
                        // against.
                        clients.present(&repo, &state, None, &mut sources, &mut Reading::cached());
                        if owner {
                            refolded(
                                &repo,
                                &state,
                                &mut clients,
                                &mut sources,
                                &mut custody,
                                &mut owned,
                            );
                        }
                        folding.folded(state);
                    }
                    // The counter stays where it is, so the
                    // watchdog keeps counting this as a still look —
                    // which is how a repeatedly failing fold ends up
                    // standing the monitor down.
                    None => folding.failed(dirty),
                }
            }
        }
        if owner {
            custody.reach(&repo, &clients, &mut owned);
        }
        if let Some(changed) = uc_round
            && owner
        {
            user_commands.round(
                changed,
                &repo,
                folding.state(),
                &mut clients,
                &mut sources,
                &mut custody,
                &mut owned,
            );
        }
    };
    let _ = std::fs::remove_file(observation_path(&repo, hold.pid()));
    ended
}

/// [`stand_down`] with everything the loop holds: its producers drained
/// within `grace`, and the whole within `budget`.
async fn stand_down_now(
    repo: &std::path::Path,
    clients: &mut Clients,
    sources: &mut Sources,
    folding: &Folding,
    owned: &mut OwnedTasks,
    grace: Duration,
    budget: Duration,
) -> Shutdown {
    stand_down(
        repo,
        clients,
        sources,
        std::mem::take(owned),
        folding.state(),
        Some(Drain {
            owners: &crate::cli::source_group::OWNERS,
            grace,
        }),
        budget,
        SETTLE_BUDGET,
    )
    .await
}

/// A lock the loop found held was released ([`Releases`]): what met it
/// runs again ([`retry_contended`]), and recovery folds again what it
/// could not.
fn released(
    repo: &std::path::Path,
    state: Option<&crate::repo_state::RepoState>,
    clients: &mut Clients,
    sources: &mut Sources,
    custody: &mut Custody,
    owned: &mut OwnedTasks,
) {
    retry_contended(repo, state, clients, sources, custody);
    if let Some(state) = state {
        custody.released(repo, state, clients, sources, owned);
    }
}

/// Re-run each hosted timer's contended arm or acceptance, write what an
/// agent's held lock left unwritten of its turn record and questions,
/// and serve again every waiting client and held report, whose reads
/// went without a held log's items.
fn retry_contended(
    repo: &std::path::Path,
    state: Option<&crate::repo_state::RepoState>,
    clients: &mut Clients,
    sources: &mut Sources,
    custody: &mut Custody,
) {
    sources.retry_contended();
    custody.retry_unwritten();
    if let Some(state) = state {
        clients.present(repo, state, None, sources, &mut Reading::cached());
    }
    clients.serve_reports(repo, state, None, &mut Reading::cached(), sources, custody);
}

/// Retire `author`'s runs whose process is gone, and serve its parked wait
/// again: what they held quiet is deliverable now, and no repo change marks
/// that moment.
fn retire_gone_runs(
    repo: &std::path::Path,
    author: &AgentLabel,
    state: Option<&crate::repo_state::RepoState>,
    clients: &mut Clients,
    sources: &mut Sources,
    custody: &mut Custody,
) {
    let g = sources.gather(repo, state, author, &mut Reading::afresh());
    if !custody.end_gone_runs(author, &g) {
        return;
    }
    custody.project(author, &g);
    clients.serve_reports(
        repo,
        state,
        Some(author),
        &mut Reading::afresh(),
        sources,
        custody,
    );
}

/// A held report's hook or an attendance reached its deadline. A report's
/// hook is answered whether or not this monitor owns the repo; only an
/// owner records it, and fails the deliveries ([`expire_deliveries`]) and
/// steps the check-ins ([`deadline_round`]) that fell due.
fn deadlines(
    owner: bool,
    repo: &std::path::Path,
    state: Option<&crate::repo_state::RepoState>,
    clients: &mut Clients,
    sources: &mut Sources,
    custody: &mut Custody,
) {
    let now = crate::cli::turn_custody::now_ms();
    clients.expire_reports(owner.then_some(&mut *custody), now);
    if owner {
        expire_deliveries(repo, state, clients, sources, custody, now);
        deadline_round(repo, state, clients, sources, custody, now);
    }
}

/// The loop's step when attendance deadlines fall due: each due check-in
/// is owed, and each of those agents' parked waits is served again with its
/// facts as they are now, which is what presents the check-in to a park.
///
/// No repo change marks a deadline, so without this re-serve a parked
/// agent would be checked in on, and shown the work its attendance
/// quieted, only when something else changed.
fn deadline_round(
    repo: &std::path::Path,
    state: Option<&crate::repo_state::RepoState>,
    clients: &mut Clients,
    sources: &mut Sources,
    custody: &mut Custody,
    now: u64,
) {
    let stepped = custody.fall_due(now, |author| {
        sources.gather(repo, state, author, &mut Reading::afresh())
    });
    for (author, g) in stepped {
        custody.project(&author, &g);
        clients.serve_reports(
            repo,
            state,
            Some(&author),
            &mut Reading::afresh(),
            sources,
            custody,
        );
    }
}

/// Fail every presentation whose carrier has had its time and not
/// answered, and serve each of those agents' parked waits again: what the
/// presentation carried is owed and undelivered, and nothing else marks
/// the moment it stopped being on its way.
fn expire_deliveries(
    repo: &std::path::Path,
    state: Option<&crate::repo_state::RepoState>,
    clients: &mut Clients,
    sources: &mut Sources,
    custody: &mut Custody,
    now: u64,
) {
    for author in custody.overdue_authors(now) {
        let g = sources
            .gather(repo, state, &author, &mut Reading::afresh())
            .at(now);
        if !custody.expire(&author, &g) {
            continue;
        }
        eprintln!(
            "monitor: `{author}`'s wake was handed to its hook, which never answered for it; \
             it stays owed"
        );
        custody.project(&author, &g);
        clients.serve_reports(
            repo,
            state,
            Some(&author),
            &mut Reading::afresh(),
            sources,
            custody,
        );
    }
}

/// Reporting connection `conn` said something, or closed.
#[allow(clippy::too_many_arguments)]
fn heard(
    repo: &std::path::Path,
    conn: u64,
    said: Heard,
    state: Option<&crate::repo_state::RepoState>,
    clients: &mut Clients,
    sources: &mut Sources,
    custody: &mut Custody,
    owned: &mut OwnedTasks,
) {
    let author = custody.carrier(conn);
    match said {
        Heard::Line(line) => match (
            author,
            serde_json::from_str::<crate::cli::turn_wire::TurnResult>(&line),
        ) {
            (Some(author), Ok(result)) => {
                let settled = settle_result(repo, state, &author, conn, result, sources, custody);
                let g = sources.gather(repo, state, &author, &mut Reading::afresh());
                match settled {
                    Ok(Some(commit)) => {
                        let repo = repo.to_path_buf();
                        let committed = commit.author.clone();
                        owned.depart_task(tokio::spawn(async move {
                            commit.run(&repo).await;
                            tracing::debug!(author = %committed, "monitor delivery committed");
                        }));
                    }
                    Ok(None) => {}
                    Err(why) => eprintln!("monitor: refused a delivery result: {why}"),
                }
                custody.project(&author, &g);
                clients.serve_reports(
                    repo,
                    state,
                    Some(&author),
                    &mut Reading::afresh(),
                    sources,
                    custody,
                );
            }
            (_, Ok(result)) => eprintln!(
                "monitor: a result for {:?} came from a connection that carried \
                 nothing; nothing was committed",
                result.presentation()
            ),
            (_, Err(e)) => {
                eprintln!("monitor: an unreadable frame after a report: {e}")
            }
        },
        Heard::Closed => clients.ended(conn, repo, state, sources, custody),
    }
}

/// Settle a result from connection `conn` against `author`'s custody. A
/// delivery discharges the transient items it presented; anything else
/// leaves them held, owed to the next presentation.
fn settle_result(
    repo: &std::path::Path,
    state: Option<&crate::repo_state::RepoState>,
    author: &AgentLabel,
    conn: u64,
    result: crate::cli::turn_wire::TurnResult,
    sources: &mut Sources,
    custody: &mut Custody,
) -> Result<Option<crate::cli::turn_custody::Commit>, String> {
    let g = sources.gather(repo, state, author, &mut Reading::afresh());
    let commit = custody.result(author, conn, result, &g)?;
    if let Some(commit) = &commit {
        sources.discharge(repo, author, commit);
    }
    Ok(commit)
}

/// A fold completed: runs gone while no monitor watched are ended, held
/// reports stepped against it, and recovery reconciled with it.
fn refolded(
    repo: &std::path::Path,
    state: &crate::repo_state::RepoState,
    clients: &mut Clients,
    sources: &mut Sources,
    custody: &mut Custody,
    owned: &mut OwnedTasks,
) {
    custody.end_every_gone_run(repo);
    clients.serve_reports(
        repo,
        Some(state),
        None,
        &mut Reading::cached(),
        sources,
        custody,
    );
    custody.reconcile(repo, state, clients, sources, owned);
}

/// What woke the monitor's loop, received before anything is done
/// about it.
enum Wake {
    LeaseChanged,
    RecoveryDue,
    Authorize(Authorize),
    Answered(RecoveryAnswer),
    Records(std::collections::BTreeSet<crate::cli::recovery::RecordEvent>),
    TimersDue,
    Accepted(tokio::net::UnixStream),
    Arrived(Greeted),
    Emitted(Stream, crate::cli::event_source::Emission),
    Lifecycle(crate::cli::event_source::LifecycleReport),
    Interrupted(AgentLabel),
    /// A held report's hook reached its deadline, or an attendance's.
    TurnsDue,
    /// A reporting connection said something, or closed.
    Heard(u64, Heard),
    /// A task this loop owned has ended.
    Collected,
    /// Where this build stands against its installation may have moved.
    Installed,
    /// The repo changed, or a failed fold is due again; `false` once the
    /// watcher is gone.
    Changed(bool),
    /// A lock a loop operation found held was let go.
    Released,
    /// A registered run's process exited.
    RunEnded(AgentLabel),
    /// A deadline that only asks the loop to look again: the waiters'
    /// settling, a quiescing build's end, a client gone.
    Settled,
    /// A user-command record changed on disk.
    UserCommands,
    /// A waiting user command expires, or a released park has settled.
    UserCommandsDue,
    /// A user-command attempt asks for the loop's last look.
    UserCommandAsk(crate::cli::user_command::live::Ask),
    UserCommandDone(crate::cli::user_command::live::Done),
}
