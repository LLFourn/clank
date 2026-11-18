use super::connect::{
    ATTACH_BUDGET, Attached, Attempt, Conn, SHARE_ATTEMPTS, SPAWN_SETTLE, attach_by,
    connect_within, look_at, settled,
};
use super::lease::acquire_lease_now;
use super::retire::SETTLE_BUDGET;
use super::runtime::{Repair, take_repair_permit};
use super::wire::{Control, Note, Who};
use std::time::Duration;
/// Whether this repo's ownership became available./// Whether this repo's ownership became available.
#[derive(Debug)]
pub(crate) enum Ownership {
    /// The lease was taken and released, so the next monitor started
    /// can hold it.
    Ready,
    /// Still held when the deadline passed. Not permission.
    Busy,
    /// The holder published while we waited, and this is how looking
    /// at it ended.
    ///
    /// A lease held by nothing published is as often a monitor coming
    /// UP as one going: it takes the lease before it binds and
    /// publishes, and a debug build spends most of a second between the
    /// two. Waiting only for the lease to free waits out the whole
    /// deadline on a healthy monitor.
    Answered(Attempt),
}

/// Wait, within one deadline, for this repo's ownership to free.
///
/// Probing by TAKING the lease is the canonical proof and needs no
/// timer: it is non-blocking, and dropping it immediately hands the
/// repo to whichever monitor is started next. Several clients racing
/// here is fine — whoever wins serves the rest.
#[cfg(test)]
pub(crate) async fn wait_for_ownership(
    repo: &std::path::Path,
    by: std::time::Instant,
    step: Duration,
) -> Ownership {
    wait_for_ownership_by(repo, by, step, |r| acquire_lease_now(r).is_ok(), |_| None).await
}

/// Wait within one deadline using the caller's non-blocking ownership probe.
///
/// A seam, because the boundary that matters cannot be reached
/// otherwise: ownership becoming available and then the deadline
/// passing BEFORE anything is started. Taking the real lease is
/// fast, so a test cannot arrange that gap by scheduling — and a
/// guard nothing can drive is a guard nobody has checked.
///
/// `look` is asked, with what is left of the deadline, after every
/// step the lease stays held; `Some` ends the wait.
pub(crate) async fn wait_for_ownership_by(
    repo: &std::path::Path,
    by: std::time::Instant,
    step: Duration,
    took_it: impl Fn(&std::path::Path) -> bool,
    mut look: impl FnMut(Duration) -> Option<Attempt>,
) -> Ownership {
    loop {
        // Expiry BEFORE readiness. Taking the lease first meant an
        // already-expired deadline still returned Ready when the lease
        // happened to be free, and so did a timer that resumed after
        // `by` — and the caller then started a child with no further
        // check (codex on cf29219).
        let left = by.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return Ownership::Busy;
        }
        if took_it(repo) {
            return Ownership::Ready;
        }
        // Async, so a client waiting for a monitor to go does not park
        // the runtime thread it is waiting on.
        tokio::time::sleep(step.min(left)).await;
        let left = by.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return Ownership::Busy;
        }
        if let Some(ended) = look(left) {
            return Ownership::Answered(ended);
        }
    }
}

/// A monitor this client started, still to be reaped.
///
/// Reaping has to OUTLIVE the attachment. Dropping a `Child` does not
/// reap it and `process_group` does not reparent it, so a monitor that
/// exits while its spawning client is still parked stays a zombie for
/// the rest of that park — and for a repo full of parked waits that is
/// one zombie per wait (codex on fe22d39). The concrete case is two
/// clients starting at once: ours is still coming up, we attach to the
/// winner and return, and ours then loses the lease and exits with
/// nobody left watching for it.
///
/// A trait rather than the started child itself because no test here
/// may start a binary — so this is the only shape in which a test can
/// own a started monitor at all.
pub(crate) trait Started: Send + 'static {
    /// Hand its wait to something that outlives this call, WITHOUT
    /// killing it and without blocking: the monitor must outlive the
    /// client that started it. That is the whole point of spawning
    /// rather than binding.
    fn reap(self: Box<Self>);
}

pub(super) struct Spawned(crate::proc_run::Managed);

impl Started for Spawned {
    fn reap(self: Box<Self>) {
        self.0.detach();
    }
}

/// Hand a started monitor to a waiter that outlives this call: the
/// wait must survive attachment success, fallback and cancellation
/// alike — anything owned by the retry loop is released exactly when
/// the monitor is least likely to have exited yet.
fn supervise(started: Box<dyn Started>) {
    started.reap();
}

/// Attach to this repo's monitor, starting one if none is running.
///
/// Connect-or-SPAWN, never connect-or-bind. A delivery client exits
/// when it delivers, so a client that became the monitor would tear
/// down the watcher and the source set on every wake and rebuild them
/// on the next — rebuilding exactly the duplication this plan exists
/// to remove (codex on a206455). The monitor is a process of its own,
/// and its ownership outlives any delivery.
/// The starting is a parameter so a caller's own tests can reach this
/// decision with no chance of starting a real monitor, which on a temp
/// repo would bind a socket under `/tmp` and take that repo's lease
/// behind the test's back. Production passes [`spawn_monitor`].
pub(crate) async fn attach_or_spawn_with(
    repo: &std::path::Path,
    who: Who,
    spawn: impl Fn(&std::path::Path) -> std::io::Result<Box<dyn Started>>,
) -> Attached {
    attempt_with(repo, who, spawn).await.into()
}

/// [`attach_or_spawn_with`], keeping how the attempt ended as an
/// [`Attempt`].
pub(crate) async fn attempt_with(
    repo: &std::path::Path,
    who: Who,
    spawn: impl Fn(&std::path::Path) -> std::io::Result<Box<dyn Started>>,
) -> Attempt {
    attempt_by(
        repo,
        who,
        ATTACH_BUDGET,
        SPAWN_SETTLE,
        SHARE_ATTEMPTS,
        spawn,
        connect_within,
        |r| acquire_lease_now(r).is_ok(),
    )
    .await
}

/// [`attach_or_spawn`] with the starting supplied, so the whole
/// decision is reachable without a process: no test here may start a
/// binary, and one that started the real monitor would leave it
/// holding this repo's lease after the test returned.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(crate) async fn attach_or_spawn_by(
    repo: &std::path::Path,
    who: Who,
    budget: Duration,
    settle: Duration,
    limit: u32,
    spawn: impl Fn(&std::path::Path) -> std::io::Result<Box<dyn Started>>,
    connect: impl Fn(
        &std::path::Path,
        std::time::Instant,
    ) -> std::io::Result<std::os::unix::net::UnixStream>
    + Copy,
    took_it: impl Fn(&std::path::Path) -> bool,
) -> Attached {
    attempt_by(repo, who, budget, settle, limit, spawn, connect, took_it)
        .await
        .into()
}

/// [`attach_or_spawn_by`], keeping how the attempt ended as an
/// [`Attempt`].
#[allow(clippy::too_many_arguments)]
pub(crate) async fn attempt_by(
    repo: &std::path::Path,
    who: Who,
    budget: Duration,
    settle: Duration,
    limit: u32,
    spawn: impl Fn(&std::path::Path) -> std::io::Result<Box<dyn Started>>,
    connect: impl Fn(
        &std::path::Path,
        std::time::Instant,
    ) -> std::io::Result<std::os::unix::net::UnixStream>
    + Copy,
    took_it: impl Fn(&std::path::Path) -> bool,
) -> Attempt {
    // ONE deadline over the whole handoff: notify, wait for ownership
    // to free, start, attach. The retries used to take a fresh budget
    // each, so the count was the bound rather than any deadline.
    let by = std::time::Instant::now() + settle + budget;
    attach_or_spawn_until(
        repo,
        who,
        by,
        budget,
        settle / limit,
        spawn,
        connect,
        took_it,
    )
    .await
}

/// [`attach_or_spawn_by`] under a deadline the caller already holds: the
/// first handshake gets at most `first` of what is left of it, and every
/// later step only what is left when it starts.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn attach_or_spawn_until(
    repo: &std::path::Path,
    who: Who,
    by: std::time::Instant,
    first: Duration,
    step: Duration,
    spawn: impl Fn(&std::path::Path) -> std::io::Result<Box<dyn Started>>,
    connect: impl Fn(
        &std::path::Path,
        std::time::Instant,
    ) -> std::io::Result<std::os::unix::net::UnixStream>
    + Copy,
    took_it: impl Fn(&std::path::Path) -> bool,
) -> Attempt {
    let left = by.saturating_duration_since(std::time::Instant::now());
    if left.is_zero() {
        return Attempt::NotStarted("the handoff ran out of time before it began".into());
    }
    // A monitor that should keep the repo ends the attempt here.
    // Starting one would add a monitor that cannot own anything, and
    // doing it on every park is the cost this classification exists to
    // remove.
    let look = |left| settled(look_at(repo, who.clone(), left, connect));
    let mut why = match look(first.min(left)) {
        Ok(ended) => return ended,
        Err(why) => why,
    };

    // The repo is ours when its LEASE frees, and nothing sooner says
    // so — so whether it freed has to be an ANSWER, not a fall-through.
    // All three ways out of the old loop reached the start below:
    // acquired, retries exhausted, deadline gone. Only the first is
    // permission (codex on 1c0ce9d).
    //
    // Bounded by the DEADLINE and not by a retry count, because the
    // count ended the wait after about 450ms while a holder standing
    // down legitimately has its retirement budget plus settlement. A
    // child started at 450ms loses the lease and exits, and nothing
    // creates the replacement once the lease finally frees.
    // ONE repair per repo, not one per waiting client. The permit is
    // taken before any lease polling and held through startup — if
    // another client holds it, this one is not a second repairer
    // (codex on cf29219). It looks for the repairer's monitor until the
    // deadline instead: ending on the first refusal failed every client
    // that arrived while one was starting.
    let permit = loop {
        match take_repair_permit(repo) {
            Repair::Acquired(permit) => break permit,
            Repair::Busy => match look_after(by, step, &look).await {
                Some(Ok(ended)) => return ended,
                Some(Err(next)) => why = next,
                None => {
                    return Attempt::Busy(format!(
                        "{why}, and another client is already starting one"
                    ));
                }
            },
            // Not established that anybody is repairing — only that we
            // cannot ask. Saying otherwise would be a guess reported as
            // a fact.
            Repair::Unavailable(gate) => {
                return Attempt::Unproven(format!(
                    "{why}, and the repair gate is unusable: {gate}"
                ));
            }
        }
    };

    // A winner may have appeared while we queued for the permit, so
    // the answer from before it is stale. Attaching to a compatible
    // one, or leaving a newer one serving, is cheaper and more correct
    // than starting anything.
    let left = by.saturating_duration_since(std::time::Instant::now());
    if !left.is_zero() {
        match look(left) {
            Ok(ended) => return ended,
            Err(next) => why = next,
        }
    }

    let ownership = wait_for_ownership_by(repo, by, step, took_it, |left| match look(left) {
        Ok(ended) => Some(ended),
        Err(next) => {
            why = next;
            None
        }
    })
    .await;
    match ownership {
        Ownership::Ready => {}
        Ownership::Answered(ended) => return ended,
        // Still held when time ran out. This client does its own work;
        // a monitor that cannot own the repo helps nobody.
        Ownership::Busy => {
            return Attempt::LeaseHeld(format!("{why}, and its lease is still held"));
        }
    }

    // Expiry again, immediately before starting. Checking it before
    // the lease probe is necessary and does not cover a probe that
    // finished after `by`, or this task resuming past it (codex on
    // 06021ee). The permit goes with the early return.
    if by
        .saturating_duration_since(std::time::Instant::now())
        .is_zero()
    {
        return Attempt::NotStarted(format!("{why}, and the handoff ran out of time"));
    }

    match spawn(repo) {
        // Handed over BEFORE any attach can return, so no exit path
        // below can be the one that drops it.
        Ok(started) => supervise(started),
        Err(e) => return Attempt::SpawnFailed(format!("{why}, and starting one failed: {e}")),
    }

    // Bounded by the deadline alone. A retry count here gave the
    // monitor about 450ms to read its installation, take the lease,
    // bind and publish, which a debug build cannot do; and several
    // callers have no local watch to fall back on.
    while let Some(seen) = look_after(by, step, &look).await {
        match seen {
            Ok(ended) => return ended,
            Err(next) => why = next,
        }
    }
    // Held until here: startup and classification are both done, and a
    // permit released sooner is one the next client takes while this
    // one is still working.
    drop(permit);
    Attempt::Unattached(format!(
        "started a monitor for {}, but {why}",
        repo.display()
    ))
}

/// Sleep one `step`, then `look` with what is LEFT of the one deadline;
/// `None` once it has gone.
///
/// Never a fresh budget: an attempt entering its handshake just before
/// `by` could otherwise block for another whole `ATTACH_BUDGET`, and an
/// outer check cannot make that bounded.
async fn look_after(
    by: std::time::Instant,
    step: Duration,
    look: impl Fn(Duration) -> Result<Attempt, String>,
) -> Option<Result<Attempt, String>> {
    let left = by.saturating_duration_since(std::time::Instant::now());
    if left.is_zero() {
        return None;
    }
    tokio::time::sleep(step.min(left)).await;
    let left = by.saturating_duration_since(std::time::Instant::now());
    if left.is_zero() {
        return None;
    }
    Some(look(left))
}

/// Subscribe to this repo's fold, read-only, or report why not.
///
/// Attaches but never STARTS one. A panel is not a reason to put a
/// daemon on somebody's machine, and the fallback costs only what it
/// cost before this existed — its own watcher.
/// Ask the running monitor to restart one source, and say what it
/// answered. Never starts a monitor: with none running there is nothing
/// to restart.
pub(crate) fn request(repo: &std::path::Path, control: Control) -> Result<String, String> {
    let mut conn = match attach_by(
        repo,
        Who::Control { request: control },
        ATTACH_BUDGET,
        connect_within,
    ) {
        Attached::Shared(conn) => conn,
        Attached::Alone(why) => return Err(format!("no monitor is running for this repo ({why})")),
    };
    let deadline = std::time::Instant::now() + ATTACH_BUDGET + SETTLE_BUDGET;
    match conn.note(deadline) {
        Ok(Note::Done { what }) => Ok(what),
        Ok(Note::Refused { why }) => Err(why),
        Ok(other) => Err(format!("the monitor answered {other:?}")),
        Err(e) => Err(format!("the monitor did not answer: {e}")),
    }
}

pub(crate) fn subscribe(repo: &std::path::Path) -> Attached {
    attach_by(repo, Who::Observer, ATTACH_BUDGET, connect_within)
}

/// How a subscriber's reading thread is told to stop: a flag it reads,
/// a shutdown of the socket it is blocked on, and a wake for a thread
/// parked holding a watcher. Nothing about it is looked at on a clock.
#[derive(Default)]
pub(crate) struct Hangup {
    stopped: std::sync::Mutex<bool>,
    parked: std::sync::Condvar,
    socket: std::sync::Mutex<Option<std::os::unix::net::UnixStream>>,
    pokes: std::sync::Mutex<Vec<std::sync::mpsc::Sender<()>>>,
}

impl Hangup {
    fn lock_stopped(&self) -> std::sync::MutexGuard<'_, bool> {
        self.stopped
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn stop(&self) {
        let mut stopped = self.lock_stopped();
        *stopped = true;
        self.parked.notify_all();
        let socket = self
            .socket
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(socket) = socket {
            let _ = socket.shutdown(std::net::Shutdown::Both);
        }
        for poke in self
            .pokes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain(..)
        {
            let _ = poke.send(());
        }
    }

    /// Send on `poke` at [`Self::stop`], for a thread blocked receiving.
    pub(crate) fn poke_on_stop(&self, poke: std::sync::mpsc::Sender<()>) {
        let stopped = self.lock_stopped();
        if *stopped {
            let _ = poke.send(());
        } else {
            self.pokes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(poke);
        }
    }

    pub(crate) fn stopped(&self) -> bool {
        *self.lock_stopped()
    }

    /// Shut `conn`'s socket down on [`Self::stop`], or now if that
    /// already happened, so a read blocked on it returns.
    pub(crate) fn hang_up_on(&self, conn: &Conn) {
        let stopped = self.lock_stopped();
        let Ok(socket) = conn.cancellation_socket() else {
            return;
        };
        if *stopped {
            let _ = socket.shutdown(std::net::Shutdown::Both);
        } else {
            *self
                .socket
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(socket);
        }
    }

    /// Block until [`Self::stop`].
    pub(crate) fn park(&self) {
        let mut stopped = self.lock_stopped();
        while !*stopped {
            stopped = self
                .parked
                .wait(stopped)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }
}

/// Forward a monitor's notes onto `wake` until the sharing ends, then
/// hand the job back to a local watcher for good.
///
/// On its own thread: `Conn` reads a blocking socket, and the callers
/// are the status loop and the TUI, neither of which has a place to
/// await. It reads with no deadline. A monitor that dies closes the
/// socket, and a wedged one is stood down by its own watchdog, which
/// closes it too; `stop` shuts it from this end. An observer is sent a
/// note only when the fold moved, so silence is the healthy idle state
/// and is never read as loss.
pub(super) fn forward_beats<M: Clone + Send + Sync + 'static>(
    mut conn: Conn,
    repo: std::path::PathBuf,
    wake: crate::fs_notice::Wake<M>,
    stop: std::sync::Arc<Hangup>,
    fault: crate::fs_notice::Fault,
) {
    std::thread::spawn(move || {
        stop.hang_up_on(&conn);
        loop {
            if stop.stopped() {
                return;
            }
            match conn.next_note() {
                Ok(_) => {
                    if !wake.ring() {
                        return;
                    }
                }
                Err(e) => {
                    if stop.stopped() {
                        return;
                    }
                    eprintln!(
                        "clank: the monitor for {} went away ({e}); watching this repo directly",
                        repo.display()
                    );
                    break;
                }
            }
        }
        // Losing the monitor must not leave a panel frozen on stale
        // state, so this thread takes over the watching itself rather
        // than returning and leaving nobody.
        let attached = crate::repo_watch::RepoStateWatcher::attach_reporting_to(
            &repo,
            wake.clone(),
            fault.clone(),
        );
        if let Err(e) = &attached {
            crate::fs_notice::record(&fault, format!("cannot watch {}: {e:#}", repo.display()));
            wake.ring();
        }
        let Some(watcher) = reconcile_after(&wake, attached) else {
            return;
        };
        stop.park();
        drop(watcher);
    });
}

/// Take over from the monitor, and reconcile the gap it leaves.
///
/// The wake is the point. A change that landed after the last beat and
/// before `attached` existed is BASELINED by that watcher rather than
/// reported, so the consumer would sit on stale output until some later
/// edit or its own backstop — and the handoff happens exactly when the
/// repo is most likely to have changed, since that is why the monitor
/// is gone (codex on 697807a).
///
/// Separated from the attaching because the wake is otherwise
/// indistinguishable from the watcher's own startup traffic: a test
/// that watched a real directory passed whether or not this sent
/// anything, which is a test that proves nothing. The ordering is
/// still guaranteed — `attached` is evaluated before the call.
pub(super) fn reconcile_after<W, M: Clone>(
    wake: &crate::fs_notice::Wake<M>,
    attached: anyhow::Result<W>,
) -> Option<W> {
    let watcher = match attached {
        Ok(watcher) => watcher,
        // Nothing is watching, so a wake would promise a freshness
        // that is not coming.
        Err(e) => {
            eprintln!("clank: cannot watch this repo directly either: {e}");
            return None;
        }
    };
    wake.ring();
    Some(watcher)
}

/// A subscription to the repo's fold, or a watcher of our own.
///
/// Dropping it stops whichever it turned out to be. The two cases are
/// behind one type because a caller has no decision to make: it wants
/// to be woken when the repo changes, and whether that comes from the
/// shared fold or a local watcher is this module's business.
pub(crate) struct CoreWakes {
    stop: std::sync::Arc<Hangup>,
    /// Held only in the unshared case; the shared one keeps its
    /// watcher, if it ever needs one, inside its thread.
    _local: Option<crate::repo_watch::RepoStateWatcher>,
    fault: crate::fs_notice::Fault,
}

impl CoreWakes {
    /// Why the repo watcher behind these wakes failed, once it has: from
    /// then on nothing will wake its owner for the repo's state.
    pub(crate) fn fault(&self) -> Option<String> {
        crate::fs_notice::read(&self.fault)
    }
}

impl Drop for CoreWakes {
    fn drop(&mut self) {
        self.stop.stop();
    }
}

/// Be woken on repo-state changes, through the monitor if there is one.
///
/// This is the duplication the plan measured: every `clank tui` and
/// every `clank status --watch` attached a watcher of its own and
/// refolded on every change, alongside one per parked wait. A
/// subscriber computes nothing — it is told the fold moved and rebuilds
/// only what it displays.
pub(crate) fn core_wakes<M: Clone + Send + Sync + 'static>(
    repo: &std::path::Path,
    wake: impl Into<crate::fs_notice::Wake<M>>,
) -> anyhow::Result<CoreWakes> {
    let wake = wake.into();
    let stop = std::sync::Arc::new(Hangup::default());
    let fault = crate::fs_notice::Fault::default();
    match subscribe(repo) {
        Attached::Shared(conn) => {
            forward_beats(conn, repo.to_path_buf(), wake, stop.clone(), fault.clone());
            Ok(CoreWakes {
                stop,
                _local: None,
                fault,
            })
        }
        Attached::Alone(why) => {
            tracing::debug!("watching {} directly: {why}", repo.display());
            Ok(CoreWakes {
                stop,
                _local: Some(crate::repo_watch::RepoStateWatcher::attach_reporting_to(
                    repo,
                    wake,
                    fault.clone(),
                )?),
                fault,
            })
        }
    }
}

/// Start this repo's monitor as a process of its own.
///
/// Its own process group, so a tool that signals the agent's group
/// cannot take the repo's watcher down with it, and null stdio so it
/// neither writes into the client's terminal nor dies with its pipes.
/// A spawn that loses the lease race exits by itself, which is why
/// starting one needs no coordination with the other clients.
///
/// It is still this process's CHILD — `process_group` moves the group,
/// not the parent — so it needs [`supervise`] to reap it, and only the
/// client's own exit reparents it to init.
pub(crate) fn spawn_monitor(repo: &std::path::Path) -> std::io::Result<Box<dyn Started>> {
    let installed = match crate::build_stamp::rendezvous() {
        Some(rendezvous) => rendezvous,
        None => crate::build_stamp::Rendezvous {
            path: std::env::current_exe()?,
            source: crate::build_stamp::Source::Fallback,
        },
    };
    crate::proc_run::start(monitor_command(&installed, repo).group(crate::proc_run::Group::New))
        .map(|child| Box::new(Spawned(child)) as Box<dyn Started>)
}

/// The command that starts `repo`'s monitor from the installation at
/// `installed`, telling it where that is so it compares against the same
/// file it was started from.
pub(super) fn monitor_command(
    installed: &crate::build_stamp::Rendezvous,
    repo: &std::path::Path,
) -> crate::proc_run::Spec {
    crate::proc_run::Spec::new(&installed.path)
        .args(["monitor", "start", "--repo"])
        .arg(repo)
        .arg("--installed-at")
        .arg(&installed.path)
        .args(["--installed-from", installed.source.as_str()])
}

#[cfg(test)]
#[path = "spawn_tests.rs"]
mod tests;
