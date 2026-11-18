use std::time::Duration;

use clank_core::ids::AgentLabel;

use super::clients::{Client, Clients, WRITE_BUDGET, Wrote};
use super::hosted::{Reading, Sources, roster_authors};

/// Standing down is not success, and not the caller's failure either.
/// Nothing reads it — a spawned monitor's stdio is null and its
/// supervisor only reaps — so this is for a human running it by hand.
pub(super) const STOOD_DOWN_EXIT: i32 = 75;

/// What a retirement established before it released the repo.
///
/// Four outcomes for the writers rather than a success count, because
/// "the task ended" is not "the bytes left" and the ways it can end
/// are different answers: a closed peer, a peer that stopped reading,
/// a task that panicked or was cancelled, and one still going when
/// the budget ran out. Counting them together reported a lost frame
/// as a delivery (codex on d2b012d).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Retired {
    /// Queue emptied, socket closed: everything queued landed.
    pub(crate) drained: usize,
    /// Reached a terminal state without draining — the peer errored or
    /// stopped reading.
    pub(crate) failed: usize,
    /// Ended without reporting at all: panicked, or cancelled by
    /// something other than this retirement.
    pub(crate) lost: usize,
    /// Not collected during the grace, so cancelled here.
    ///
    /// Deliberately not "still writing": the deadline can win while a
    /// terminal outcome is already sitting ready, so this counts what
    /// was NOT ACCOUNTED FOR rather than what was definitely still
    /// going (codex on dd67677).
    pub(crate) abandoned: usize,
    /// Transients still held when the cutoff came.
    ///
    /// Reported because they are the one thing a retirement cannot
    /// make good: a durable row replays from the WAL for whoever
    /// connects next, and a transient has no row. Silence here would
    /// hide a lost wake.
    pub(crate) undelivered: usize,
    /// Every task this monitor owned was observed TERMINATED. Decides
    /// whether the lease may be released by unwinding; see
    /// [`Shutdown`].
    pub(crate) settled: bool,
}

/// Whether a retirement earned the right to give the repo up quietly.
///
/// Exhaustive, and returned rather than reported. The first version
/// made this a `bool` on [`Retired`]: the unsettled case printed
/// "releasing anyway" and then released anyway, which is a log line
/// where a contract was wanted (codex on dd67677).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Shutdown {
    /// Everything this monitor owned has ended. Returning from `start`
    /// drops the lease, and nothing is left to race the successor.
    ReadyToRelease(Retired),
    /// Something cancelled has not stopped. The lease must NOT unwind:
    /// a successor taking the repo while a task still owns its
    /// resources is the thing ownership exists to prevent.
    RequiresProcessExit(Retired),
}

impl Shutdown {
    pub(crate) fn report(self) -> Retired {
        match self {
            Shutdown::ReadyToRelease(r) | Shutdown::RequiresProcessExit(r) => r,
        }
    }
}

/// How long a retirement waits for its clients' last frames.
///
/// A whole-step budget, not per client: retiring must be FINITE, and a
/// peer that has stopped reading cannot extend it by the number of its
/// peers. Each writer also bounds its own `write_all` with
/// `WRITE_BUDGET`, so this is the outer limit on all of them together.
pub(super) const RETIRE_BUDGET: Duration = Duration::from_secs(3);

/// What of a successor's stand-down its producers may spend draining,
/// each source's stop grace clipped to it: the clients' last frames keep
/// a writer's whole budget of the rest.
pub(super) const HANDOFF_DRAIN: Duration = RETIRE_BUDGET.saturating_sub(WRITE_BUDGET);

/// How long cancellation is given to actually take effect.
///
/// Separate from `RETIRE_BUDGET` and much shorter: by this point every
/// task has been asked to stop, so this covers only the time between
/// `abort` and the task's future being dropped. Exceeding it means the
/// process exits with the lease still held, because an orderly return
/// would unwind that lease first.
pub(super) const SETTLE_BUDGET: Duration = Duration::from_millis(500);

/// The tasks this monitor owns besides its clients' writers.
///
/// One place rather than two locals, because a retirement has to take
/// ALL of them and "all of them" was spelled at a call site inside
/// `start`'s loop — which no test reaches, so a mutation that left one
/// set behind survived. Here [`OwnedTasks::take_all`] is the whole
/// rule and a test can hold it to it.
#[derive(Default)]
///
/// The sets are open to the loop only for its `select!` to collect what
/// ended; tasks are handed in through the methods.
pub(super) struct OwnedTasks {
    pub(super) interrupting: crate::cli::event_interrupt::Scheduler,
    /// Producers a reconcile cancelled, still winding down.
    pub(super) departing: futures_util::stream::FuturesUnordered<tokio::task::JoinHandle<()>>,
    /// Handshakes still running. One mid-greeting at the cutoff has a
    /// socket, and if it completes it puts a CLIENT with a live writer
    /// into `arrivals`.
    pub(super) greeting: futures_util::stream::FuturesUnordered<tokio::task::JoinHandle<()>>,
    /// Readers of reporting connections. Each ends only when its peer
    /// closes, so a retirement cancels them rather than wait.
    pub(super) hearing: futures_util::stream::FuturesUnordered<tokio::task::JoinHandle<()>>,
}

impl OwnedTasks {
    /// Cancel every live handshake and wait for it to END.
    ///
    /// Before the admission queue closes, and before anything else:
    /// these are what PRODUCE admissions, and one still running when
    /// the queue shuts can welcome a peer and spawn a writer that
    /// nothing owns. `false` means one would not stop, which leaves a
    /// live admission path and forbids an orderly release.
    ///
    /// One bound for the whole phase rather than one per greeting, so
    /// a crowd of slow peers cannot multiply the time this takes.
    async fn end_greetings(&mut self, within: Duration) -> bool {
        use futures_util::StreamExt;
        if self.greeting.is_empty() {
            return true;
        }
        for g in self.greeting.iter_mut() {
            g.abort();
        }
        let ended = async { while self.greeting.next().await.is_some() {} };
        tokio::time::timeout(within, ended).await.is_ok()
    }

    fn take_all(&mut self) -> Vec<tokio::task::JoinHandle<()>> {
        for h in self.hearing.iter() {
            h.abort();
        }
        std::mem::take(&mut self.departing)
            .into_iter()
            .chain(std::mem::take(&mut self.greeting))
            .chain(std::mem::take(&mut self.hearing))
            .chain(std::mem::take(&mut self.interrupting).take_handles())
            .collect()
    }
}

/// A stand-down's producer drain: every group asked to end, all given
/// one `grace` together, before any is cut off.
pub(crate) struct Drain {
    pub(crate) owners: &'static crate::cli::source_group::Owners,
    pub(crate) grace: Duration,
}

/// How long a lease-loss retirement gives its producers to drain.
pub(super) const DRAIN_GRACE: Duration = crate::cli::source_group::DRAIN_GRACE;

const _: () = assert!(
    crate::cli::command_source::DEFAULT_STOP_GRACE.as_millis() >= DRAIN_GRACE.as_millis(),
    "a stopped source's default grace is no shorter than a drain's"
);

/// A lease-loss retirement's whole budget, so its drain is never cut
/// short by the ordinary stand-down's.
pub(super) const LEASE_RETIREMENT: Duration = DRAIN_GRACE.saturating_add(RETIRE_BUDGET);

/// Exit WITH the lease held rather than unwind it. An ordinary return
/// would drop it first and let a successor take the repo on top of a
/// task that still owns its resources; the kernel releases the flock
/// when this process dies, which is the one release that cannot race.
pub(super) fn exit_holding_the_lease() -> ! {
    eprintln!(
        "monitor: cancellation had not taken effect after {SETTLE_BUDGET:?}; exiting with \
         the lease held"
    );
    crate::cli::source_group::exit(STOOD_DOWN_EXIT)
}

/// Everything a stand-down does, from the cutoff to the decision.
///
/// Extracted from `start`'s arm so the whole transition is reachable
/// from a test. It was inline, and a mutation that skipped claiming
/// late arrivals SURVIVED: the helper was proven and its only caller
/// was not, which is the shape of gap that keeps producing these.
/// What stays in the loop is the match on the returned decision.
#[allow(clippy::too_many_arguments)]
pub(super) async fn stand_down(
    repo: &std::path::Path,
    clients: &mut Clients,
    sources: &mut Sources,
    mut owned: OwnedTasks,
    folded: Option<&crate::repo_state::RepoState>,
    drain: Option<Drain>,
    budget: Duration,
    settle: Duration,
) -> Shutdown {
    // ONE deadline across the stages, not one budget each. Giving
    // every phase a fresh `budget` made the cooperative worst case
    // `2 * budget + settle`, so a caller timing out on `budget` would
    // be wrong about its own contract (codex on 5f7bc36).
    let by = tokio::time::Instant::now() + budget;
    let left = |by: tokio::time::Instant| by.saturating_duration_since(tokio::time::Instant::now());
    // ZERO: the drain begins at the cutoff, not after the greetings
    // settle, so they share one timeline: producer admission closes and
    // every group has its TERM and its one cutoff now.
    let draining = drain.map(|drain| drain.owners.drain(drain.grace));
    // ONE: stop the things that PRODUCE admissions, with the receiver
    // still open. A greeting that already handed its client over must
    // still land, and one cancelled mid-handshake must not be able to
    // welcome a peer and spawn a writer after admission closed
    // (codex on b37b6b6). One bound for the phase, not one per
    // greeting.
    let greetings_ended = owned.end_greetings(left(by)).await;

    // TWO: with the producers of admissions stopped, draining the
    // queue is finite. It is the COMPLETE set only when the phase
    // above succeeded; when it did not, the decision at the end
    // forbids an orderly release for exactly that reason.
    let late = clients.claim_late_arrivals();
    if late > 0 {
        eprintln!("monitor: {late} admission(s) arrived during the cutoff");
    }

    // Before any producer task is cancelled: a group draining still
    // sends what it finishes into `from_sources`, read below, and its
    // host reads until the group is gone.
    let drained = match draining {
        Some(draining) => {
            let drainage = draining.finish(by).await;
            for (source, how) in &drainage.outcomes {
                eprintln!("monitor: source {}: {how}", source.display());
            }
            if drainage.end != crate::cli::source_group::DrainEnd::Complete {
                eprintln!(
                    "monitor: the producer drain did not finish: {:?}",
                    drainage.end
                );
            }
            drainage.end == crate::cli::source_group::DrainEnd::Complete
        }
        None => true,
    };

    // Successful sends already hold custody in this queue; cancelling
    // the producers cannot discard a private forwarding buffer. Durable
    // rows replay from the WAL, but transients need this final drain.
    let mut producers: Vec<tokio::task::JoinHandle<()>> = owned.take_all();
    producers.extend(sources.stop_producing());
    sources.close_emissions();

    // FOUR: present ONCE, to the whole set. This used to run in the
    // caller, BEFORE the late clients were claimed — so a transient
    // whose only eligible recipient was still in the queue was
    // retained, then dropped unpresented when that client's sender
    // went. Both the item and its recipient were in hand and the item
    // was reported lost anyway. Nothing to present before the first
    // fold: what a client is owed is read against folded state.
    if let Some(state) = folded {
        clients.present(repo, state, None, sources, &mut Reading::afresh());
    }

    let (served, leaving) = clients.retire();
    let decision = retire(
        served,
        leaving,
        producers,
        left(by),
        settle,
        sources.undelivered(),
    )
    .await;
    let got = decision.report();
    eprintln!(
        "monitor: standing down for {} ({} drained, {} failed, {} lost, {} abandoned)",
        repo.display(),
        got.drained,
        got.failed,
        got.lost,
        got.abandoned
    );
    if got.undelivered > 0 {
        eprintln!(
            "monitor: {} transient(s) reached no client; they have no log row to replay from",
            got.undelivered
        );
    }
    // A greeting that could not be stopped is a live admission path,
    // and a drain not accounted for may leave a producer running: an
    // orderly release must leave neither behind.
    match (greetings_ended && drained, decision) {
        (true, d) => d,
        (false, d) => Shutdown::RequiresProcessExit(d.report()),
    }
}

/// Give up ownership: no more admissions, the queued frames out, the
/// producers stopped, and every owned task observed to have ended.
///
/// The ORDER is the contract. Hosting is cancelled first so no source
/// can submit another emission; then each client's sender is dropped,
/// which is what ends `write_to`'s loop AFTER it has written what is
/// queued; then outcomes are collected as they arrive rather than in
/// handle order, because a stalled writer must not hide the peers
/// behind it that finished long ago. Only when every handle — the
/// writers and the cancelled producers alike — has been observed to
/// end does the caller release the lease.
pub(super) async fn retire(
    clients: Vec<Client>,
    leaving: Vec<tokio::task::JoinHandle<Wrote>>,
    producers: Vec<tokio::task::JoinHandle<()>>,
    budget: Duration,
    settle: Duration,
    undelivered: usize,
) -> Shutdown {
    // Producers arrive already cancelled, by the caller: stopping
    // them is what makes the source buffer finite, and that has to
    // happen before the final presentation rather than here.
    // The already-retired first: a client woken on the very frame that
    // retired it is the likeliest to still have bytes in flight.
    let mut writers: Vec<tokio::task::JoinHandle<Wrote>> = leaving;
    writers.extend(clients.into_iter().filter_map(|c| {
        // Dropping the sender is what ends `write_to`'s loop, AFTER it
        // has written what is already queued.
        c.hang_up()
    }));

    // A set this function OWNS across the deadline, rather than a
    // combinator future it hands the handles to. `select_all` takes
    // them by value, and `tokio::select!` builds its arms eagerly — so
    // when the deadline won, that future was dropped with the
    // unfinished handles inside it and those tasks DETACHED instead of
    // being cancelled. The retirement then reported them as neither
    // delivered nor abandoned. Found by the test for the stalled peer.
    use futures_util::StreamExt;
    let mut running: futures_util::stream::FuturesUnordered<_> = writers.into_iter().collect();

    let mut got = Retired {
        undelivered,
        ..Default::default()
    };
    let deadline = tokio::time::sleep(budget);
    tokio::pin!(deadline);
    while !running.is_empty() {
        tokio::select! {
            _ = &mut deadline => break,
            Some(ended) = running.next() => match ended {
                Ok(Wrote::Drained) => got.drained += 1,
                Ok(Wrote::Failed | Wrote::TimedOut) => got.failed += 1,
                // Panicked, or cancelled by something that is not this
                // retirement. Either way it never reported.
                Err(_) => got.lost += 1,
            },
        }
    }

    got.abandoned = running.len();
    for w in running.iter_mut() {
        w.abort();
    }
    // `abort` only REQUESTS cancellation. Awaiting afterwards is what
    // establishes that the future was dropped and its resources with
    // it — which has to happen before the lease frees, or a successor
    // takes the repo while this one is still winding down.
    let settling = async {
        while running.next().await.is_some() {}
        for p in producers {
            let _ = p.await;
        }
    };
    got.settled = tokio::time::timeout(settle, settling).await.is_ok();
    if got.settled {
        Shutdown::ReadyToRelease(got)
    } else {
        Shutdown::RequiresProcessExit(got)
    }
}

impl OwnedTasks {
    pub(super) fn interrupt_roster(&mut self, repo: &std::path::Path) {
        for author in roster_authors(repo) {
            self.interrupt(repo, author);
        }
    }

    pub(super) fn interrupt(&mut self, repo: &std::path::Path, author: AgentLabel) {
        self.interrupting.notify(repo, author);
    }

    /// `author`'s interrupt pass ended.
    pub(super) fn interrupted(&mut self, repo: &std::path::Path, author: AgentLabel) {
        self.interrupting.completed(repo, author);
    }

    /// Own `tasks` until they end.
    pub(super) fn depart(&mut self, tasks: impl IntoIterator<Item = tokio::task::JoinHandle<()>>) {
        self.departing.extend(tasks);
    }

    /// Own `task` until it ends.
    pub(super) fn depart_task(&mut self, task: tokio::task::JoinHandle<()>) {
        self.departing.push(task);
    }

    /// Own a handshake until it ends.
    pub(super) fn greet(&mut self, task: tokio::task::JoinHandle<()>) {
        self.greeting.push(task);
    }

    /// Own a reporting connection's reader until it ends.
    pub(super) fn hear(&mut self, task: tokio::task::JoinHandle<()>) {
        self.hearing.push(task);
    }
}

#[cfg(test)]
#[path = "retire_tests.rs"]
mod tests;
