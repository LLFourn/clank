//! The process group a command source's producer runs in, from before
//! anything in it can consume until it is confirmed empty.
//!
//! A producer cannot be gated by itself: the user's command can pull a
//! remote message the instant it runs, and for a source whose receive
//! is destructive, a consumer nobody reads is data loss. So the group
//! is made recoverable FIRST, by an anchor that consumes nothing:
//!
//! a. the anchor is spawned leading a new group, and ignores TERM
//!    before it does anything else;
//! b. the group is recorded, by the anchor's pgid and birth;
//! c. the host writes the activation token, and the anchor — which
//!    decides whether the token was right — acknowledges it;
//! d. only after that acknowledgement is the command spawned INTO the
//!    anchor's group.
//!
//! An unarmed anchor exits on EOF, so a host dying anywhere before (d)
//! leaves its successor a group that is `Gone`, or an armed anchor it
//! can identify. The anchor is killed last, so the group stays
//! identifiable for its whole life, cleanup included.

use crate::cli::command_source::{self as cs, Producer, Retired};
use crate::cli::github::log::IngestLease;
use crate::signal::{Pgid, Pid};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

/// Fail-closed until armed: EOF, a short or wrong line, or an
/// acknowledgement nobody can read all end it. `exec` keeps the pid
/// and birth the record names, and carries the ignored TERM along.
const ANCHOR: &str = "trap '' TERM
IFS= read -r token || exit 1
[ \"$token\" = \"$1\" ] || exit 1
printf 'armed\\n' || exit 1
exec </dev/null >/dev/null 2>&1 sleep 2147483647";

const TOKEN: &str = "arm";

/// The whole acknowledgement. The anchor closes its stdout after it,
/// so it is read to EOF, once.
const ACK: &[u8] = b"armed\n";

/// How long an anchor has to acknowledge its activation.
pub(crate) const ARM_DEADLINE: Duration = Duration::from_secs(5);

/// What a host runs as an anchor, what it says to arm it, and how long
/// it waits to hear back.
pub(crate) struct Arming {
    pub(crate) program: Vec<String>,
    pub(crate) token: Vec<u8>,
    pub(crate) deadline: Duration,
}

impl Arming {
    pub(crate) fn standard() -> Self {
        Self {
            program: anchor_program(),
            token: activation(),
            deadline: ARM_DEADLINE,
        }
    }
}

pub(crate) fn anchor_program() -> Vec<String> {
    ["/bin/sh", "-c", ANCHOR, "clank-anchor", TOKEN]
        .map(String::from)
        .to_vec()
}

pub(crate) fn activation() -> Vec<u8> {
    format!("{TOKEN}\n").into_bytes()
}

/// Every producer group this process owns, from admission until it is
/// confirmed ended.
///
/// Process-wide because the exits that matter most do not unwind: a
/// SIGTERM, the watchdog and `process::exit` run no destructor, and
/// may find the runtime wedged. Taking an entry out of `live` IS the
/// right to retire it, so each attempt runs once, by whichever path
/// reaches it first, and the others wait for it. Admission, record
/// publication and spawning into a group all happen under the same
/// lock, so none of them can interleave with a retirement taking the
/// entry.
pub(crate) struct Owners {
    table: Mutex<Table>,
    settled: Condvar,
    /// Ends a group, escalating to KILL after the given grace.
    retirement: fn(&Entry, Duration) -> Retired,
}

struct Table {
    closing: bool,
    next: u64,
    live: BTreeMap<u64, Entry>,
    retiring: BTreeSet<u64>,
    /// Groups whose ordinary retirement deferred after shutdown began:
    /// shutdown's to retire, once, since `live` admits nothing more.
    handed_to_shutdown: BTreeMap<u64, Entry>,
    drain: Option<DrainPhase>,
    /// Hosts still reading a producer's output, by its record.
    readers: BTreeMap<PathBuf, usize>,
}

struct DrainPhase {
    cutoff: std::time::Instant,
    outcomes: Vec<(PathBuf, Drained)>,
    /// Every group this process owned has been through its retirement.
    retired: bool,
    /// Rung when a retirement finishes or a reader stops.
    wake: Arc<tokio::sync::Notify>,
}

/// One owned group, and what ending it needs.
///
/// Holds the source's lease, so a group whose retirement deferred
/// keeps its source from being replaced for as long as this process
/// still owns it.
pub(crate) struct Entry {
    path: PathBuf,
    record: Producer,
    children: Vec<Pid>,
    _lease: Arc<IngestLease>,
    /// A retirement already ran and could not confirm it ended. Only
    /// these are anybody else's to retry: an entry without it is a
    /// running producer some host is serving.
    deferred: bool,
}

fn retire_natively(entry: &Entry, escalate_after: Duration) -> Retired {
    cs::retire_after(
        &entry.path,
        &entry.record,
        &cs::Native {
            children: &entry.children,
        },
        cs::Whose::Own,
        escalate_after,
    )
}

pub(crate) static OWNERS: Owners = Owners::new();

impl Owners {
    pub(crate) const fn new() -> Self {
        Self::retiring_by(retire_natively)
    }

    const fn retiring_by(retirement: fn(&Entry, Duration) -> Retired) -> Self {
        Self {
            table: Mutex::new(Table {
                closing: false,
                next: 0,
                live: BTreeMap::new(),
                retiring: BTreeSet::new(),
                handed_to_shutdown: BTreeMap::new(),
                drain: None,
                readers: BTreeMap::new(),
            }),
            settled: Condvar::new(),
            retirement,
        }
    }

    fn table(&self) -> MutexGuard<'_, Table> {
        self.table.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn wait<'a>(&self, table: MutexGuard<'a, Table>) -> MutexGuard<'a, Table> {
        self.settled
            .wait(table)
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Admit a group and publish its record, or refuse both once this
    /// process has begun to exit. A failed publication still admits
    /// it, so the returned error's `Owned` has already retired it.
    fn admit(
        &'static self,
        entry: Entry,
        pgid: Pgid,
        store_dir: &Path,
        key: &str,
    ) -> std::io::Result<Owned> {
        let mut table = self.table();
        if table.closing {
            return Err(std::io::Error::other(Closed));
        }
        let published = cs::record_producer(store_dir, key, &entry.record);
        let id = table.next;
        table.next += 1;
        table.live.insert(id, entry);
        drop(table);
        let owned = Owned {
            owners: self,
            id,
            pgid,
            done: false,
        };
        published.map(|()| owned)
    }

    /// Spawn `command` into a group still owned and not being retired,
    /// and note it as a child to reap. Refused once retirement has
    /// taken the group: a consumer started into a group being ended is
    /// one nothing would account for.
    fn spawn_into(
        &self,
        id: u64,
        pgid: Pgid,
        command: &mut tokio::process::Command,
    ) -> std::io::Result<tokio::process::Child> {
        let mut table = self.table();
        if table.closing {
            return Err(std::io::Error::other(Closed));
        }
        let Some(entry) = table.live.get_mut(&id).filter(|entry| !entry.deferred) else {
            return Err(std::io::Error::other("its group is being retired"));
        };
        command.process_group(pgid.get());
        let child = command.spawn()?;
        if let Some(pid) = child.id().and_then(|pid| Pid::new(pid as i32)) {
            entry.children.push(pid);
        }
        Ok(child)
    }

    /// `None` when this process no longer owns the group — it was
    /// confirmed ended, or the exit path took it. A deferral puts it
    /// back, still owned.
    fn retire(&self, id: u64) -> Option<Retired> {
        let mut table = self.table();
        loop {
            if let Some(entry) = table.live.remove(&id) {
                table.retiring.insert(id);
                drop(table);
                let out = self.retire_entry(&entry);
                let mut table = self.table();
                table.retiring.remove(&id);
                if matches!(out, Retired::Deferred(_)) {
                    let entry = Entry {
                        deferred: true,
                        ..entry
                    };
                    if table.closing {
                        table.handed_to_shutdown.insert(id, entry);
                    } else {
                        table.live.insert(id, entry);
                    }
                }
                drop(table);
                self.settled.notify_all();
                return Some(out);
            }
            if !table.retiring.contains(&id) {
                return None;
            }
            table = self.wait(table);
        }
    }

    /// Try again to end any group recorded at `path` whose retirement
    /// deferred. Run where a host would otherwise just wait on the
    /// lease that group still holds.
    pub(crate) fn retry_deferred(&self, path: &Path) {
        let ids: Vec<u64> = self
            .table()
            .live
            .iter()
            .filter(|(_, entry)| entry.deferred && entry.path == path)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            if let Some(Retired::Deferred(why)) = self.retire(id) {
                eprintln!("clank: a command source's producer is still not retired: {why}");
            }
        }
    }

    /// Retire every group, admit no more, and return only once every
    /// group is accounted for.
    ///
    /// Closing is a terminal transition, not a snapshot: an ordinary
    /// retirement already in flight may defer afterwards, and its group
    /// then comes here instead of back to `live`. Each group gets one
    /// attempt from here; one that still defers keeps its record for a
    /// successor and is reported, and nothing is retried forever.
    pub(crate) fn retire_all(&self) {
        self.table().closing = true;
        loop {
            let batch = {
                let mut table = self.table();
                loop {
                    let mut batch = std::mem::take(&mut table.live);
                    batch.append(&mut table.handed_to_shutdown);
                    if !batch.is_empty() {
                        table.retiring.extend(batch.keys().copied());
                        break Some(batch);
                    }
                    if table.retiring.is_empty() {
                        break None;
                    }
                    table = self.wait(table);
                }
            };
            let Some(batch) = batch else {
                return;
            };
            std::thread::scope(|scope| {
                for (id, entry) in &batch {
                    scope.spawn(move || {
                        if let Retired::Deferred(why) = self.retire_entry(entry) {
                            eprintln!(
                                "clank: a command source's producer was not retired, and its \
                                 record is left for the next host: {why}"
                            );
                        }
                        self.table().retiring.remove(id);
                        self.settled.notify_all();
                    });
                }
            });
        }
    }

    /// Begin a drain: admit no more groups, ask every one running to
    /// end, all at once, and give them until one shared cutoff `grace`
    /// from now. From here every retirement, whoever runs it, waits for
    /// its group's work to end or the cutoff, whichever is first, and
    /// keeps the group and its source's lease until it is confirmed
    /// ended.
    pub(crate) fn drain(&'static self, grace: Duration) -> Draining {
        {
            let mut table = self.table();
            table.closing = true;
            table.drain = Some(DrainPhase {
                cutoff: std::time::Instant::now() + grace,
                outcomes: Vec::new(),
                retired: false,
                wake: Arc::new(tokio::sync::Notify::new()),
            });
            for entry in table.live.values().filter(|entry| !entry.deferred) {
                cs::ask_to_end(
                    &entry.record,
                    &cs::Native {
                        children: &entry.children,
                    },
                );
            }
        }
        // A thread of its own, not the blocking pool: whether it runs
        // must not depend on that pool having room.
        std::thread::spawn(move || {
            self.retire_all();
            if let Some(drain) = &mut self.table().drain {
                drain.retired = true;
                drain.wake.notify_one();
            }
        });
        Draining { owners: self }
    }

    /// The cutoff of a drain under way.
    pub(crate) fn drain_cutoff(&self) -> Option<std::time::Instant> {
        self.table().drain.as_ref().map(|drain| drain.cutoff)
    }

    /// Bring a drain's cutoff forward to now, for an exit that cannot
    /// wait for it.
    fn cut_off(&self) {
        if let Some(drain) = &mut self.table().drain {
            drain.cutoff = drain.cutoff.min(std::time::Instant::now());
        }
    }

    /// Note that a host is reading `path`'s producer output, until the
    /// guard is dropped. A drain is not over while one still is.
    pub(crate) fn reading(&'static self, path: PathBuf) -> Reading {
        *self.table().readers.entry(path.clone()).or_default() += 1;
        Reading { owners: self, path }
    }

    /// Where a drain stands: complete, or what it is still waiting on.
    fn drain_end(&self) -> DrainEnd {
        let table = self.table();
        let Some(drain) = &table.drain else {
            return DrainEnd::Complete;
        };
        let unconfirmed = drain
            .outcomes
            .iter()
            .filter(|(_, how)| *how == Drained::Unconfirmed)
            .count();
        let reading: usize = table.readers.values().sum();
        if !drain.retired {
            DrainEnd::StillRetiring
        } else if unconfirmed > 0 {
            DrainEnd::Unconfirmed(unconfirmed)
        } else if reading > 0 {
            DrainEnd::StillReading(reading)
        } else {
            DrainEnd::Complete
        }
    }

    /// End `entry`'s group: by this registry's ordinary retirement, or,
    /// during a drain, once its work has ended or the cutoff has come,
    /// recording which.
    fn retire_entry(&self, entry: &Entry) -> Retired {
        if self.table().drain.is_none() {
            return (self.retirement)(entry, cs::ESCALATE_AFTER);
        }
        let how = self.await_work(entry);
        let out = (self.retirement)(entry, Duration::ZERO);
        let how = match out {
            Retired::Deferred(_) => Drained::Unconfirmed,
            _ => how,
        };
        if let Some(drain) = &mut self.table().drain {
            drain.outcomes.push((entry.path.clone(), how));
        }
        out
    }

    /// Wait for every member of `entry`'s group but its anchor to end,
    /// or for the drain's cutoff.
    fn await_work(&self, entry: &Entry) -> Drained {
        use cs::Os;
        let os = cs::Native {
            children: &entry.children,
        };
        let anchor = entry.record.pgid();
        loop {
            os.collect();
            let working = Pgid::new(anchor)
                .and_then(crate::signal::group_members)
                .map(|members| members.iter().any(|pid| *pid != anchor));
            if working == Some(false) {
                return Drained::WithinGrace;
            }
            let now = std::time::Instant::now();
            match self.drain_cutoff() {
                Some(cutoff) if now < cutoff => std::thread::sleep(DRAIN_POLL.min(cutoff - now)),
                _ => return Drained::KilledAtGrace,
            }
        }
    }

    #[cfg(test)]
    fn forget(&self, id: u64) {
        self.table().live.remove(&id);
    }

    /// Whether this registry admits no more groups: the process is
    /// draining or exiting.
    pub(crate) fn closing(&self) -> bool {
        self.table().closing
    }

    #[cfg(test)]
    fn owns(&self, id: u64) -> bool {
        self.table().live.contains_key(&id)
    }
}

const DRAIN_POLL: Duration = Duration::from_millis(50);

/// How a group asked to end by a drain did end.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Drained {
    /// It ended after TERM, before the deadline. That says it exited in
    /// time, not that it finished its work: only a producer that drains
    /// on TERM did that.
    WithinGrace,
    /// Still running at the deadline, so it was killed.
    KilledAtGrace,
    /// Its retirement could not confirm it ended.
    Unconfirmed,
}

/// A drain under way.
pub(crate) struct Draining {
    owners: &'static Owners,
}

/// Whether a drain finished: only `Complete` accounts for every group
/// and every reader it owned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DrainEnd {
    Complete,
    /// The retirement of the groups had not finished.
    StillRetiring,
    /// Groups whose end could not be confirmed.
    Unconfirmed(usize),
    /// Hosts still reading producer output.
    StillReading(usize),
}

/// What a drain did: how each group ended, and whether it finished.
#[derive(Debug)]
pub(crate) struct Drainage {
    pub(crate) end: DrainEnd,
    pub(crate) outcomes: Vec<(PathBuf, Drained)>,
}

impl Draining {
    /// Wait, until `by` at the latest, for every group to be retired and
    /// every host to finish reading its output. Past `by` it waits for
    /// nothing, and says what was still outstanding.
    pub(crate) async fn finish(self, by: tokio::time::Instant) -> Drainage {
        let owners = self.owners;
        let wake = owners
            .table()
            .drain
            .as_ref()
            .map(|drain| drain.wake.clone());
        loop {
            if owners.drain_end() == DrainEnd::Complete {
                break;
            }
            let Some(wake) = &wake else { break };
            // A ring before this wait is kept as a permit, so nothing is
            // missed between the look above and here.
            if tokio::time::timeout_at(by, wake.notified()).await.is_err() {
                break;
            }
        }
        let end = owners.drain_end();
        let outcomes = owners
            .table()
            .drain
            .as_mut()
            .map(|drain| std::mem::take(&mut drain.outcomes))
            .unwrap_or_default();
        Drainage { end, outcomes }
    }
}

/// A host reading a producer's output.
pub(crate) struct Reading {
    owners: &'static Owners,
    path: PathBuf,
}

impl Drop for Reading {
    fn drop(&mut self) {
        let mut table = self.owners.table();
        if let Some(count) = table.readers.get_mut(&self.path) {
            *count -= 1;
            if *count == 0 {
                table.readers.remove(&self.path);
            }
        }
        if let Some(drain) = &table.drain {
            drain.wake.notify_one();
        }
    }
}

impl std::fmt::Display for Drained {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Drained::WithinGrace => "ended within grace",
            Drained::KilledAtGrace => "killed at grace",
            Drained::Unconfirmed => "not confirmed ended",
        })
    }
}

/// This process's claim on one producer group, retired when dropped.
pub(crate) struct Owned {
    owners: &'static Owners,
    id: u64,
    pgid: Pgid,
    done: bool,
}

impl Owned {
    #[cfg(test)]
    pub(crate) fn pgid(&self) -> Pgid {
        self.pgid
    }

    /// End the group. A deferral hands the claim back: the group is
    /// still this process's, lease and all, until a later attempt
    /// confirms it ended.
    pub(crate) fn retire(mut self) -> Result<(), (Owned, String)> {
        match self.owners.retire(self.id) {
            Some(Retired::Deferred(why)) => Err((self, why)),
            _ => {
                self.done = true;
                Ok(())
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn abandon(mut self) {
        self.owners.forget(self.id);
        self.done = true;
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        if !self.done
            && let Some(Retired::Deferred(why)) = self.owners.retire(self.id)
        {
            eprintln!(
                "clank: a command source's producer is not yet retired, and stays owned \
                 until it is: {why}"
            );
        }
    }
}

/// (a) An anchor leading a group of its own, unrecorded and unarmed.
pub(crate) struct Spawned {
    anchor: tokio::process::Child,
    pgid: Pgid,
    activation: std::io::PipeWriter,
    /// The host's own copy of the anchor's read end. A write to a pipe
    /// with no reader raises SIGPIPE, which clank leaves at its default
    /// — so without this, an anchor that died before reading would take
    /// the monitor down with it.
    held_reader: std::io::PipeReader,
}

impl Spawned {
    /// The pipe is close-on-exec on the host's side, so no later child
    /// can hold the write end and keep an unarmed anchor alive.
    pub(crate) fn spawn(program: &[String]) -> std::io::Result<Self> {
        let (reader, activation) = std::io::pipe()?;
        let held_reader = reader.try_clone()?;
        let anchor = tokio::process::Command::new(&program[0])
            .args(&program[1..])
            .stdin(Stdio::from(reader))
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()?;
        let pgid = anchor
            .id()
            .and_then(|pid| Pgid::new(pid as i32))
            .ok_or_else(|| std::io::Error::other("the anchor has no pid to lead a group by"))?;
        Ok(Self {
            anchor,
            pgid,
            activation,
            held_reader,
        })
    }

    /// (b) Admitted and recorded in one step that a retirement cannot
    /// interleave with.
    pub(crate) fn record(
        self,
        owners: &'static Owners,
        store_dir: &Path,
        key: &str,
        lease: Arc<IngestLease>,
    ) -> std::io::Result<Recorded> {
        let birth = crate::proc_identity::token_for(self.pgid.get()).ok_or_else(|| {
            std::io::Error::other("the system would not say when the anchor started")
        })?;
        let record = Producer::Anchored {
            pgid: self.pgid.get(),
            birth,
        };
        let owned = owners.admit(
            Entry {
                path: cs::producer_path(store_dir, key),
                record,
                children: Pid::new(self.pgid.get()).into_iter().collect(),
                _lease: lease,
                deferred: false,
            },
            self.pgid,
            store_dir,
            key,
        )?;
        Ok(Recorded {
            owned,
            anchor: self.anchor,
            activation: self.activation,
            held_reader: self.held_reader,
        })
    }

    #[cfg(test)]
    fn abandon(self) -> tokio::process::Child {
        self.anchor
    }
}

/// (b) A recorded group led by an anchor that has not been armed.
pub(crate) struct Recorded {
    owned: Owned,
    anchor: tokio::process::Child,
    activation: std::io::PipeWriter,
    held_reader: std::io::PipeReader,
}

impl Recorded {
    /// (c) Arm the anchor, and believe it only when it says so.
    ///
    /// A successful write proves nothing about whether the anchor took
    /// the token, is alive, or has installed its TERM-ignore. Anything
    /// short of the exact acknowledgement within `deadline` ends the
    /// attempt with no command started, and the group is retired.
    pub(crate) async fn arm(self, token: &[u8], deadline: Duration) -> Result<Armed, String> {
        use std::io::Write;
        use tokio::io::AsyncReadExt;
        let Recorded {
            owned,
            mut anchor,
            mut activation,
            held_reader,
        } = self;
        let Some(ack) = anchor.stdout.take() else {
            return Err("the anchor has no acknowledgement channel".into());
        };
        let written = activation.write_all(token);
        drop(activation);
        drop(held_reader);
        if let Err(e) = written {
            return Err(format!("its activation could not be written ({e})"));
        }
        let mut answer = Vec::new();
        match tokio::time::timeout(
            deadline,
            ack.take(ACK.len() as u64 + 1).read_to_end(&mut answer),
        )
        .await
        {
            Ok(Ok(_)) if answer == ACK => Ok(Armed {
                owned,
                #[cfg(test)]
                anchor,
            }),
            Ok(Ok(_)) => Err(format!(
                "it answered {:?} instead of acknowledging",
                String::from_utf8_lossy(&answer)
            )),
            Ok(Err(e)) => Err(format!("its acknowledgement could not be read ({e})")),
            Err(_) => Err(format!("it did not acknowledge within {deadline:?}")),
        }
    }

    #[cfg(test)]
    fn abandon(self) -> tokio::process::Child {
        self.owned.abandon();
        self.anchor
    }

    #[cfg(test)]
    fn abandon_after_activating(mut self, token: &[u8]) -> tokio::process::Child {
        use std::io::Write;
        self.activation.write_all(token).unwrap();
        self.anchor.stdout.take();
        self.abandon()
    }
}

/// (c) An armed anchor whose group nothing else has joined yet.
pub(crate) struct Armed {
    owned: Owned,
    #[cfg(test)]
    anchor: tokio::process::Child,
}

impl Armed {
    /// (d) Start `command` in the anchor's group, unless retirement has
    /// already taken it.
    ///
    /// Spawned by the host rather than by the anchor, so it starts with
    /// default dispositions instead of inheriting the anchor's ignored
    /// TERM.
    pub(crate) fn spawn(
        self,
        command: &mut tokio::process::Command,
    ) -> std::io::Result<(Owned, tokio::process::Child)> {
        let child = self
            .owned
            .owners
            .spawn_into(self.owned.id, self.owned.pgid, command)?;
        Ok((self.owned, child))
    }

    #[cfg(test)]
    fn abandon(self) -> tokio::process::Child {
        self.owned.abandon();
        self.anchor
    }
}

/// Start `command` as a source's producer, in a group recorded and
/// armed before it runs.
pub(crate) async fn launch(
    owners: &'static Owners,
    arming: &Arming,
    store_dir: &Path,
    key: &str,
    lease: Arc<IngestLease>,
    command: &mut tokio::process::Command,
) -> Result<(Owned, tokio::process::Child), LaunchError> {
    if owners.closing() {
        return Err(LaunchError::Closed);
    }
    // Whatever stopped it, a launch the registry closed on was not the
    // source failing.
    let failed = |why: String| match owners.closing() {
        true => LaunchError::Closed,
        false => LaunchError::Failed(why),
    };
    let spawned = Spawned::spawn(&arming.program)
        .map_err(|e| failed(format!("its anchor could not start ({e})")))?;
    let recorded =
        spawned
            .record(owners, store_dir, key, lease)
            .map_err(|e| match refused(&e) {
                true => LaunchError::Closed,
                false => failed(format!("its group could not be recorded ({e})")),
            })?;
    let armed = recorded
        .arm(&arming.token, arming.deadline)
        .await
        .map_err(|e| failed(format!("its anchor was not armed: {e}")))?;
    armed.spawn(command).map_err(|e| match refused(&e) {
        true => LaunchError::Closed,
        false => failed(format!("it failed to spawn ({e})")),
    })
}

/// Why a producer did not start.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LaunchError {
    /// The registry admits no more groups: this process is draining or
    /// exiting. Not the source failing.
    Closed,
    Failed(String),
}

impl std::fmt::Display for LaunchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LaunchError::Closed => f.write_str("this process is exiting"),
            LaunchError::Failed(why) => f.write_str(why),
        }
    }
}

/// Whether `e` is the registry refusing because it is closing.
fn refused(e: &std::io::Error) -> bool {
    e.get_ref().is_some_and(|inner| inner.is::<Closed>())
}

/// The registry's refusal once it is closing, told apart from every
/// other launch error by its type.
#[derive(Debug)]
struct Closed;

impl std::fmt::Display for Closed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("this process is exiting")
    }
}

impl std::error::Error for Closed {}

/// Retire this process's groups when it is told to stop, then stop as
/// that signal would have stopped it.
///
/// A thread, not a task: the runtime is what may be wedged.
pub(crate) fn retire_on_termination() {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| {
        use signal_hook::consts::{SIGINT, SIGTERM};
        match signal_hook::iterator::Signals::new([SIGTERM, SIGINT]) {
            Ok(mut signals) => {
                std::thread::spawn(move || {
                    if let Some(sig) = signals.forever().next() {
                        OWNERS.retire_all();
                        let _ = signal_hook::low_level::emulate_default_handler(sig);
                        std::process::exit(128 + sig);
                    }
                });
            }
            Err(e) => eprintln!(
                "clank: cannot watch for termination ({e}); a SIGTERM will leave this \
                 process's producers for its successor to end"
            ),
        }
    });
}

/// `process::exit`, after this process's producer groups are retired.
///
/// A bare `process::exit` runs no destructor, so every deliberate exit
/// of a process that may host sources goes through here.
pub fn exit(code: i32) -> ! {
    OWNERS.cut_off();
    OWNERS.retire_all();
    std::process::exit(code)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::cli::command_source::{Group, Leader, group_of, leader_of};
    use crate::signal::Probe;

    pub(crate) fn owners() -> &'static Owners {
        Box::leak(Box::new(Owners::new()))
    }

    /// Owners whose drain retirements end the group but cannot confirm
    /// it.
    pub(crate) fn owners_unconfirmed_in_a_drain() -> &'static Owners {
        fn ends_but_cannot_confirm(entry: &Entry, escalate_after: Duration) -> Retired {
            let out = retire_natively(entry, escalate_after);
            if escalate_after.is_zero() {
                return Retired::Deferred("cannot confirm it ended".into());
            }
            out
        }
        Box::leak(Box::new(Owners::retiring_by(ends_but_cannot_confirm)))
    }

    fn lease(dir: &Path, key: &str) -> Arc<IngestLease> {
        Arc::new(IngestLease::try_acquire(dir, key).unwrap().unwrap())
    }

    /// Reap `pid` the way init reaps a dead host's orphans, so a
    /// successor in this same process sees what it would see in life.
    pub(crate) fn init_stands_in_for(pid: i32) {
        std::thread::spawn(move || {
            let mut status = 0;
            // SAFETY: a blocking wait on one pid; `status` is valid.
            unsafe { libc::waitpid(pid, &mut status, 0) };
        });
    }

    pub(crate) fn sh(script: &str) -> tokio::process::Command {
        let mut command = tokio::process::Command::new("sh");
        command
            .args(["-c", script])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
    }

    fn fake_anchor(script: &str) -> Vec<String> {
        ["/bin/sh", "-c", script, "fake-anchor", TOKEN]
            .map(String::from)
            .to_vec()
    }

    pub(crate) async fn pid_in(file: &Path) -> i32 {
        for _ in 0..500 {
            if let Ok(text) = std::fs::read_to_string(file)
                && let Ok(pid) = text.trim().parse()
            {
                return pid;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("{} was never written", file.display());
    }

    pub(crate) async fn gone(pid: i32) -> bool {
        for _ in 0..500 {
            if crate::signal::probe_process(pid) == Probe::Gone {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        false
    }

    async fn successor(dir: &Path, key: &str) -> Retired {
        let (dir, key) = (dir.to_path_buf(), key.to_string());
        tokio::task::spawn_blocking(move || cs::reap_previous_producer(&dir, &key))
            .await
            .unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_host_that_dies_before_the_record_leaves_an_anchor_that_ends_itself() {
        let dir = tempfile::tempdir().unwrap();
        let mut anchor = Spawned::spawn(&anchor_program()).unwrap().abandon();
        let status = tokio::time::timeout(Duration::from_secs(10), anchor.wait())
            .await
            .expect("an unarmed anchor must end on EOF, with no successor's help")
            .unwrap();
        assert!(!status.success(), "it ends refusing, not armed: {status:?}");
        assert!(
            !cs::producer_path(dir.path(), "src").exists(),
            "and there was never a record to recover"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_host_that_dies_before_arming_leaves_a_record_its_successor_spends() {
        let dir = tempfile::tempdir().unwrap();
        let recorded = Spawned::spawn(&anchor_program())
            .unwrap()
            .record(owners(), dir.path(), "src", lease(dir.path(), "src"))
            .unwrap();
        let mut anchor = recorded.abandon();
        tokio::time::timeout(Duration::from_secs(10), anchor.wait())
            .await
            .expect("the unarmed anchor ends on EOF")
            .unwrap();
        assert!(cs::producer_path(dir.path(), "src").exists());

        assert_eq!(successor(dir.path(), "src").await, Retired::Clear);
        assert!(!cs::producer_path(dir.path(), "src").exists());
    }

    /// The anchor may have read the token and armed before the host
    /// died, or found its acknowledgement unreadable and ended. Both
    /// are recoverable, and neither may leave anything running.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_host_that_dies_while_arming_leaves_nothing_running() {
        let dir = tempfile::tempdir().unwrap();
        let recorded = Spawned::spawn(&anchor_program())
            .unwrap()
            .record(owners(), dir.path(), "src", lease(dir.path(), "src"))
            .unwrap();
        let pgid = recorded.owned.pgid();
        drop(recorded.abandon_after_activating(&activation()));
        init_stands_in_for(pgid.get());

        assert_eq!(successor(dir.path(), "src").await, Retired::Clear);
        assert_eq!(group_of(pgid), Group::Empty);
        assert!(!cs::producer_path(dir.path(), "src").exists());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_host_that_dies_armed_leaves_a_group_its_successor_identifies_and_ends() {
        let dir = tempfile::tempdir().unwrap();
        let armed = Spawned::spawn(&anchor_program())
            .unwrap()
            .record(owners(), dir.path(), "src", lease(dir.path(), "src"))
            .unwrap()
            .arm(&activation(), ARM_DEADLINE)
            .await
            .unwrap();
        let pgid = armed.owned.pgid();
        drop(armed.abandon());
        init_stands_in_for(pgid.get());
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            crate::signal::probe_process(pgid.get()),
            Probe::Ours,
            "an ARMED anchor outlives its host"
        );

        assert_eq!(successor(dir.path(), "src").await, Retired::Clear);
        assert_eq!(group_of(pgid), Group::Empty);
        assert!(!cs::producer_path(dir.path(), "src").exists());
    }

    /// Driven through `launch`, the host's own path, with a command
    /// that would leave a mark if it ever ran.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_activation_short_of_the_exact_acknowledgement_starts_nothing() {
        let cases: Vec<(&str, Vec<String>, Vec<u8>, Duration)> = vec![
            (
                "rejected",
                anchor_program(),
                b"nope\n".to_vec(),
                ARM_DEADLINE,
            ),
            ("incomplete", anchor_program(), b"ar".to_vec(), ARM_DEADLINE),
            ("closed early", anchor_program(), Vec::new(), ARM_DEADLINE),
            (
                "anchor exits",
                fake_anchor("exit 0"),
                activation(),
                ARM_DEADLINE,
            ),
            (
                "anchor never answers",
                fake_anchor("exec sleep 30"),
                activation(),
                Duration::from_millis(300),
            ),
            (
                "anchor answers wrongly",
                fake_anchor("read t; printf 'armed!\\n'"),
                activation(),
                ARM_DEADLINE,
            ),
        ];
        for (case, program, token, deadline) in cases {
            let dir = tempfile::tempdir().unwrap();
            let marker = dir.path().join("ran");
            let mut command = sh(&format!("touch '{}'", marker.display()));
            let out = launch(
                owners(),
                &Arming {
                    program,
                    token,
                    deadline,
                },
                dir.path(),
                "src",
                lease(dir.path(), "src"),
                &mut command,
            )
            .await;
            assert!(out.is_err(), "{case}: launched");
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert!(!marker.exists(), "{case}: the command ran");
            assert!(
                !cs::producer_path(dir.path(), "src").exists(),
                "{case}: the attempt was not retired"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_delayed_acknowledgement_starts_nothing_until_it_arrives() {
        let dir = tempfile::tempdir().unwrap();
        let gate = dir.path().join("gate");
        let made = std::process::Command::new("mkfifo")
            .arg(&gate)
            .status()
            .unwrap();
        assert!(made.success());
        let marker = dir.path().join("ran");
        let program = fake_anchor(&format!(
            "trap '' TERM; IFS= read -r t; read go < '{}'; printf 'armed\\n'; \
             exec </dev/null >/dev/null sleep 300",
            gate.display()
        ));
        let task = {
            let dir = dir.path().to_path_buf();
            let marker = marker.clone();
            tokio::spawn(async move {
                let mut command = sh(&format!("touch '{}'; exec sleep 300", marker.display()));
                launch(
                    owners(),
                    &Arming {
                        program,
                        ..Arming::standard()
                    },
                    &dir,
                    "src",
                    lease(&dir, "src"),
                    &mut command,
                )
                .await
            })
        };
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(!task.is_finished(), "launched without an acknowledgement");
        assert!(
            !marker.exists(),
            "the command ran before its group was armed"
        );

        std::fs::write(&gate, b"go\n").unwrap();
        let (owned, _child) = tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap()
            .expect("an acknowledgement inside the deadline arms the group");
        for _ in 0..500 {
            if marker.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(marker.exists(), "and the command runs once it is armed");
        let retire = tokio::task::spawn_blocking(move || owned.retire());
        assert!(retire.await.unwrap().is_ok());
    }

    /// A child the host starts later must not hold the activation
    /// pipe's write end, or it would keep an unarmed anchor waiting
    /// for as long as that child lives.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_later_child_cannot_keep_an_unarmed_anchor_alive() {
        let spawned = Spawned::spawn(&anchor_program()).unwrap();
        let mut bystander = sh("exec sleep 300").kill_on_drop(true).spawn().unwrap();
        let mut anchor = spawned.abandon();
        tokio::time::timeout(Duration::from_secs(10), anchor.wait())
            .await
            .expect("the anchor saw no EOF: a later child holds its write end")
            .unwrap();
        bystander.kill().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unrecordable_group_is_ended_before_anything_runs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(
            cs::producer_path(dir.path(), "src").with_extension("producer.tmp"),
        )
        .unwrap();
        let spawned = Spawned::spawn(&anchor_program()).unwrap();
        let pgid = spawned.pgid;
        let out = tokio::task::spawn_blocking({
            let dir = dir.path().to_path_buf();
            move || {
                spawned
                    .record(owners(), &dir, "src", lease(&dir, "src"))
                    .map(drop)
            }
        })
        .await
        .unwrap();
        assert!(
            out.is_err(),
            "the injected recording failure must be reached"
        );
        assert!(
            gone(pgid.get()).await,
            "an unrecorded anchor was left running"
        );
        assert_eq!(group_of(pgid), Group::Empty);
    }

    async fn launched(
        owners: &'static Owners,
        dir: &Path,
        script: &str,
    ) -> (Owned, tokio::process::Child) {
        let mut command = sh(script);
        launch(
            owners,
            &Arming::standard(),
            dir,
            "src",
            lease(dir, "src"),
            &mut command,
        )
        .await
        .unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_command_runs_in_its_anchors_group_with_term_at_its_default() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let (owned, mut child) = launched(
            owners(),
            dir.path(),
            &format!("echo $$ > '{}'; exec sleep 300", pidfile.display()),
        )
        .await;
        let pid = pid_in(&pidfile).await;
        // SAFETY: a plain query, no memory effects.
        assert_eq!(unsafe { libc::getpgid(pid) }, owned.pgid().get());

        crate::signal::signal_process(Pid::new(pid).unwrap(), libc::SIGTERM);
        tokio::time::timeout(Duration::from_secs(10), child.wait())
            .await
            .expect("the anchor's ignored TERM leaked into the command")
            .unwrap();
        let retire = tokio::task::spawn_blocking(move || owned.retire());
        assert!(retire.await.unwrap().is_ok());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_leader_that_exits_leaves_a_group_still_identified_and_ended() {
        for host_dies in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let pidfile = dir.path().join("pid");
            let (owned, mut child) = launched(
                owners(),
                dir.path(),
                &format!("sleep 300 & echo $! > '{}'", pidfile.display()),
            )
            .await;
            let descendant = pid_in(&pidfile).await;
            child.wait().await.unwrap();
            assert_eq!(crate::signal::probe_process(descendant), Probe::Ours);
            let record: Producer = serde_json::from_slice(
                &std::fs::read(cs::producer_path(dir.path(), "src")).unwrap(),
            )
            .unwrap();
            assert_eq!(leader_of(&record), Leader::Same, "the anchor still leads");

            let out = if host_dies {
                let anchor = owned.pgid().get();
                owned.abandon();
                init_stands_in_for(anchor);
                successor(dir.path(), "src").await
            } else {
                let ended = tokio::task::spawn_blocking(move || owned.retire().is_ok());
                assert!(ended.await.unwrap());
                Retired::Clear
            };
            assert_eq!(out, Retired::Clear, "host dies: {host_dies}");
            assert!(gone(descendant).await, "host dies: {host_dies}");
            assert!(!cs::producer_path(dir.path(), "src").exists());
        }
    }

    /// The host has sent its TERM and dies inside the grace period.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_descendant_that_ignores_term_is_ended_by_the_successor() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let (owned, child) = launched(
            owners(),
            dir.path(),
            &format!(
                "trap '' TERM; echo $$ > '{}'; exec sleep 300",
                pidfile.display()
            ),
        )
        .await;
        let stubborn = pid_in(&pidfile).await;
        let pgid = owned.pgid();
        crate::signal::signal_group(pgid, libc::SIGTERM);
        owned.abandon();
        drop(child);
        init_stands_in_for(pgid.get());
        init_stands_in_for(stubborn);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(crate::signal::probe_process(stubborn), Probe::Ours);
        assert_eq!(
            crate::signal::probe_process(pgid.get()),
            Probe::Ours,
            "the anchor must survive the TERM and its host's death"
        );

        assert_eq!(successor(dir.path(), "src").await, Retired::Clear);
        assert!(gone(stubborn).await);
        assert_eq!(group_of(pgid), Group::Empty);
    }

    static RETIRING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    static RELEASED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    fn held_open_until_released(entry: &Entry, _: Duration) -> Retired {
        use std::sync::atomic::Ordering;
        RETIRING.store(true, Ordering::Release);
        while !RELEASED.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(5));
        }
        retire_natively(entry, cs::ESCALATE_AFTER)
    }

    /// Shutdown takes an armed, acknowledged group, and its anchor is
    /// still alive, when the host reaches the command's spawn. The
    /// spawn must refuse rather than start a consumer in a group that
    /// is being ended.
    #[tokio::test(flavor = "multi_thread")]
    async fn retirement_that_wins_the_race_to_an_armed_group_starts_nothing() {
        use std::sync::atomic::Ordering;
        let owners: &'static Owners =
            Box::leak(Box::new(Owners::retiring_by(held_open_until_released)));
        let dir = tempfile::tempdir().unwrap();
        let armed = Spawned::spawn(&anchor_program())
            .unwrap()
            .record(owners, dir.path(), "src", lease(dir.path(), "src"))
            .unwrap()
            .arm(&activation(), ARM_DEADLINE)
            .await
            .unwrap();
        let anchor = armed.owned.pgid.get();
        let shutdown = tokio::task::spawn_blocking(|| owners.retire_all());
        while !RETIRING.load(Ordering::Acquire) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            crate::signal::probe_process(anchor),
            Probe::Ours,
            "the race needs a live group to spawn into"
        );

        // A refused spawn drops its claim, and that waits for the very
        // retirement this test is holding open.
        std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(300));
            RELEASED.store(true, Ordering::Release);
        });
        let marker = dir.path().join("ran");
        let spawned = armed.spawn(&mut sh(&format!("touch '{}'", marker.display())));
        shutdown.await.unwrap();
        assert!(
            spawned.is_err(),
            "a consumer was started into a group being retired"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!marker.exists());
        assert!(gone(anchor).await);

        let late = tempfile::tempdir().unwrap();
        let refused = Spawned::spawn(&anchor_program()).unwrap().record(
            owners,
            late.path(),
            "src",
            lease(late.path(), "src"),
        );
        assert!(refused.is_err());
        assert!(
            !cs::producer_path(late.path(), "src").exists(),
            "a record published after the last retirement names a group nobody ends"
        );
    }

    static DEFER: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

    fn deferring_while_asked(entry: &Entry, _: Duration) -> Retired {
        if DEFER.load(std::sync::atomic::Ordering::Acquire) {
            Retired::Deferred("held for the test".into())
        } else {
            retire_natively(entry, cs::ESCALATE_AFTER)
        }
    }

    /// A retirement that cannot confirm the group ended keeps it owned
    /// — lease included — until one that can.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_deferred_retirement_keeps_the_group_and_its_lease_until_one_succeeds() {
        let owners: &'static Owners =
            Box::leak(Box::new(Owners::retiring_by(deferring_while_asked)));
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let (owned, _child) = launched(
            owners,
            dir.path(),
            &format!("echo $$ > '{}'; exec sleep 300", pidfile.display()),
        )
        .await;
        let producer = pid_in(&pidfile).await;
        let id = owned.id;
        let held = || {
            IngestLease::try_acquire(dir.path(), "src")
                .unwrap()
                .is_none()
        };

        let (owned, _) = owned
            .retire()
            .expect_err("the deferral hands the claim back");
        assert!(owners.owns(id));
        drop(owned);
        assert!(
            owners.owns(id),
            "dropping a deferred claim forgot the group"
        );
        assert!(
            held(),
            "the lease went with a group that may still be running"
        );
        assert_eq!(crate::signal::probe_process(producer), Probe::Ours);

        DEFER.store(false, std::sync::atomic::Ordering::Release);
        let path = cs::producer_path(dir.path(), "src");
        tokio::task::spawn_blocking(move || owners.retry_deferred(&path))
            .await
            .unwrap();
        assert!(!owners.owns(id));
        // A fork elsewhere in this test process inherits the lease's
        // descriptor until its exec closes it, holding the flock for
        // that moment; only a lease held past it was not released.
        let mut released = false;
        for _ in 0..200 {
            if !held() {
                released = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(released, "a confirmed retirement releases the lease");
        assert!(gone(producer).await);
        assert!(!cs::producer_path(dir.path(), "src").exists());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn retrying_deferred_retirements_leaves_a_running_group_alone() {
        let owners = owners();
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let (owned, _child) = launched(
            owners,
            dir.path(),
            &format!("echo $$ > '{}'; exec sleep 300", pidfile.display()),
        )
        .await;
        let producer = pid_in(&pidfile).await;
        let path = cs::producer_path(dir.path(), "src");
        tokio::task::spawn_blocking(move || owners.retry_deferred(&path))
            .await
            .unwrap();
        assert_eq!(crate::signal::probe_process(producer), Probe::Ours);
        assert!(owners.owns(owned.id));
        assert!(
            tokio::task::spawn_blocking(move || owned.retire().is_ok())
                .await
                .unwrap()
        );
    }

    static FIRST_ENTERED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    static FIRST_RELEASED: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);
    static ATTEMPTS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    static FINAL_DEFERS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    /// The first attempt is held until released and then defers; the
    /// next succeeds, or defers too when `FINAL_DEFERS` is set.
    fn first_held_then_deferred(entry: &Entry, _: Duration) -> Retired {
        use std::sync::atomic::Ordering;
        if ATTEMPTS.fetch_add(1, Ordering::AcqRel) == 0 {
            FIRST_ENTERED.store(true, Ordering::Release);
            while !FIRST_RELEASED.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(5));
            }
            return Retired::Deferred("the first look failed".into());
        }
        if FINAL_DEFERS.load(Ordering::Acquire) {
            Retired::Deferred("still cannot tell".into())
        } else {
            retire_natively(entry, cs::ESCALATE_AFTER)
        }
    }

    /// Shutdown begins while an ordinary retirement is in flight, and
    /// that attempt then defers. The group is shutdown's from there:
    /// it must not return until it has made its own attempt.
    #[tokio::test(flavor = "multi_thread")]
    async fn shutdown_takes_a_group_whose_inflight_retirement_defers() {
        use std::sync::atomic::Ordering;
        for final_defers in [false, true] {
            FIRST_ENTERED.store(false, Ordering::Release);
            FIRST_RELEASED.store(false, Ordering::Release);
            ATTEMPTS.store(0, Ordering::Release);
            FINAL_DEFERS.store(final_defers, Ordering::Release);
            let owners: &'static Owners =
                Box::leak(Box::new(Owners::retiring_by(first_held_then_deferred)));
            let dir = tempfile::tempdir().unwrap();
            let pidfile = dir.path().join("pid");
            let (owned, child) = launched(
                owners,
                dir.path(),
                &format!("echo $$ > '{}'; exec sleep 300", pidfile.display()),
            )
            .await;
            let producer = pid_in(&pidfile).await;
            let anchor = owned.pgid().get();
            let held = || {
                IngestLease::try_acquire(dir.path(), "src")
                    .unwrap()
                    .is_none()
            };

            let ordinary = tokio::task::spawn_blocking(move || owned.retire().is_ok());
            while !FIRST_ENTERED.load(Ordering::Acquire) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            let shutdown = tokio::task::spawn_blocking(move || owners.retire_all());
            while !owners.closing() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert!(
                !shutdown.is_finished(),
                "shutdown returned around an attempt in flight"
            );
            assert!(held());

            FIRST_RELEASED.store(true, Ordering::Release);
            tokio::time::timeout(Duration::from_secs(20), shutdown)
                .await
                .expect("shutdown is bounded, whatever the final attempt says")
                .unwrap();
            assert_eq!(
                ATTEMPTS.load(Ordering::Acquire),
                2,
                "final defers: {final_defers} — shutdown made no attempt of its own"
            );
            assert!(!ordinary.await.unwrap(), "the first attempt deferred");
            let record = cs::producer_path(dir.path(), "src");
            if final_defers {
                assert!(
                    record.exists(),
                    "the evidence a successor needs was dropped"
                );
                let left: Producer =
                    serde_json::from_slice(&std::fs::read(&record).unwrap()).unwrap();
                let children = [Pid::new(anchor).unwrap(), Pid::new(producer).unwrap()];
                let cleaned = cs::retire(
                    &record,
                    &left,
                    &cs::Native {
                        children: &children,
                    },
                    cs::Whose::Own,
                );
                assert_eq!(cleaned, Retired::Clear);
            } else {
                assert!(!held(), "the lease outlived a confirmed retirement");
                assert!(!record.exists());
                assert!(gone(producer).await);
            }
            drop(child);
        }
    }

    /// What a SIGTERM, SIGINT, the watchdog or `exit` runs.
    #[tokio::test(flavor = "multi_thread")]
    async fn retiring_everything_ends_every_group_and_admits_no_more() {
        let owners = owners();
        let dir = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let mut groups = Vec::new();
        for dir in [dir.path(), other.path()] {
            let pidfile = dir.join("pid");
            let (owned, child) = launched(
                owners,
                dir,
                &format!("echo $$ > '{}'; exec sleep 300", pidfile.display()),
            )
            .await;
            groups.push((owned, child, pid_in(&pidfile).await));
        }

        tokio::task::spawn_blocking(|| owners.retire_all())
            .await
            .unwrap();
        for (_, _, pid) in &groups {
            assert!(gone(*pid).await);
        }
        for dir in [dir.path(), other.path()] {
            assert!(!cs::producer_path(dir, "src").exists());
        }
        for (owned, _, _) in groups {
            assert!(
                owned.retire().is_ok(),
                "retired once, by whoever got there first"
            );
        }

        let late = tempfile::tempdir().unwrap();
        let marker = late.path().join("ran");
        let mut command = sh(&format!("touch '{}'", marker.display()));
        let out = launch(
            owners,
            &Arming::standard(),
            late.path(),
            "src",
            lease(late.path(), "src"),
            &mut command,
        )
        .await;
        assert!(
            out.is_err(),
            "a group admitted after the last retirement outlives it"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!marker.exists());
    }

    /// A host's loop around one launched producer: when its command
    /// exits, the group is retired, as `wait`'s host does.
    fn hosted(owned: Owned, mut child: tokio::process::Child) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let _ = child.wait().await;
            let _ = tokio::task::spawn_blocking(move || owned.retire()).await;
        })
    }

    /// One deadline for all: two producers that each keep working for
    /// longer than the old 200 ms escalation, and longer than the old
    /// 3 s budget, both finish inside a grace shorter than their serial
    /// sum; the one that ignores TERM is killed at the deadline. Each
    /// source's lease is held until its producer has ended.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_drain_gives_every_producer_one_shared_grace() {
        let owners = owners();
        let grace = Duration::from_secs(4);
        let mut dirs = Vec::new();
        let mut hosts = Vec::new();
        for script in [
            "trap 'sleep 3.5; touch done; exit 0' TERM; echo $$ > pid; while :; do sleep 0.1; done",
            "trap 'sleep 3.5; touch done; exit 0' TERM; echo $$ > pid; while :; do sleep 0.1; done",
            "trap '' TERM; echo $$ > pid; while :; do sleep 0.1; done",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let mut command = sh(script);
            command.current_dir(dir.path());
            let (owned, child) = launch(
                owners,
                &Arming::standard(),
                dir.path(),
                "src",
                lease(dir.path(), "src"),
                &mut command,
            )
            .await
            .unwrap();
            hosts.push(hosted(owned, child));
            dirs.push(dir);
        }
        for dir in &dirs {
            pid_in(&dir.path().join("pid")).await;
        }
        let stubborn = pid_in(&dirs[2].path().join("pid")).await;

        let started = std::time::Instant::now();
        let draining = owners.drain(grace);
        tokio::time::sleep(Duration::from_secs(1)).await;
        for dir in &dirs {
            assert!(
                IngestLease::try_acquire(dir.path(), "src")
                    .unwrap()
                    .is_none(),
                "a source's lease was let go while its producer still ran"
            );
        }
        let late = tempfile::tempdir().unwrap();
        let relaunched = launch(
            owners,
            &Arming::standard(),
            late.path(),
            "src",
            lease(late.path(), "src"),
            &mut sh("exec sleep 300"),
        )
        .await;
        assert!(relaunched.is_err(), "a draining host started new work");
        let drainage = draining
            .finish(tokio::time::Instant::now() + grace + Duration::from_secs(2))
            .await;
        assert_eq!(drainage.end, DrainEnd::Complete);
        let mut drained = drainage.outcomes;
        let took = started.elapsed();
        drained.sort();
        let mut want: Vec<_> = dirs
            .iter()
            .map(|dir| cs::producer_path(dir.path(), "src"))
            .zip([
                Drained::WithinGrace,
                Drained::WithinGrace,
                Drained::KilledAtGrace,
            ])
            .collect();
        want.sort();
        assert_eq!(drained, want);
        assert!(took >= grace, "the stubborn one was killed early: {took:?}");
        assert!(
            took < grace + Duration::from_secs(1),
            "the producers were not given one deadline together: {took:?}"
        );
        for dir in &dirs[..2] {
            assert!(dir.path().join("done").exists(), "its work was cut off");
        }
        assert!(gone(stubborn).await);
        for host in hosts {
            host.await.unwrap();
        }
        for dir in &dirs {
            assert!(
                IngestLease::try_acquire(dir.path(), "src")
                    .unwrap()
                    .is_some(),
                "an ended producer's source stays leased"
            );
        }
    }

    /// A root that exits on TERM is not its group's end: a descendant
    /// still doing accepted work keeps the group, and the drain waits
    /// for it, through the host's own retirement too.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_drain_waits_for_work_the_root_left_running() {
        let owners = owners();
        let dir = tempfile::tempdir().unwrap();
        let mut command = sh(
            "sh -c 'trap \"\" TERM; echo $$ > worker; sleep 2; touch done' & \
             trap 'exit 0' TERM; echo $$ > pid; while :; do sleep 0.1; done",
        );
        command.current_dir(dir.path());
        let (owned, child) = launch(
            owners,
            &Arming::standard(),
            dir.path(),
            "src",
            lease(dir.path(), "src"),
            &mut command,
        )
        .await
        .unwrap();
        let host = hosted(owned, child);
        pid_in(&dir.path().join("pid")).await;
        pid_in(&dir.path().join("worker")).await;
        let drained = owners
            .drain(Duration::from_secs(4))
            .finish(tokio::time::Instant::now() + Duration::from_secs(6))
            .await;
        assert_eq!(drained.end, DrainEnd::Complete);
        let drained = drained.outcomes;
        host.await.unwrap();
        assert!(
            dir.path().join("done").exists(),
            "the descendant's work was cut off"
        );
        assert_eq!(
            drained,
            vec![(cs::producer_path(dir.path(), "src"), Drained::WithinGrace)]
        );
    }

    /// Ended at the cutoff, it is reported killed, never ended in grace.
    #[tokio::test(flavor = "multi_thread")]
    async fn work_still_running_at_the_cutoff_is_reported_killed() {
        let owners = owners();
        let dir = tempfile::tempdir().unwrap();
        let mut command = sh("sh -c 'trap \"\" TERM; echo $$ > worker; sleep 30' & \
             trap 'exit 0' TERM; echo $$ > pid; while :; do sleep 0.1; done");
        command.current_dir(dir.path());
        let (owned, child) = launch(
            owners,
            &Arming::standard(),
            dir.path(),
            "src",
            lease(dir.path(), "src"),
            &mut command,
        )
        .await
        .unwrap();
        let host = hosted(owned, child);
        pid_in(&dir.path().join("pid")).await;
        let worker = pid_in(&dir.path().join("worker")).await;
        let drained = owners
            .drain(Duration::from_millis(500))
            .finish(tokio::time::Instant::now() + Duration::from_secs(3))
            .await;
        assert_eq!(drained.end, DrainEnd::Complete);
        let drained = drained.outcomes;
        host.await.unwrap();
        assert!(gone(worker).await);
        assert_eq!(
            drained,
            vec![(cs::producer_path(dir.path(), "src"), Drained::KilledAtGrace)]
        );
    }

    /// A drain is not over while a host is still reading its producer's
    /// output, though every group has ended.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_drain_waits_for_its_readers() {
        let owners = owners();
        let reading = owners.reading(PathBuf::from("/src.producer"));
        let finish = tokio::spawn(
            owners
                .drain(Duration::ZERO)
                .finish(tokio::time::Instant::now() + Duration::from_secs(5)),
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!finish.is_finished(), "finished with a host still reading");
        drop(reading);
        tokio::time::timeout(Duration::from_secs(1), finish)
            .await
            .expect("and finishes once it is done")
            .unwrap();
    }

    /// A closed registry refuses a launch as `Closed`, before any anchor
    /// starts, and its admission refusal is told apart by type; a command
    /// that cannot spawn is still a failure.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_closed_registry_refuses_as_closed_not_failed() {
        let dir = tempfile::tempdir().unwrap();
        let mut missing = tokio::process::Command::new("/nonexistent/producer");
        let failed = launch(
            owners(),
            &Arming::standard(),
            dir.path(),
            "src",
            lease(dir.path(), "src"),
            &mut missing,
        )
        .await;
        assert!(
            matches!(failed, Err(LaunchError::Failed(_))),
            "{:?}",
            failed.err()
        );

        let closed = owners();
        tokio::task::spawn_blocking(move || closed.retire_all())
            .await
            .unwrap();
        let other = tempfile::tempdir().unwrap();
        let out = launch(
            closed,
            &Arming::standard(),
            other.path(),
            "src",
            lease(other.path(), "src"),
            &mut sh("exec sleep 300"),
        )
        .await;
        assert!(matches!(out, Err(LaunchError::Closed)), "{:?}", out.err());
        assert!(
            !cs::producer_path(other.path(), "src").exists(),
            "a group was recorded"
        );

        let spawned = Spawned::spawn(&anchor_program()).unwrap();
        let recorded = tokio::task::spawn_blocking({
            let dir = other.path().to_path_buf();
            move || {
                spawned
                    .record(closed, &dir, "src", lease(&dir, "src"))
                    .map(drop)
            }
        })
        .await
        .unwrap();
        assert!(
            refused(&recorded.unwrap_err()),
            "admission's refusal is not typed"
        );
    }

    /// A registry that closes while a launch is arming its anchor ends
    /// that launch as `Closed`, not as a failure of the source.
    #[tokio::test(flavor = "multi_thread")]
    async fn closing_while_arming_is_closed_not_failed() {
        let dir = tempfile::tempdir().unwrap();
        let owners = owners();
        let program = fake_anchor("IFS= read -r t; exec </dev/null sleep 300");
        let task = {
            let dir = dir.path().to_path_buf();
            tokio::spawn(async move {
                launch(
                    owners,
                    &Arming {
                        program,
                        ..Arming::standard()
                    },
                    &dir,
                    "src",
                    lease(&dir, "src"),
                    &mut sh("exec sleep 300"),
                )
                .await
                .err()
            })
        };
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            !task.is_finished(),
            "arming ended before the registry closed"
        );
        tokio::task::spawn_blocking(move || owners.retire_all())
            .await
            .unwrap();
        let out = tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(out, Some(LaunchError::Closed));
    }
}
