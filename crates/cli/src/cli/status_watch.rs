//! `clank status --watch`: one process watching one repo, or any number.
//!
//! Each repo is a member with its own watchers, its own Git status cache
//! and its own last line. Every wake names the member it is for, so an
//! event in one repo is work for that repo alone, and a wake from a
//! member that has since been removed, or removed and added again, is
//! dropped.
//!
//! Watching many repos, the set can change while the process runs: lines
//! on stdin add and remove repos. Stdin is read on the loop's own thread,
//! after a helper thread finds it readable, so nothing read is held
//! anywhere an exec would lose it: a replacement by a newly installed
//! build carries the set, and any line half read, in a handoff file.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Write;
use std::os::fd::OwnedFd;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use super::StatusArgs;
use super::status::{GitStatus, InputSignature, ProcessExits, StatusSnapshot, StatusWatchers};
use crate::fs_notice::{Fault, Wake};
use crate::upgrade::{Authority, Hosting, Mover, Pass};

/// Which incarnation of a member a wake is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Gen(u64);

/// What woke the loop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ring {
    Repo(Gen),
    Install,
    Stdin,
}

/// How lines are written. Chosen at start and never again, so a set
/// that shrinks to one repo, or none, keeps its line format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Wire {
    /// One repo, as `status --watch` has always printed it.
    One { json: bool },
    /// One JSON object per line, each naming its repo.
    Many,
}

/// The work done for each repo, apart from the loop that decides when.
pub(crate) trait Panel {
    type Watch: Watching;
    type PathWatch: Watching;
    type Sig: PartialEq;
    type Shown: Shown;

    /// Attach everything `repo` is watched with. Each change rings
    /// `wake`, and each Git input invalidates `git` first. Its worktree
    /// is heard from the next [`Panel::commit`].
    fn watch(&self, repo: &Path, wake: Wake<Ring>, git: &GitStatus) -> anyhow::Result<Self::Watch>;

    /// Ring `wake` when `repo`'s path changes: when it appears, through
    /// its nearest existing ancestor, or when its entries change.
    fn await_path(&mut self, repo: &Path, wake: Wake<Ring>) -> anyhow::Result<Self::PathWatch>;

    /// Bring the worktree watch in line with the repos watched now, once
    /// for however many came or went. Whether it restarted, in which case
    /// a change in a repo already watched may have gone unheard.
    fn commit(&self) -> anyhow::Result<bool>;

    /// What the snapshot is built from, cheaply.
    fn probe(&mut self, repo: &Path, git: &GitStatus) -> anyhow::Result<Self::Sig>;

    async fn build(&mut self, repo: &Path, git: &GitStatus) -> anyhow::Result<Self::Shown>;
}

pub(crate) trait Watching {
    /// Why this watch can no longer be trusted, once it can't.
    fn fault(&self) -> Option<String>;

    /// A snapshot of `repo` as it is now has been shown.
    fn shown(&mut self, _repo: &Path) {}
}

pub(crate) trait Shown {
    fn json(&self) -> serde_json::Value;
    fn human(&self, paint: bool) -> String;
}

impl Shown for StatusSnapshot {
    fn json(&self) -> serde_json::Value {
        self.to_json()
    }

    fn human(&self, paint: bool) -> String {
        StatusSnapshot::human(self, paint)
    }
}

impl Wire {
    fn line(self, repo: &Path, shown: &impl Shown, paint: bool) -> String {
        match self {
            Wire::One { json: true } => compact(&shown.json()),
            Wire::One { json: false } => shown.human(paint),
            Wire::Many => {
                let mut json = shown.json();
                if let Some(object) = json.as_object_mut() {
                    object.insert("repo".into(), repo.to_string_lossy().into());
                }
                compact(&json)
            }
        }
    }
}

fn compact(json: &serde_json::Value) -> String {
    serde_json::to_string(json).unwrap_or_default()
}

fn error_line(repo: &Path, why: &str) -> String {
    compact(&serde_json::json!({"repo": repo.to_string_lossy(), "error": why}))
}

/// Wakes landing this soon after the first are taken in the same turn.
/// Measured from the first, so a repo that never goes quiet cannot hold
/// back every other repo's turn.
const COALESCE: Duration = Duration::from_millis(200);

struct Member<P: Panel> {
    generation: Gen,
    git: GitStatus,
    state: State<P>,
    line: Option<String>,
    sig: Option<P::Sig>,
    failures: u32,
    retry_at: Option<Instant>,
    due: bool,
}

enum State<P: Panel> {
    /// Added, or due to be tried again; attached at the next turn.
    Starting,
    Watching(P::Watch),
    /// Shown as an error until its path changes or it is added again.
    Failed {
        why: String,
        path: Option<P::PathWatch>,
    },
}

/// What the installed build is started from.
pub(crate) enum Resume<'a> {
    /// The arguments this process was started with: one repo, nothing
    /// in memory that the installed build would not read again.
    AsStarted,
    Handoff(&'a Handoff),
}

/// The stdin control channel as a watch begins with it.
pub(crate) enum Stdin {
    /// The set is fixed: stdin is not read.
    Unread,
    Open {
        fd: OwnedFd,
        partial: Vec<u8>,
    },
    Ended,
}

enum Control {
    Unread,
    Open(Reader),
    Ended,
}

/// The watch loop over any number of repos.
pub(crate) struct StatusWatch<P: Panel, O: Write, E> {
    panel: P,
    wire: Wire,
    paint: bool,
    members: BTreeMap<PathBuf, Member<P>>,
    next_gen: u64,
    tx: mpsc::Sender<Ring>,
    rx: mpsc::Receiver<Ring>,
    out: O,
    wrote: bool,
    control: Control,
    mover: Mover,
    exec: E,
    _installs: crate::upgrade::Subscription,
    said_stale: bool,
}

impl<P, O, E> StatusWatch<P, O, E>
where
    P: Panel,
    O: Write,
    E: FnMut(Resume<'_>) -> std::io::Error,
{
    /// `exec` becomes the installed build and returns only if that
    /// failed.
    pub(crate) fn new(
        panel: P,
        wire: Wire,
        paint: bool,
        out: O,
        authority: Authority,
        exec: E,
        stdin: Stdin,
    ) -> anyhow::Result<Self> {
        let (tx, rx) = mpsc::channel();
        let installs = authority.subscribe({
            let wake = Wake::new(tx.clone(), Ring::Install);
            move || {
                wake.ring();
            }
        });
        let control = match stdin {
            Stdin::Unread => Control::Unread,
            Stdin::Ended => Control::Ended,
            Stdin::Open { fd, partial } => Control::Open(
                Reader::start(fd, partial, Wake::new(tx.clone(), Ring::Stdin))
                    .map_err(|e| anyhow::anyhow!("status watch: cannot read stdin: {e}"))?,
            ),
        };
        Ok(StatusWatch {
            panel,
            wire,
            paint,
            members: BTreeMap::new(),
            next_gen: 0,
            tx,
            rx,
            out,
            wrote: false,
            control,
            mover: Mover::new(authority),
            exec,
            _installs: installs,
            said_stale: false,
        })
    }

    pub(crate) async fn run(mut self) -> anyhow::Result<()> {
        loop {
            self.turn().await?;
            self.wait()?;
        }
    }

    fn fresh_gen(&mut self) -> Gen {
        self.next_gen += 1;
        Gen(self.next_gen)
    }

    fn wake(&self, generation: Gen) -> Wake<Ring> {
        Wake::new(self.tx.clone(), Ring::Repo(generation))
    }

    /// Watch the repo at `key`, a path [`repo_key`] made, from the next
    /// turn. One already watched is left as it is; one shown as an error
    /// is tried again.
    pub(crate) fn add(&mut self, key: PathBuf) {
        let member = self.members.entry(key).or_insert_with(|| Member {
            generation: Gen(0),
            git: GitStatus::default(),
            state: State::Starting,
            line: None,
            sig: None,
            failures: 0,
            retry_at: None,
            due: false,
        });
        if let State::Failed { .. } = member.state {
            member.state = State::Starting;
        }
    }

    /// Stop watching `key`. Nothing more is written for it, and what its
    /// watchers still had queued is dropped.
    pub(crate) fn remove(&mut self, key: &Path) {
        self.members.remove(key);
    }

    /// Attach a member's watchers, then mark it due: anything that
    /// changes after the watch is in place is heard, and anything before
    /// is in the snapshot it is due for.
    fn start(&mut self, key: &Path) -> anyhow::Result<()> {
        let generation = self.fresh_gen();
        let wake = self.wake(generation);
        let Some(member) = self.members.get_mut(key) else {
            return Ok(());
        };
        member.generation = generation;
        member.git = GitStatus::default();
        member.sig = None;
        member.failures = 0;
        member.retry_at = None;
        member.state = State::Starting;
        let git = member.git.clone();
        let first = match self.panel.watch(key, wake.clone(), &git) {
            Ok(watch) => {
                self.watching(key, watch);
                return Ok(());
            }
            Err(e) => e,
        };
        if self.wire != Wire::Many {
            return Err(first);
        }
        // Armed before the second attempt, so a repo that appears
        // between the two is heard by one or the other.
        let path = self.panel.await_path(key, wake.clone());
        match self.panel.watch(key, wake, &git) {
            Ok(watch) => {
                self.watching(key, watch);
                Ok(())
            }
            Err(e) => {
                let why = match &path {
                    Ok(_) => format!("{e:#}"),
                    Err(p) => unwatchable(&format!("{e:#}"), p),
                };
                if let Some(member) = self.members.get_mut(key) {
                    member.state = State::Failed {
                        why: why.clone(),
                        path: path.ok(),
                    };
                }
                self.write(key, error_line(key, &why))
            }
        }
    }

    fn watching(&mut self, key: &Path, watch: P::Watch) {
        if let Some(member) = self.members.get_mut(key) {
            member.state = State::Watching(watch);
            member.due = true;
        }
    }

    /// Show `key` as failed with `why` and wait for its path to change.
    fn fail(&mut self, key: &Path, why: String) -> anyhow::Result<()> {
        let generation = self.fresh_gen();
        let wake = self.wake(generation);
        let Some(member) = self.members.get_mut(key) else {
            return Ok(());
        };
        member.generation = generation;
        member.due = false;
        member.retry_at = None;
        let (why, path) = match self.panel.await_path(key, wake) {
            Ok(path) => (why, Some(path)),
            Err(e) => (unwatchable(&why, &e), None),
        };
        member.state = State::Failed {
            why: why.clone(),
            path,
        };
        self.write(key, error_line(key, &why))
    }

    /// One turn: end on a fault the whole process depends on, take what
    /// one member's fault says, look for an install, and refresh each
    /// member something happened to, attaching first any waiting to be.
    pub(crate) async fn turn(&mut self) -> anyhow::Result<()> {
        if let Control::Open(reader) = &self.control
            && let Some(why) = crate::fs_notice::read(&reader.fault)
        {
            anyhow::bail!("status watch: {why}");
        }
        let keys: Vec<PathBuf> = self.members.keys().cloned().collect();
        for key in &keys {
            let fault = match &self.members[key].state {
                State::Watching(watch) => watch.fault(),
                State::Failed { path, .. } => path.as_ref().and_then(Watching::fault),
                State::Starting => None,
            };
            let Some(why) = fault else {
                continue;
            };
            match (&self.members[key].state, self.wire) {
                (State::Watching(_), Wire::One { .. }) => anyhow::bail!("status watch: {why}"),
                (State::Watching(_), Wire::Many) => self.fail(key, why)?,
                (State::Failed { why: was, .. }, _) => {
                    let why = unwatchable(was, &anyhow::anyhow!(why));
                    if let Some(member) = self.members.get_mut(key) {
                        member.state = State::Failed {
                            why: why.clone(),
                            path: None,
                        };
                    }
                    self.write(key, error_line(key, &why))?;
                }
                (State::Starting, _) => {}
            }
        }
        self.pass();
        let mut starting = Vec::new();
        let mut due = Vec::new();
        for (key, member) in &mut self.members {
            let rung = std::mem::take(&mut member.due);
            match member.state {
                State::Starting => starting.push(key.clone()),
                State::Failed { .. } if rung => starting.push(key.clone()),
                State::Failed { .. } => {}
                State::Watching(_) if rung => due.push(key.clone()),
                State::Watching(_) => {}
            }
        }
        for key in &starting {
            self.start(key)?;
        }
        // Committed before any snapshot of a repo it now covers. A restart
        // leaves a gap in every other repo's watch, so each is looked at
        // again, its Git status walked afresh.
        if self.panel.commit()? {
            for (key, member) in &mut self.members {
                if matches!(member.state, State::Watching(_)) && !starting.contains(key) {
                    member.git.invalidate();
                    if !due.contains(key) {
                        due.push(key.clone());
                    }
                }
            }
        }
        for key in &starting {
            let Some(member) = self.members.get_mut(key) else {
                continue;
            };
            let State::Watching(watch) = &member.state else {
                continue;
            };
            member.due = false;
            match (watch.fault(), self.wire) {
                (None, _) => self.refresh(key).await?,
                (Some(why), Wire::One { .. }) => anyhow::bail!("status watch: {why}"),
                (Some(why), Wire::Many) => self.fail(key, why)?,
            }
        }
        for key in &due {
            if matches!(
                self.members.get(key).map(|m| &m.state),
                Some(State::Watching(_))
            ) {
                self.refresh(key).await?;
            }
        }
        Ok(())
    }

    /// Ahead of every member's gate, which an install never moves: a
    /// watch of quiet repos still finds out.
    fn pass(&mut self) {
        let Self {
            mover,
            exec,
            members,
            control,
            wire,
            ..
        } = self;
        let pass = mover.pass(Instant::now(), Hosting::Nothing, |_| match wire {
            Wire::One { .. } => exec(Resume::AsStarted),
            Wire::Many => exec(Resume::Handoff(&Handoff::new(
                members.keys().cloned().collect(),
                match control {
                    Control::Unread => HandedStdin::Unread,
                    Control::Open(reader) => HandedStdin::Open {
                        partial: reader.partial.clone(),
                    },
                    Control::Ended => HandedStdin::Ended,
                },
            ))),
        });
        if let Pass::Failed { why, retry_in } = &pass {
            tracing::debug!(%why, ?retry_in, "could not become the installed clank");
        }
        let stale = pass.stale();
        if stale && !self.said_stale {
            eprintln!("status watch: {}", crate::upgrade::STALE_BADGE);
        }
        self.said_stale = stale;
    }

    /// The nothing-changed gate (lloyd's invariant): a snapshot is built
    /// only when an input it depends on moved. A failure after the first
    /// snapshot keeps the last line and retries the same state on the
    /// next event or after [`watch_retry`]'s pause.
    async fn refresh(&mut self, key: &Path) -> anyhow::Result<()> {
        let Some(member) = self.members.get_mut(key) else {
            return Ok(());
        };
        member.retry_at = None;
        let git = member.git.clone();
        let sig = match self.panel.probe(key, &git) {
            Ok(sig) => sig,
            Err(e) => return self.refresh_failed(key, "input probe", e),
        };
        if self.members[key].sig.as_ref() == Some(&sig) {
            if let Some(member) = self.members.get_mut(key) {
                member.failures = 0;
            }
            return Ok(());
        }
        let shown = match self.panel.build(key, &git).await {
            Ok(shown) => shown,
            Err(e) => return self.refresh_failed(key, "refresh", e),
        };
        let line = self.wire.line(key, &shown, self.paint);
        self.write(key, line)?;
        if let Some(member) = self.members.get_mut(key) {
            member.failures = 0;
            member.sig = Some(sig);
            if let State::Watching(watch) = &mut member.state {
                watch.shown(key);
            }
        }
        Ok(())
    }

    fn refresh_failed(&mut self, key: &Path, stage: &str, e: anyhow::Error) -> anyhow::Result<()> {
        let Some(member) = self.members.get_mut(key) else {
            return Ok(());
        };
        member.failures += 1;
        let bootstrapped = member.sig.is_some();
        if bootstrapped {
            member.retry_at = watch_retry(member.failures).map(|pause| Instant::now() + pause);
        }
        match self.wire {
            Wire::One { .. } => watch_failure(bootstrapped, stage, format!("{e:#}")),
            Wire::Many if bootstrapped => {
                eprintln!(
                    "status watch: {}: {stage} failed ({e:#}); kept last output, retrying",
                    key.display()
                );
                Ok(())
            }
            Wire::Many => self.fail(key, format!("{stage} failed: {e:#}")),
        }
    }

    /// Write `line` for `key` unless it is what was last written for it.
    fn write(&mut self, key: &Path, line: String) -> anyhow::Result<()> {
        let Some(member) = self.members.get_mut(key) else {
            return Ok(());
        };
        if member.line.as_deref() == Some(&line) {
            return Ok(());
        }
        if matches!(self.wire, Wire::One { .. }) && self.wrote {
            writeln!(self.out)?;
        }
        writeln!(self.out, "{line}")?;
        self.out.flush()?;
        member.line = Some(line);
        self.wrote = true;
        Ok(())
    }

    /// How long the loop may sleep before a timer of its own is due: a
    /// failed refresh's retry, or a failed replacement's. With neither,
    /// `None`: idle repos cost nothing until an event.
    pub(crate) fn timer(&self) -> Option<Duration> {
        let now = Instant::now();
        self.members
            .values()
            .filter_map(|m| m.retry_at)
            .map(|at| at.saturating_duration_since(now))
            .chain(self.mover.retry_in(now))
            .min()
    }

    /// Wait for the next wake, or for [`Self::timer`], and take every
    /// wake that lands within [`COALESCE`] of the first.
    pub(crate) fn wait(&mut self) -> anyhow::Result<()> {
        let mut rings = Vec::new();
        match self.timer() {
            Some(timer) => match self.rx.recv_timeout(timer) {
                Ok(ring) => rings.push(ring),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => disconnected()?,
            },
            None => match self.rx.recv() {
                Ok(ring) => rings.push(ring),
                Err(_) => disconnected()?,
            },
        }
        if !rings.is_empty() {
            let until = Instant::now() + COALESCE;
            while let Some(left) = until.checked_duration_since(Instant::now())
                && let Ok(ring) = self.rx.recv_timeout(left)
            {
                rings.push(ring);
            }
        }
        for ring in rings {
            match ring {
                Ring::Repo(generation) => {
                    // A removed member's queued wakes, and a re-added one's
                    // old ones, name a generation no member holds.
                    if let Some(member) = self
                        .members
                        .values_mut()
                        .find(|m| m.generation == generation)
                    {
                        member.due = true;
                    }
                }
                Ring::Install => {}
                Ring::Stdin => self.read_stdin()?,
            }
        }
        let now = Instant::now();
        for member in self.members.values_mut() {
            if member.retry_at.is_some_and(|at| at <= now) {
                member.due = true;
            }
        }
        Ok(())
    }

    fn read_stdin(&mut self) -> anyhow::Result<()> {
        let Control::Open(reader) = &mut self.control else {
            return Ok(());
        };
        let (lines, ended) = reader
            .read()
            .map_err(|e| anyhow::anyhow!("status watch: reading stdin failed: {e}"))?;
        if ended {
            self.control = Control::Ended;
        }
        for line in lines {
            self.command(&line)?;
        }
        Ok(())
    }

    /// Take one repo-list line. A line that says nothing, or nothing a
    /// watch can act on, is written as an error line naming the line, and
    /// the watch goes on.
    pub(crate) fn command(&mut self, line: &[u8]) -> anyhow::Result<()> {
        let change = match parse_line(line) {
            None => return Ok(()),
            Some(Ok(change)) => change,
            Some(Err(why)) => return self.refuse(line, &why),
        };
        let (path, add) = match &change {
            Change::Add(path) => (path, true),
            Change::Remove(path) => (path, false),
        };
        let key = match repo_key(path) {
            Ok(key) => key,
            Err(e) => return self.refuse(line, &e.to_string()),
        };
        match add {
            true => self.add(key),
            false => self.remove(&key),
        }
        Ok(())
    }

    fn refuse(&mut self, line: &[u8], why: &str) -> anyhow::Result<()> {
        let said = String::from_utf8_lossy(line);
        writeln!(self.out, "{}", error_line(Path::new(said.trim()), why))?;
        self.out.flush()?;
        Ok(())
    }
}

fn disconnected() -> anyhow::Result<()> {
    anyhow::bail!("status watch: the filesystem watcher disconnected")
}

fn unwatchable(why: &str, path: &anyhow::Error) -> String {
    format!(
        "{why}; its path cannot be watched ({path:#}), so it is tried again only when added again"
    )
}

/// The watch loop's failure policy for one repo (tui-transient-refresh-
/// resilience, codex 697cdb1): BEFORE the first successful snapshot there
/// is no prior output to retain, so a probe or build failure is FATAL,
/// exactly like the one-shot path. AFTER one success, failures degrade to
/// a stderr warning with the last output kept; the signature is left
/// untouched, so the SAME state retries on the next event or after
/// [`watch_retry`]'s pause (recovery is the ordinary success path).
fn watch_failure(bootstrapped: bool, stage: &str, e: impl std::fmt::Display) -> anyhow::Result<()> {
    if bootstrapped {
        eprintln!("status watch: {stage} failed ({e}); kept last output, retrying");
        Ok(())
    } else {
        Err(anyhow::anyhow!("status watch: {stage} failed: {e}"))
    }
}

/// The pause before retrying after `failures` failed refreshes in a
/// row: 1s, 2s, 4s … capped at 30s. A timer the loop sets itself after
/// a failure, so a transient one recovers without waiting for an edit.
fn watch_retry(failures: u32) -> Option<Duration> {
    (failures > 0)
        .then(|| Duration::from_secs(1u64 << (failures - 1).min(5)).min(Duration::from_secs(30)))
}

#[derive(Debug, PartialEq, Eq)]
enum Change {
    Add(PathBuf),
    Remove(PathBuf),
}

/// One line of a repo list: `+PATH` adds, `-PATH` removes, a bare `PATH`
/// adds, and a blank line says nothing.
fn parse_line(line: &[u8]) -> Option<Result<Change, String>> {
    let line = match std::str::from_utf8(line) {
        Ok(line) => line.trim(),
        Err(_) => return Some(Err("a repo line must be UTF-8".into())),
    };
    if line.is_empty() {
        return None;
    }
    let (path, add) = match (line.strip_prefix('+'), line.strip_prefix('-')) {
        (Some(path), _) => (path.trim(), true),
        (_, Some(path)) => (path.trim(), false),
        _ => (line, true),
    };
    Some(match (path.is_empty(), add) {
        (true, _) => Err("the line names no repo".into()),
        (false, true) => Ok(Change::Add(path.into())),
        (false, false) => Ok(Change::Remove(path.into())),
    })
}

/// The one name of the repo `path` names: absolute, with the symlinks
/// of every part that exists resolved, so two spellings of one repo are
/// one member, and a repo not made yet keeps the name it will have.
pub(crate) fn repo_key(path: &Path) -> std::io::Result<PathBuf> {
    let absolute = match path.is_absolute() {
        true => path.to_path_buf(),
        false => std::env::current_dir()?.join(path),
    };
    let mut existing = absolute.as_path();
    let mut rest = Vec::new();
    let mut real = loop {
        match dunce::canonicalize(existing) {
            Ok(real) => break real,
            Err(_) => {
                rest.extend(existing.components().next_back());
                existing = existing.parent().unwrap_or(Path::new("/"));
            }
        }
    };
    for part in rest.into_iter().rev() {
        match part {
            Component::ParentDir => {
                real.pop();
            }
            Component::Normal(name) => real.push(name),
            _ => {}
        }
    }
    if real.to_str().is_none() {
        return Err(std::io::Error::other("a repo path must be UTF-8"));
    }
    Ok(real)
}

/// Stdin, read on the loop's thread: a helper thread only waits for it
/// to be readable, rings, and waits to be told it was read before it
/// waits again.
struct Reader {
    fd: Arc<OwnedFd>,
    /// The start of a line not yet ended.
    partial: Vec<u8>,
    read_it: mpsc::Sender<()>,
    fault: Fault,
}

impl Reader {
    fn start(fd: OwnedFd, partial: Vec<u8>, wake: Wake<Ring>) -> std::io::Result<Self> {
        use rustix::event::{PollFd, PollFlags, poll};
        let fd = Arc::new(fd);
        let (read_it, was_read) = mpsc::channel::<()>();
        let fault = Fault::default();
        std::thread::Builder::new()
            .name("status-stdin".into())
            .spawn({
                let (fd, fault) = (fd.clone(), fault.clone());
                move || {
                    loop {
                        let mut fds = [PollFd::new(&*fd, PollFlags::IN)];
                        match poll(&mut fds, None) {
                            Ok(_) => {}
                            Err(rustix::io::Errno::INTR) => continue,
                            Err(e) => {
                                crate::fs_notice::record(
                                    &fault,
                                    format!("waiting for stdin failed: {e}"),
                                );
                                wake.ring();
                                return;
                            }
                        }
                        if !wake.ring() || was_read.recv().is_err() {
                            return;
                        }
                    }
                }
            })?;
        Ok(Reader {
            fd,
            partial,
            read_it,
            fault,
        })
    }

    /// One read: the lines it ended, and whether stdin is at its end, in
    /// which case a last line with no newline counts as ended too.
    fn read(&mut self) -> std::io::Result<(Vec<Vec<u8>>, bool)> {
        let mut buf = [0u8; 8192];
        let n = loop {
            match rustix::io::read(&*self.fd, &mut buf) {
                Ok(n) => break n,
                Err(rustix::io::Errno::INTR) => continue,
                Err(e) => return Err(e.into()),
            }
        };
        if n == 0 {
            let last = std::mem::take(&mut self.partial);
            return Ok((
                (!last.is_empty()).then_some(last).into_iter().collect(),
                true,
            ));
        }
        self.partial.extend_from_slice(&buf[..n]);
        let mut lines = Vec::new();
        while let Some(end) = self.partial.iter().position(|b| *b == b'\n') {
            let rest = self.partial.split_off(end + 1);
            let mut line = std::mem::replace(&mut self.partial, rest);
            line.pop();
            lines.push(line);
        }
        let _ = self.read_it.send(());
        Ok((lines, false))
    }
}

const HANDOFF_VERSION: u32 = 1;

/// What a multi-repo watch hands the installed build it becomes: the set
/// as it stands, whatever argv said, and stdin as it was left. Written by
/// one build and read by another, so its bytes are a format:
/// `tests/fixtures/formats/status-watch-handoff.json`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Handoff {
    v: u32,
    pub(crate) repos: Vec<PathBuf>,
    pub(crate) stdin: HandedStdin,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum HandedStdin {
    Unread,
    /// Still read; `partial` is the start of a line read before the exec.
    Open {
        partial: Vec<u8>,
    },
    Ended,
}

impl Handoff {
    pub(crate) fn new(repos: Vec<PathBuf>, stdin: HandedStdin) -> Self {
        Handoff {
            v: HANDOFF_VERSION,
            repos,
            stdin,
        }
    }

    /// Write this where only this user can read it, run `exec` on its
    /// path, and remove it again if `exec` came back.
    pub(crate) fn hand_to(
        &self,
        dir: &Path,
        exec: impl FnOnce(&Path) -> std::io::Error,
    ) -> std::io::Error {
        match serde_json::to_vec(self) {
            Ok(bytes) => super::hook_handoff::hand_over(dir, "status-handoff", &bytes, exec),
            Err(e) => std::io::Error::other(e),
        }
    }

    /// Read the handoff a replaced build left at `path`, and remove it.
    /// A file that is not one is left where it is.
    pub(crate) fn take(path: &Path) -> anyhow::Result<Self> {
        let bytes = std::fs::read(path).map_err(|e| {
            anyhow::anyhow!("status watch: reading its handoff {}: {e}", path.display())
        })?;
        let handoff = Self::parse(&bytes)?;
        let _ = std::fs::remove_file(path);
        Ok(handoff)
    }

    pub(crate) fn parse(bytes: &[u8]) -> anyhow::Result<Self> {
        let handoff: Handoff = serde_json::from_slice(bytes)
            .map_err(|e| anyhow::anyhow!("status watch: its handoff did not parse: {e}"))?;
        match handoff.v {
            HANDOFF_VERSION => Ok(handoff),
            other => anyhow::bail!(
                "status watch: its handoff is format {other}, and this build reads {HANDOFF_VERSION}"
            ),
        }
    }
}

const HANDOFF_FLAG: &str = "--handoff";

/// The arguments the installed build is started with: the ones this
/// process was, less every flag naming a repo set, which `handoff` says
/// in full.
pub(crate) fn resumed_args(
    started_with: impl IntoIterator<Item = OsString>,
    handoff: &Path,
) -> Vec<OsString> {
    const SETS: [&str; 3] = ["--repo", "--repos-from", HANDOFF_FLAG];
    let mut out = Vec::new();
    let mut args = started_with.into_iter();
    while let Some(arg) = args.next() {
        let text = arg.to_string_lossy();
        if SETS.contains(&text.as_ref()) {
            args.next();
        } else if !SETS
            .iter()
            .any(|flag| text.starts_with(&format!("{flag}=")))
        {
            out.push(arg);
        }
    }
    out.push(HANDOFF_FLAG.into());
    out.push(handoff.into());
    out
}

/// The lines a watch starts with, as stdin would give them, and how it
/// reads stdin: from its arguments, or from the handoff it was resumed
/// with.
pub(crate) fn starting_set(args: &StatusArgs) -> anyhow::Result<(Vec<Vec<u8>>, Stdin)> {
    use std::os::unix::ffi::OsStrExt as _;
    let added = |path: &Path| [b"+", path.as_os_str().as_bytes()].concat();
    if let Some(path) = &args.handoff {
        let handoff = Handoff::take(path)?;
        let stdin = match handoff.stdin {
            HandedStdin::Unread => Stdin::Unread,
            HandedStdin::Ended => Stdin::Ended,
            HandedStdin::Open { partial } => Stdin::Open {
                fd: stdin_fd()?,
                partial,
            },
        };
        return Ok((handoff.repos.iter().map(|r| added(r)).collect(), stdin));
    }
    let mut lines: Vec<Vec<u8>> = args.repo.iter().map(|r| added(r)).collect();
    let stdin = match args.repos_from.as_deref() {
        Some(from) if from == Path::new("-") => Stdin::Open {
            fd: stdin_fd()?,
            partial: Vec::new(),
        },
        Some(from) => {
            let text = std::fs::read(from)
                .map_err(|e| anyhow::anyhow!("status watch: reading `{}`: {e}", from.display()))?;
            lines.extend(text.split(|b| *b == b'\n').map(<[u8]>::to_vec));
            Stdin::Unread
        }
        None => Stdin::Unread,
    };
    Ok((lines, stdin))
}

fn stdin_fd() -> anyhow::Result<OwnedFd> {
    rustix::io::fcntl_dupfd_cloexec(std::io::stdin(), 3)
        .map_err(|e| anyhow::anyhow!("status watch: cannot read stdin: {e}"))
}

/// Whether these arguments watch a set of repos rather than one.
pub(crate) fn watches_many(args: &StatusArgs) -> bool {
    args.handoff.is_some() || args.repos_from.is_some() || args.repo.len() > 1
}

/// The panel `clank status --watch` shows.
pub(crate) struct StatusPanel {
    pub(crate) watches: super::status::Watches,
    pub(crate) home: Option<PathBuf>,
    pub(crate) policy: crate::rebuild::CachePolicy,
    pub(crate) plan: Option<String>,
}

pub(crate) struct RepoWatch {
    watchers: StatusWatchers,
    exits: ProcessExits,
}

impl Watching for RepoWatch {
    fn fault(&self) -> Option<String> {
        self.watchers
            .git
            .fault()
            .or_else(|| self.watchers.core.fault())
            .or_else(|| self.exits.fault())
    }

    fn shown(&mut self, repo: &Path) {
        self.exits.track(repo);
    }
}

/// A kernel watch on a repo's path while it cannot be watched itself.
pub(crate) struct PathWatch {
    _watcher: crate::proc_identity::tree_watch::KernelWatcher,
    fault: Fault,
}

impl Watching for PathWatch {
    fn fault(&self) -> Option<String> {
        crate::fs_notice::read(&self.fault)
    }
}

pub(crate) fn watch_path(repo: &Path, wake: Wake<Ring>) -> anyhow::Result<PathWatch> {
    use crate::fs_notice::{Notice, notice};
    use notify::Watcher as _;
    let fault = Fault::default();
    let mut watcher = crate::proc_identity::tree_watch::KernelWatcher::new(|_| false, {
        let fault = fault.clone();
        move |res: notify::Result<notify::Event>| {
            match notice(&res) {
                Notice::Read => return,
                Notice::Failed(why) => {
                    crate::fs_notice::record(&fault, format!("the watch on its path failed: {why}"))
                }
                Notice::Rescan | Notice::Changed(_) => {}
            }
            wake.ring();
        }
    })?;
    watcher.watch(repo, notify::RecursiveMode::NonRecursive)?;
    Ok(PathWatch {
        _watcher: watcher,
        fault,
    })
}

impl Panel for StatusPanel {
    type Watch = RepoWatch;
    type PathWatch = PathWatch;
    type Sig = InputSignature;
    type Shown = StatusSnapshot;

    fn watch(&self, repo: &Path, wake: Wake<Ring>, git: &GitStatus) -> anyhow::Result<RepoWatch> {
        // Before any watcher: one of the repo's own makes `.clank/`, and
        // a path that is not a repo must be left as it was.
        crate::git_io::open(repo)?;
        let watchers = super::status::watch_status_paths(wake.clone(), repo, git, &self.watches)?;
        let exits = ProcessExits::new(move || {
            wake.ring();
        });
        Ok(RepoWatch { watchers, exits })
    }

    fn await_path(&mut self, repo: &Path, wake: Wake<Ring>) -> anyhow::Result<PathWatch> {
        watch_path(repo, wake)
    }

    fn commit(&self) -> anyhow::Result<bool> {
        self.watches.commit()
    }

    fn probe(&mut self, repo: &Path, git: &GitStatus) -> anyhow::Result<InputSignature> {
        super::status::input_signature(repo, git)
    }

    async fn build(&mut self, repo: &Path, git: &GitStatus) -> anyhow::Result<StatusSnapshot> {
        let basename = super::repo_basename(repo)?;
        StatusSnapshot::build_with(
            repo,
            &basename,
            self.home.as_deref(),
            self.policy,
            self.plan.as_deref(),
            true,
            git,
        )
        .await
    }
}

/// Raise this process's soft limit on open files as far as it will go.
/// Each repo holds about a dozen descriptors, its kernel watches among
/// them, and the soft limit a macOS process starts with (256) runs out at
/// about twenty repos.
fn raise_open_files() {
    use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
    let limit = getrlimit(Resource::Nofile);
    for want in open_file_targets(limit.current, limit.maximum) {
        let raised = Rlimit {
            current: Some(want),
            maximum: limit.maximum,
        };
        if setrlimit(Resource::Nofile, raised).is_ok() {
            return;
        }
    }
}

/// The soft limits to try, best first: the hard limit, then `OPEN_MAX`,
/// which macOS accepts when it refuses a hard limit it calls unlimited.
fn open_file_targets(current: Option<u64>, maximum: Option<u64>) -> Vec<u64> {
    const OPEN_MAX: u64 = 10240;
    let Some(current) = current else {
        return Vec::new();
    };
    [maximum, Some(OPEN_MAX)]
        .into_iter()
        .flatten()
        .filter(|want| *want > current && maximum.is_none_or(|max| *want <= max))
        .collect()
}

pub(crate) async fn run(args: StatusArgs, panel: StatusPanel) -> anyhow::Result<()> {
    let many = watches_many(&args);
    if many && !args.json {
        anyhow::bail!("watching several repos needs `-j`: each line is one repo's JSON");
    }
    let exec = |resume: Resume<'_>| match resume {
        Resume::AsStarted => crate::upgrade::exec_installed_as_started(),
        Resume::Handoff(handoff) => {
            if let Err(why) = super::monitor::ensure_runtime_dir() {
                return std::io::Error::other(why);
            }
            let dir = super::monitor::runtime_dir(rustix::process::getuid().as_raw());
            handoff.hand_to(&dir, |path| {
                crate::upgrade::exec_installed(resumed_args(std::env::args_os().skip(1), path))
            })
        }
    };
    let authority = crate::upgrade::process().clone();
    if !many {
        let repo = super::resolve_repo(args.repo.first().map(PathBuf::as_path))?;
        super::repo_basename(&repo)?;
        let mut watch = StatusWatch::new(
            panel,
            Wire::One { json: args.json },
            super::status::paint_stdout(),
            std::io::stdout(),
            authority,
            exec,
            Stdin::Unread,
        )?;
        watch.add(repo);
        return watch.run().await;
    }
    raise_open_files();
    let (lines, stdin) = starting_set(&args)?;
    let mut watch = StatusWatch::new(
        panel,
        Wire::Many,
        false,
        std::io::stdout(),
        authority,
        exec,
        stdin,
    )?;
    for line in lines {
        watch.command(&line)?;
    }
    watch.run().await
}

#[cfg(test)]
#[path = "status_watch_tests.rs"]
mod tests;
