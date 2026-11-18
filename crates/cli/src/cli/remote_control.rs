//! `clank remote`: a repo's remote, controlled through the TUI that owns
//! it by files in the per-repo runtime namespace. No socket, port,
//! listener or admin endpoint: the owner already watches that directory.
//!
//! The owner is whoever holds the TUI's status lease. Its holder record
//! names it (pid and birth) and says whether it speaks this protocol; its
//! status file says whether control is ready. A request names the owner
//! it is for, and is answered in a result file of its own.

use crate::proc_identity::ProcToken;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The protocol a current owner's lease holder record advertises.
pub(crate) const PROTOCOL: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Owner {
    pub(crate) pid: i32,
    pub(crate) birth: ProcToken,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Control {
    Starting,
    Ready,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Shown {
    Off,
    Starting,
    On,
    Stopping,
    Failed,
    Unconfigured,
}

/// What the owner publishes: a fixed projection, never configuration,
/// credentials or login links. A login link is only ever in the result
/// file of the request that asked for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Status {
    pub(crate) protocol: u32,
    pub(crate) owner: Owner,
    pub(crate) control: Control,
    pub(crate) control_reason: Option<String>,
    pub(crate) shown: Shown,
    pub(crate) url: Option<String>,
    pub(crate) error: Option<String>,
    pub(crate) updated: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Want {
    On,
    Off,
}

/// What a request is for. A link is not a state to switch to, so it
/// never reaches [`decide`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "ask", rename_all = "snake_case")]
pub(crate) enum Ask {
    Switch { want: Want },
    Link,
}

impl From<Want> for Ask {
    fn from(want: Want) -> Self {
        Ask::Switch { want }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Request {
    pub(crate) id: String,
    #[serde(flatten)]
    pub(crate) ask: Ask,
    pub(crate) owner: Owner,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub(crate) enum Outcome {
    Applied {
        shown: Shown,
    },
    Busy {
        shown: Shown,
    },
    Failed {
        reason: String,
    },
    StaleOwner,
    /// A login link: whoever reads it can log in.
    Link {
        link: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Response {
    pub(crate) id: String,
    #[serde(flatten)]
    pub(crate) outcome: Outcome,
}

fn named(repo: &Path, name: &str) -> PathBuf {
    crate::cli::monitor::runtime_path(repo, name)
}

pub(crate) fn status_path(repo: &Path) -> PathBuf {
    named(repo, "remote-status.json")
}

pub(crate) fn request_path(repo: &Path, id: &str) -> PathBuf {
    named(repo, &format!("remote-req-{id}.json"))
}

pub(crate) fn result_path(repo: &Path, id: &str) -> PathBuf {
    named(repo, &format!("remote-res-{id}.json"))
}

/// The file-name prefix of this repo's requests in the runtime dir.
pub(crate) fn request_prefix(repo: &Path) -> String {
    file_name(&named(repo, "remote-req-"))
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The request id a runtime-dir entry names, if it is one of `repo`'s.
pub(crate) fn request_id(repo: &Path, entry: &Path) -> Option<String> {
    let name = file_name(entry);
    let id = name
        .strip_prefix(&request_prefix(repo))?
        .strip_suffix(".json")?;
    (!id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
        .then(|| id.to_string())
}

/// Write `value` whole or not at all: a temp file beside it, then rename.
pub(crate) fn write_atomic<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    use std::io::Write;
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("no parent directory"))?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(&serde_json::to_vec(value)?)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

pub(crate) fn read<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// A base address only: no credentials, query or fragment.
pub(crate) fn safe_url(raw: &str) -> Option<String> {
    let mut url = url::Url::parse(raw).ok()?;
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_query(None);
    url.set_fragment(None);
    Some(url.to_string())
}

/// A reason fit to publish: `secrets` masked, every URL cut to its
/// scheme, host and port, then the shapes `sanitize` knows masked. A
/// URL's userinfo, path, query and fragment are where login links and
/// connector capabilities live, and no shape heuristic sees a short
/// token there.
pub(crate) fn safe_reason(text: &str, secrets: &[String]) -> String {
    let mut text = text.to_string();
    for secret in secrets.iter().filter(|s| !s.is_empty()) {
        text = text.replace(secret.as_str(), "[redacted]");
    }
    crate::cli::source_diagnostic::sanitize(&addresses_only(&text), secrets)
}

fn addresses_only(text: &str) -> String {
    let scheme_char = |c: char| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.');
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("://") {
        let start = rest[..at]
            .char_indices()
            .rev()
            .take_while(|(_, c)| scheme_char(*c))
            .last()
            .map_or(at, |(i, _)| i);
        let end = rest[at..]
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | '`'))
            .map_or(rest.len(), |e| at + e);
        out.push_str(&rest[..start]);
        let raw = rest[start..end].trim_end_matches(['.', ',', ';', ':', ')', ']']);
        out.push_str(&match url::Url::parse(raw) {
            Ok(url) if start < at && url.host_str().is_some() => {
                let mut base = format!("{}://{}", url.scheme(), url.host_str().unwrap_or(""));
                if let Some(port) = url.port() {
                    base.push_str(&format!(":{port}"));
                }
                base
            }
            _ => "[address]".to_string(),
        });
        out.push_str(&rest[start + raw.len()..end]);
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// Whether the owner acts on a request it has just seen, whatever it asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Admit {
    /// Already answered, or already being worked on.
    Seen,
    /// Written for an owner this process is not.
    Stale,
    Act,
}

/// `seen`: this id has a result, or is the one being worked on.
pub(crate) fn admit(request: &Request, me: &Owner, seen: bool) -> Admit {
    if seen {
        Admit::Seen
    } else if request.owner != *me {
        Admit::Stale
    } else {
        Admit::Act
    }
}

/// What the owner does with an admitted switch request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Decision {
    Answer(Outcome),
    Start,
    Stop,
}

/// `busy`: another request's transition is in flight.
pub(crate) fn decide(want: Want, shown: Shown, busy: bool) -> Decision {
    if busy {
        return Decision::Answer(Outcome::Busy { shown });
    }
    match (want, shown) {
        (_, Shown::Starting | Shown::Stopping) => Decision::Answer(Outcome::Busy { shown }),
        (Want::On, Shown::On) | (Want::Off, Shown::Off | Shown::Unconfigured) => {
            Decision::Answer(Outcome::Applied { shown })
        }
        (Want::On, Shown::Unconfigured) => Decision::Answer(Outcome::Failed {
            reason: "no remote is configured for this repo".into(),
        }),
        (Want::On, Shown::Off | Shown::Failed) => Decision::Start,
        (Want::Off, Shown::On | Shown::Failed) => Decision::Stop,
    }
}

/// Why there is no link to give while the remote is `shown`, quoting
/// `failure` when it failed; `None` when on.
pub(crate) fn unlinkable(shown: Shown, failure: Option<&str>) -> Option<String> {
    Some(match shown {
        Shown::On => return None,
        Shown::Off => "the remote is off; run `clank remote on` first".into(),
        Shown::Starting => "the remote is starting; try again once it is on".into(),
        Shown::Stopping => "the remote is stopping".into(),
        Shown::Failed => format!(
            "the remote failed: {}",
            failure.unwrap_or("no reason given")
        ),
        Shown::Unconfigured => "no remote is configured for this repo".into(),
    })
}

/// Whether the transition a request started has finished, and how.
pub(crate) fn completion(want: Want, shown: Shown, error: Option<&str>) -> Option<Outcome> {
    match (want, shown) {
        (_, Shown::Starting | Shown::Stopping) => None,
        (Want::On, Shown::On) | (Want::Off, Shown::Off) => Some(Outcome::Applied { shown }),
        (_, Shown::Failed) => Some(Outcome::Failed {
            reason: error.unwrap_or("the remote failed").to_string(),
        }),
        (_, shown) => Some(Outcome::Failed {
            reason: format!("the remote ended {shown:?}, not as asked"),
        }),
    }
}

/// The lease holder as the CLI read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Holder {
    pub(crate) pid: i32,
    pub(crate) birth: Option<ProcToken>,
    pub(crate) remote_protocol: Option<u32>,
}

/// What the CLI can conclude before writing a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Readiness {
    /// The lease is free: no TUI owns this repo.
    NoOwner,
    /// The lease is held but its holder cannot be verified.
    Unknown(String),
    /// A verified holder without the protocol: an older build.
    Older {
        pid: i32,
    },
    /// A verified holder, or its status, speaking another protocol.
    Unsupported {
        pid: i32,
        protocol: u32,
    },
    /// A current owner whose control cannot work, and why.
    Unavailable {
        owner: Owner,
        reason: String,
    },
    /// A current owner whose ready status has not appeared yet.
    Wait(Owner),
    Go(Owner),
}

impl Readiness {
    /// The current owner this was concluded about, when it is one.
    pub(crate) fn owner(&self) -> Option<&Owner> {
        match self {
            Readiness::Unavailable { owner, .. }
            | Readiness::Wait(owner)
            | Readiness::Go(owner) => Some(owner),
            _ => None,
        }
    }
}

/// `lease_free` is the CLI's own non-blocking flock attempt; `verified`
/// checks a holder's pid against its birth natively.
pub(crate) fn readiness(
    lease_free: bool,
    holder: Option<&Holder>,
    verified: impl Fn(i32, &ProcToken) -> bool,
    status: Option<&Status>,
) -> Readiness {
    if lease_free {
        return Readiness::NoOwner;
    }
    let Some(holder) = holder else {
        return Readiness::Unknown("its holder record is unreadable".into());
    };
    let Some(birth) = holder.birth.clone().filter(|b| verified(holder.pid, b)) else {
        return Readiness::Unknown(format!("pid {} could not be verified", holder.pid));
    };
    match holder.remote_protocol {
        None => return Readiness::Older { pid: holder.pid },
        Some(protocol) if protocol != PROTOCOL => {
            return Readiness::Unsupported {
                pid: holder.pid,
                protocol,
            };
        }
        Some(_) => {}
    }
    let me = Owner {
        pid: holder.pid,
        birth,
    };
    match status.filter(|s| s.owner == me) {
        None => Readiness::Wait(me),
        Some(s) if s.protocol != PROTOCOL => Readiness::Unsupported {
            pid: me.pid,
            protocol: s.protocol,
        },
        Some(s) => match s.control {
            Control::Ready => Readiness::Go(me),
            Control::Starting => Readiness::Wait(me),
            Control::Unavailable => Readiness::Unavailable {
                owner: me,
                reason: s
                    .control_reason
                    .clone()
                    .unwrap_or_else(|| "remote control is unavailable".into()),
            },
        },
    }
}

/// What `clank remote on|off` concluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Answer {
    Done(Outcome),
    NoOwner,
    Unknown(String),
    Older {
        pid: i32,
    },
    Unsupported {
        pid: i32,
        protocol: u32,
    },
    Unavailable {
        pid: i32,
        reason: String,
    },
    /// The owner may or may not have acted; it did not say in time.
    Unconfirmed(String),
}

impl std::fmt::Display for Answer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Answer::Done(Outcome::Applied { shown }) => write!(f, "done: the remote is {shown:?}"),
            Answer::Done(Outcome::Busy { shown }) => {
                write!(
                    f,
                    "busy: the remote is {shown:?}; try again when it settles"
                )
            }
            Answer::Done(Outcome::Failed { reason }) => write!(f, "failed: {reason}"),
            Answer::Done(Outcome::Link { .. }) => write!(f, "answered with a login link"),
            Answer::Done(Outcome::StaleOwner) => {
                write!(
                    f,
                    "refused: the request named an owner that has since been replaced"
                )
            }
            Answer::NoOwner => write!(f, "no `clank tui` owns this repo's remote"),
            Answer::Unknown(why) => {
                write!(f, "this repo's `clank tui` cannot be identified: {why}")
            }
            Answer::Older { pid } => write!(
                f,
                "the owner (pid {pid}) runs an older clank without remote control; \
                 it will have it after its next restart"
            ),
            Answer::Unsupported { pid, protocol } => write!(
                f,
                "the owner (pid {pid}) speaks remote-control protocol {protocol}, \
                 this clank speaks {PROTOCOL}; restart that `clank tui` with this build"
            ),
            Answer::Unavailable { pid, reason } => {
                write!(
                    f,
                    "the owner (pid {pid}) cannot take remote control: {reason}"
                )
            }
            Answer::Unconfirmed(why) => write!(f, "unconfirmed: {why}"),
        }
    }
}

fn current(repo: &Path) -> (Readiness, Option<Status>) {
    let status = read::<Status>(&status_path(repo));
    let (free, holder) = crate::cli::status_tui::lease::probe(repo);
    let readiness = readiness(
        free,
        holder.as_ref(),
        |pid, birth| birth.still_holds(pid),
        status.as_ref(),
    );
    (readiness, status)
}

/// How `ask` hears the runtime directory and the owner's exit.
pub(crate) trait Observe {
    /// Report `owner`'s exit from now on.
    fn arm(&mut self, owner: &Owner) -> crate::proc_identity::exit_watch::Armed;
    /// The owners seen exiting since the last call (empty when only the
    /// directory changed), or `None` once `until` passes with nothing.
    fn next(&mut self, until: std::time::Instant) -> Option<Vec<Owner>>;
}

/// The production [`Observe`]: one exit watch on the runtime directory.
pub(crate) struct Watched {
    watch: crate::proc_identity::exit_watch::ExitWatch<Owner>,
    rx: std::sync::mpsc::Receiver<crate::proc_identity::exit_watch::Wake<Owner>>,
}

impl Watched {
    pub(crate) fn new(repo: &Path) -> std::io::Result<Self> {
        let (tx, rx) = std::sync::mpsc::channel();
        let dir = status_path(repo).parent().map(Path::to_path_buf);
        let watch = crate::proc_identity::exit_watch::ExitWatch::new(dir.as_deref(), move |w| {
            let _ = tx.send(w);
        })?;
        Ok(Self { watch, rx })
    }
}

/// Wakes taken in one batch: a busy directory is read once per batch,
/// not once per event.
const BATCH: usize = 1024;

impl Observe for Watched {
    fn arm(&mut self, owner: &Owner) -> crate::proc_identity::exit_watch::Armed {
        self.watch.arm(owner.clone(), owner.pid, &owner.birth)
    }
    fn next(&mut self, until: std::time::Instant) -> Option<Vec<Owner>> {
        use crate::proc_identity::exit_watch::Wake;
        let left = until.saturating_duration_since(std::time::Instant::now());
        let first = self.rx.recv_timeout(left).ok()?;
        Some(
            std::iter::once(first)
                .chain(self.rx.try_iter().take(BATCH))
                .filter_map(|w| match w {
                    Wake::Exited(owner) => Some(owner),
                    Wake::Dir => None,
                    Wake::Failed(why) => {
                        eprintln!(
                            "clank: the watch on the owning TUI failed: {why}; its answer is \
                             not followed"
                        );
                        None
                    }
                })
                .collect(),
        )
    }
}

/// Ask the owning TUI, and wait up to `timeout` for its answer.
pub(crate) fn ask(repo: &Path, ask: impl Into<Ask>, timeout: std::time::Duration) -> Answer {
    let ask = ask.into();
    let deadline = std::time::Instant::now() + timeout;
    if let r @ (Readiness::NoOwner
    | Readiness::Unknown(_)
    | Readiness::Older { .. }
    | Readiness::Unsupported { .. }
    | Readiness::Unavailable { .. }) = current(repo).0
    {
        return settled(r);
    }
    match Watched::new(repo) {
        Ok(mut watched) => ask_observed(repo, ask, deadline, &mut watched),
        Err(e) => Answer::Unconfirmed(format!("cannot watch for the owner's answer: {e}")),
    }
}

/// `ask`, through `observe`. Everything it concludes is about the one
/// owner its exit watch is armed on: a replacement seen before the
/// request is written is armed and re-read first, and any other
/// owner's exit is not this request's. The result file is removed once
/// read: it may hold a login link.
pub(crate) fn ask_observed(
    repo: &Path,
    ask: Ask,
    deadline: std::time::Instant,
    observe: &mut impl Observe,
) -> Answer {
    use crate::proc_identity::exit_watch::Armed;
    let expired = || std::time::Instant::now() >= deadline;
    let mut armed: Option<Owner> = None;
    let owner = loop {
        if expired() {
            return Answer::Unconfirmed(match &armed {
                Some(o) => format!("the owner (pid {}) did not become ready in time", o.pid),
                None => "no owner became ready in time".into(),
            });
        }
        let readiness = current(repo).0;
        let Some(owner) = readiness
            .owner()
            .filter(|_| matches!(readiness, Readiness::Wait(_) | Readiness::Go(_)))
        else {
            return settled(readiness);
        };
        if armed.as_ref() != Some(owner) {
            match observe.arm(owner) {
                Armed::Watching => armed = Some(owner.clone()),
                Armed::Ended => {
                    return Answer::Unconfirmed(format!(
                        "the owner (pid {}) exited before it was ready",
                        owner.pid
                    ));
                }
                Armed::Unknown => {
                    return Answer::Unconfirmed(format!(
                        "the owner (pid {}) cannot be watched for its exit",
                        owner.pid
                    ));
                }
            }
            // Armed after it was read: read again, so nothing between
            // the read and the arming goes unseen.
            continue;
        }
        if let Readiness::Go(owner) = readiness {
            break owner;
        }
        match observe.next(deadline) {
            Some(exited) if exited.contains(owner) => {
                return Answer::Unconfirmed(format!(
                    "the owner (pid {}) exited before it was ready",
                    owner.pid
                ));
            }
            Some(_) | None => {}
        }
    };
    let id = format!(
        "{:x}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
        std::process::id()
    );
    let request = Request {
        id: id.clone(),
        ask,
        owner: owner.clone(),
    };
    let request_file = request_path(repo, &id);
    if let Err(e) = write_atomic(&request_file, &request) {
        return Answer::Unconfirmed(format!("cannot write the request: {e}"));
    }
    let answer = loop {
        if expired() {
            break Answer::Unconfirmed(format!(
                "the owner (pid {}) did not answer in time",
                owner.pid
            ));
        }
        if let Some(response) = read::<Response>(&result_path(repo, &id)) {
            break Answer::Done(response.outcome);
        }
        match observe.next(deadline) {
            Some(exited) if exited.contains(&owner) => {
                // Its answer may have landed just before it went.
                break match read::<Response>(&result_path(repo, &id)) {
                    Some(response) => Answer::Done(response.outcome),
                    None => Answer::Unconfirmed(format!(
                        "the owner (pid {}) exited before answering",
                        owner.pid
                    )),
                };
            }
            Some(_) | None => {}
        }
    };
    let _ = std::fs::remove_file(&request_file);
    let _ = std::fs::remove_file(result_path(repo, &id));
    answer
}

fn settled(r: Readiness) -> Answer {
    match r {
        Readiness::NoOwner => Answer::NoOwner,
        Readiness::Unknown(why) => Answer::Unknown(why),
        Readiness::Older { pid } => Answer::Older { pid },
        Readiness::Unsupported { pid, protocol } => Answer::Unsupported { pid, protocol },
        Readiness::Unavailable { owner, reason } => Answer::Unavailable {
            pid: owner.pid,
            reason,
        },
        Readiness::Wait(owner) => {
            Answer::Unconfirmed(format!("the owner (pid {}) is not ready", owner.pid))
        }
        Readiness::Go(_) => Answer::Unconfirmed("nothing was asked".into()),
    }
}

/// `clank remote status`: who owns the remote, and what that owner
/// publishes. A status file some other process left is not shown.
pub(crate) fn status_text(repo: &Path) -> String {
    let (readiness, status) = current(repo);
    let mut out = match &readiness {
        Readiness::NoOwner => "owner: none".to_string(),
        Readiness::Unknown(why) => format!("owner: unknown ({why})"),
        Readiness::Older { pid } => {
            format!("owner: pid {pid}, an older clank without remote control")
        }
        Readiness::Unsupported { pid, protocol } => {
            format!("owner: pid {pid}, speaking remote-control protocol {protocol}, not {PROTOCOL}")
        }
        Readiness::Unavailable { owner, reason } => {
            format!("owner: pid {}; control unavailable: {reason}", owner.pid)
        }
        Readiness::Wait(owner) => format!("owner: pid {}; control not ready yet", owner.pid),
        Readiness::Go(owner) => format!("owner: pid {}; control ready", owner.pid),
    };
    if let Some(s) = status.filter(|s| Some(&s.owner) == readiness.owner()) {
        out.push_str(&format!("\nremote: {:?}", s.shown));
        if let Some(url) = s.url {
            out.push_str(&format!("\nurl: {url}"));
        }
        if let Some(error) = s.error {
            out.push_str(&format!("\nerror: {error}"));
        }
    }
    out
}

/// The `clank remote` command.
pub async fn run(args: crate::cli::RemoteArgs) -> anyhow::Result<()> {
    let repo = crate::cli::resolve_repo(args.repo.as_deref())?;
    let timeout = std::time::Duration::from_secs(args.timeout);
    let asked = match args.command {
        crate::cli::RemoteCmd::Status => {
            println!("{}", status_text(&repo));
            return Ok(());
        }
        crate::cli::RemoteCmd::On => Want::On.into(),
        crate::cli::RemoteCmd::Off => Want::Off.into(),
        crate::cli::RemoteCmd::Link => Ask::Link,
    };
    let answer = tokio::task::spawn_blocking(move || ask(&repo, asked, timeout)).await?;
    match (asked, answer) {
        (Ask::Switch { .. }, answer @ Answer::Done(Outcome::Applied { .. })) => {
            println!("{answer}");
            Ok(())
        }
        (Ask::Link, Answer::Done(Outcome::Link { link })) => {
            println!("{link}");
            Ok(())
        }
        (_, other) => anyhow::bail!("{other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn me() -> Owner {
        Owner {
            pid: 40,
            birth: ProcToken::MacStart { sec: 1, usec: 2 },
        }
    }

    fn ask(id: &str, ask: Ask, owner: Owner) -> Request {
        Request {
            id: id.into(),
            ask,
            owner,
        }
    }

    #[test]
    fn asking_for_the_state_it_is_in_is_applied_without_a_transition() {
        assert_eq!(
            decide(Want::On, Shown::On, false),
            Decision::Answer(Outcome::Applied { shown: Shown::On })
        );
        assert_eq!(
            decide(Want::Off, Shown::Off, false),
            Decision::Answer(Outcome::Applied { shown: Shown::Off })
        );
        assert_eq!(decide(Want::On, Shown::Off, false), Decision::Start);
        assert_eq!(decide(Want::Off, Shown::On, false), Decision::Stop);
    }

    #[test]
    fn a_transition_in_progress_or_another_request_is_busy() {
        for shown in [Shown::Starting, Shown::Stopping] {
            assert_eq!(
                decide(Want::On, shown, false),
                Decision::Answer(Outcome::Busy { shown })
            );
        }
        assert_eq!(
            decide(Want::On, Shown::Off, true),
            Decision::Answer(Outcome::Busy { shown: Shown::Off })
        );
    }

    #[test]
    fn a_request_already_seen_is_not_acted_on_whatever_it_asks() {
        for asked in [Want::On.into(), Ask::Link] {
            assert_eq!(admit(&ask("a", asked, me()), &me(), true), Admit::Seen);
            assert_eq!(admit(&ask("a", asked, me()), &me(), false), Admit::Act);
        }
    }

    #[test]
    fn a_request_for_another_owner_is_stale_even_on_a_reused_pid() {
        let previous = Owner {
            pid: 40,
            birth: ProcToken::MacStart { sec: 0, usec: 9 },
        };
        for asked in [Want::On.into(), Ask::Link] {
            assert_eq!(
                admit(&ask("a", asked, previous.clone()), &me(), false),
                Admit::Stale
            );
        }
    }

    #[test]
    fn only_an_on_remote_has_a_link_and_off_says_how_to_get_one() {
        assert_eq!(unlinkable(Shown::On, None), None);
        let off = unlinkable(Shown::Off, None).unwrap();
        assert!(off.contains("run `clank remote on` first"), "{off}");
        for shown in [
            Shown::Starting,
            Shown::Stopping,
            Shown::Failed,
            Shown::Unconfigured,
        ] {
            let why = unlinkable(shown, Some("tunnel refused")).unwrap();
            assert!(!why.contains("remote on"), "{shown:?}: {why}");
        }
        assert!(
            unlinkable(Shown::Failed, Some("tunnel refused"))
                .unwrap()
                .contains("tunnel refused")
        );
    }

    #[test]
    fn a_link_request_is_its_own_kind_on_the_wire() {
        let link = serde_json::to_value(ask("a", Ask::Link, me())).unwrap();
        assert_eq!(link["ask"], "link");
        assert!(link.get("want").is_none(), "{link}");
        let on = serde_json::to_value(ask("a", Want::On.into(), me())).unwrap();
        assert_eq!((&on["ask"], &on["want"]), (&"switch".into(), &"on".into()));
        let back: Request = serde_json::from_value(link).unwrap();
        assert_eq!(back.ask, Ask::Link);
    }

    #[test]
    fn completion_waits_for_the_asked_state_and_reports_failure() {
        assert_eq!(completion(Want::On, Shown::Starting, None), None);
        assert_eq!(
            completion(Want::On, Shown::On, None),
            Some(Outcome::Applied { shown: Shown::On })
        );
        assert_eq!(
            completion(Want::On, Shown::Failed, Some("tunnel refused")),
            Some(Outcome::Failed {
                reason: "tunnel refused".into()
            })
        );
    }

    fn status(control: Control) -> Status {
        Status {
            protocol: PROTOCOL,
            owner: me(),
            control,
            control_reason: Some("no native watch".into()),
            shown: Shown::Off,
            url: None,
            error: None,
            updated: "now".into(),
        }
    }

    fn holder(protocol: Option<u32>) -> Holder {
        Holder {
            pid: 40,
            birth: Some(me().birth),
            remote_protocol: protocol,
        }
    }

    #[test]
    fn readiness_is_concluded_only_from_evidence() {
        let yes = |_: i32, _: &ProcToken| true;
        let no = |_: i32, _: &ProcToken| false;
        assert_eq!(readiness(true, None, yes, None), Readiness::NoOwner);
        assert!(matches!(
            readiness(false, None, yes, None),
            Readiness::Unknown(_)
        ));
        assert!(matches!(
            readiness(false, Some(&holder(Some(PROTOCOL))), no, None),
            Readiness::Unknown(_)
        ));
        assert_eq!(
            readiness(false, Some(&holder(None)), yes, None),
            Readiness::Older { pid: 40 }
        );
        assert_eq!(
            readiness(false, Some(&holder(Some(PROTOCOL))), yes, None),
            Readiness::Wait(me()),
            "a current holder with no status yet is waited for, never called older"
        );
        assert_eq!(
            readiness(
                false,
                Some(&holder(Some(PROTOCOL))),
                yes,
                Some(&status(Control::Starting))
            ),
            Readiness::Wait(me())
        );
        assert_eq!(
            readiness(
                false,
                Some(&holder(Some(PROTOCOL))),
                yes,
                Some(&status(Control::Ready))
            ),
            Readiness::Go(me())
        );
        assert!(matches!(
            readiness(false, Some(&holder(Some(PROTOCOL))), yes, Some(&status(Control::Unavailable))),
            Readiness::Unavailable { reason, .. } if reason == "no native watch"
        ));
        assert_eq!(
            readiness(false, Some(&holder(Some(PROTOCOL + 1))), yes, None),
            Readiness::Unsupported {
                pid: 40,
                protocol: PROTOCOL + 1
            }
        );
        let mut newer = status(Control::Ready);
        newer.protocol = PROTOCOL + 1;
        assert_eq!(
            readiness(false, Some(&holder(Some(PROTOCOL))), yes, Some(&newer)),
            Readiness::Unsupported {
                pid: 40,
                protocol: PROTOCOL + 1
            }
        );
        let mut other = status(Control::Ready);
        other.owner.birth = ProcToken::MacStart { sec: 9, usec: 9 };
        assert_eq!(
            readiness(false, Some(&holder(Some(PROTOCOL))), yes, Some(&other)),
            Readiness::Wait(me()),
            "a previous owner's status is not this owner's"
        );
    }

    #[test]
    fn the_projection_never_carries_a_credential() {
        assert_eq!(
            safe_url("https://u:p@host.example/login?t=SECRETTOKEN#x").as_deref(),
            Some("https://host.example/login")
        );
        let reason = safe_reason("start failed: token=SECRETTOKEN and abc", &["abc".into()]);
        assert!(
            !reason.contains("SECRETTOKEN") && !reason.contains("abc"),
            "{reason}"
        );
        assert_eq!(
            safe_reason(
                "no answer from https://u:p@host.example:8443/login?t=Ab3, \
                 nor (wss://h.example/connector/k9#f).",
                &[]
            ),
            "no answer from https://host.example:8443, nor (wss://h.example)."
        );
        assert_eq!(
            safe_reason("bad address ://x?t=Ab3", &[]),
            "bad address [address]"
        );
    }

    #[test]
    fn only_this_repos_request_files_name_requests() {
        let repo = Path::new("/r/one");
        let path = request_path(repo, "abc-1");
        assert_eq!(request_id(repo, &path).as_deref(), Some("abc-1"));
        assert_eq!(request_id(Path::new("/r/two"), &path), None);
        assert_eq!(request_id(repo, &result_path(repo, "abc-1")), None);
    }
}
