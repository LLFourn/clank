use std::time::Duration;

use clank_core::ids::AgentLabel;

use super::connect::ATTACH_BUDGET;
use super::custody::Custody;
use super::hosted::{Reading, Sources};
use super::retire::OwnedTasks;
use super::wire::{Control, Greeting, Holder, Note, PROTO, Welcome, Who, greeting_of};

mod serving;

#[cfg(test)]
pub(super) use self::serving::{
    ended, present_to, serve, serve_newcomer, serve_reports, serve_to, tend,
};
#[cfg(not(test))]
use self::serving::{ended, present_to, serve_newcomer, serve_reports, tend};

/// One connected waiter, as the fold owner sees it.
///
/// A QUEUE, never a socket. An earlier version held the stream and
/// awaited each write inside the owner's tick: a timeout there bounds
/// one write but not the loop, so three unread clients held the owner
/// for three budgets while it could not accept, drain arrivals or tick
/// — long enough for a healthy newcomer to exhaust its whole attach
/// budget unaccepted (codex on 38088ac). The owner now enqueues and
/// moves on; a writer task per client owns the socket.
/// What the monitor knows about a connected reader.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Served {
    Agent { author: AgentLabel },
    Observer,
    Helper { author: AgentLabel },
}

#[derive(Debug)]
pub(super) struct Client {
    pub(super) who: Served,
    pub(super) holder: Option<Holder>,
    pub(super) out: tokio::sync::mpsc::Sender<String>,
    /// The task that owns this client's socket.
    ///
    /// Retained so a retirement can tell TRANSMISSION from queue
    /// admission. `send_note` only `try_send`s into `out`; the frame
    /// is written by this task, and a detached handle is cancelled
    /// when the runtime shuts down — so returning from `start` could
    /// lose the final frame, and a transient in it has no WAL row to
    /// replay from (codex on 294c18d).
    ///
    /// `None` only in the tests that read a queue directly and have no
    /// socket for anyone to write to.
    pub(super) writer: Option<tokio::task::JoinHandle<Wrote>>,
    /// The writer's last frame, written after everything queued once
    /// the queue closes, so a full queue cannot drop it.
    pub(super) farewell: Option<tokio::sync::oneshot::Sender<String>>,
    /// What THIS client has been handed from its author's source logs.
    ///
    /// Per client, not per monitor: the monitor outlives every client,
    /// so a set it kept for itself would mean a client admitted later
    /// is told nothing about rows logged before it arrived. See
    /// `command_source::owed_from_log`.
    pub(super) shown: std::collections::HashSet<(String, u64, String)>,
    /// This connection, so that a delivery result is credited only to the
    /// connection that carried the presentation.
    pub(super) conn: u64,
    /// The hook's report this connection carries, until its verdict.
    /// Boxed: most connections carry none, and a client moves by value
    /// through the loop's channels.
    pub(super) report: Option<Box<Reporting>>,
    pub(super) admitted: Admitted,
    /// Its last serve met a log another process held, so what it was told
    /// lacks that log: the contention retry serves it again.
    pub(super) unavailable: bool,
}

/// When a connection was admitted, and the build its client said it runs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Admitted {
    /// Unix seconds.
    pub(super) at: u64,
    pub(super) build: Option<String>,
}

/// A report waiting on its verdict.
#[derive(Debug)]
pub(super) struct Reporting {
    pub(super) report: crate::cli::turn_wire::Report,
    /// Whether it has been stepped once: after that it is a parked report,
    /// not a new Stop.
    pub(super) stepped: bool,
    /// The connection's read side, with anything read past the hello, until
    /// the owner starts hearing it.
    pub(super) reader: Option<(Vec<u8>, tokio::net::unix::OwnedReadHalf)>,
}

/// A connection's identity, never reused within a monitor's life.
pub(super) fn next_conn() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// What a reporting connection said after its hello, or that it closed.
#[derive(Debug)]
pub(super) enum Heard {
    Line(String),
    Closed,
}

/// How many frames a client may fall behind before it is retired.
///
/// Small on purpose. The queue absorbs a brief hiccup; a reader that
/// cannot keep up with a 5-second fold tick is not slow, it is gone,
/// and holding frames for it would turn one stalled agent into
/// unbounded memory in the owner.
pub(super) const CLIENT_QUEUE: usize = 2;

/// Told when a client's writer ends, which is how the loop learns a
/// client went: a peer's close ends its writer.
pub(super) static CLIENT_GONE: tokio::sync::Notify = tokio::sync::Notify::const_new();

/// How long one client may take to accept a frame.
pub(super) const WRITE_BUDGET: Duration = Duration::from_secs(2);

/// Add a newcomer, having presented to IT and to nobody else.
///
/// A named step rather than loop plumbing because the thing worth
/// getting right is which clients a join touches: a queue is
/// `CLIENT_QUEUE` deep, so presenting to everybody on each arrival
/// lets a burst of joins retire established readers that were keeping
/// up. Returns nothing because a newcomer retired on its first frame
/// simply never joins, which is the same answer as never arriving.
pub(super) fn admit_into(
    repo: &std::path::Path,
    clients: &mut Vec<Client>,
    client: Client,
    state: Option<&crate::repo_state::RepoState>,
    sources: &mut Sources,
) -> Vec<tokio::task::JoinHandle<Wrote>> {
    let mut leaving = Vec::new();
    // EOF closes each writer's receiver. Remove those connections before
    // admitting a replacement; no task token or cleanup by session-wide key.
    let mut live = Vec::new();
    for old in clients.drain(..) {
        if old.out.is_closed() && old.who == client.who {
            if let Some(writer) = old.writer {
                leaving.push(writer);
            }
        } else {
            live.push(old);
        }
    }
    *clients = live;
    let duplicate = automatic_grok_client(repo, &client)
        && clients
            .iter()
            .any(|old| old.holder == client.holder && old.who == client.who);
    // A report's binding is the policy's to judge: it answers a stale one
    // Silent, which a refusal would turn into noise.
    let refusal = client.report.is_none().then(|| client_refusal(repo, &client)).flatten().or_else(|| duplicate.then(||
        "this Grok session already has a connected automatic wait; keep that task or cancel it before starting another".to_owned()));
    if let Some(why) = refusal {
        send_note(&client, &Note::Refused { why });
        if let Some(writer) = client.writer {
            leaving.push(writer);
        }
        return leaving;
    }
    // One parked wait per agent session: the one admitted last. The
    // harness never cancels a Claude or Codex hook's background wait, so
    // the older ones are left parked, and each would wake the agent with
    // the same work. Grok's automatic wait is a task Grok manages, so its
    // duplicate is refused above instead, keeping the connected one.
    let (older, kept): (Vec<Client>, Vec<Client>) = std::mem::take(clients)
        .into_iter()
        .partition(|old| same_wait(old, &client));
    *clients = kept;
    for old in older {
        leaving.extend(supersede(old));
    }
    let mut arriving = vec![client];
    if let Some(state) = state {
        // The REAL buffer: a transient retained while its author had
        // no client is owed to the first client that author gets, and
        // a newcomer that returns on admission work without it would
        // leave it waiting for a tick that may never find an eligible
        // client (codex on 1552f66).
        leaving.extend(present_to(
            repo,
            &mut arriving,
            state,
            None,
            sources,
            &mut Reading::afresh(),
        ));
        clients.append(&mut arriving);
        return leaving;
    }
    clients.append(&mut arriving);
    leaving
}

fn automatic_grok_client(repo: &std::path::Path, client: &Client) -> bool {
    let Served::Agent { author } = &client.who else {
        return false;
    };
    if !client.holder.as_ref().is_some_and(|h| {
        clank_core::harness::of(h.tool).behaviour.stop == clank_core::harness::Stop::Handoff
    }) {
        return false;
    }
    let Ok(cfg) = crate::agent_store::load_agent_config(repo, author) else {
        return false;
    };
    let home = crate::cli::grok_setup::launch_home(repo, author);
    matches!(
        crate::cli::team::resolve_effective_auto_mode(cfg.as_ref(), home.as_deref()),
        Ok(clank_core::AutoMode::On)
    )
}

pub(super) fn client_refusal(repo: &std::path::Path, client: &Client) -> Option<String> {
    let (Served::Agent { author } | Served::Helper { author }) = &client.who else {
        return None;
    };
    let holder = client.holder.as_ref()?;
    if !crate::agent_store::binding_matches(repo, author, holder.tool, &holder.session) {
        return Some(format!(
            "this {} session no longer owns `{author}`",
            holder.tool.as_str()
        ));
    }
    None
}

/// Which [`Served`] a client's declared identity earns, or why not.
///
/// A named function rather than the closure this was, so the roster
/// decision is reachable from a test without a socket.
pub(super) fn served_for(repo: &std::path::Path, who: Who) -> Result<Served, Welcome> {
    match who {
        Who::Observer => Ok(Served::Observer),
        Who::Control { .. } => Err(Welcome::Decline {
            proto: PROTO,
            why: "a control request is answered, not served".to_string(),
        }),
        Who::RecoveryHelper { author, .. } => match served_for(
            repo,
            Who::Agent {
                author,
                holder: None,
                has_cli_sources: false,
                turn_end: None,
            },
        )? {
            Served::Agent { author } => Ok(Served::Helper { author }),
            other => Ok(other),
        },
        Who::Agent { author, .. } => {
            let author = AgentLabel::parse(&author).map_err(|e| Welcome::Decline {
                proto: PROTO,
                why: format!("invalid author `{author}`: {e}"),
            })?;
            match crate::agent_store::resolve_role(repo, &author) {
                Ok(_) => Ok(Served::Agent { author }),
                Err(e) => Err(Welcome::Decline {
                    proto: PROTO,
                    why: format!("`{author}` is not on this repo's roster: {e}"),
                }),
            }
        }
    }
}

/// What an accepted connection turned out to want.
#[derive(Debug)]
pub(super) enum Greeted {
    Client(Client),
    /// A control request, for the owner loop to carry out and answer on
    /// `out` before the connection closes.
    Control {
        request: Control,
        out: tokio::sync::mpsc::Sender<String>,
        writer: tokio::task::JoinHandle<Wrote>,
    },
    /// A successor asked, and was told this monitor is retiring. The
    /// answer is already sent; what remains is to do it.
    StandDown,
}

#[cfg(test)]
impl Greeted {
    /// The client, for the tests that are about admission rather than
    /// about retirement.
    pub(super) fn client(self) -> Client {
        match self {
            Greeted::Client(c) => c,
            Greeted::StandDown => panic!("a hello was answered as a successor's ask"),
            Greeted::Control { .. } => panic!("a hello was answered as a control request"),
        }
    }
}

/// Read an opening frame and answer it, bounded and asynchronous.
pub(super) async fn greet(
    stream: tokio::net::UnixStream,
    repo: &std::path::Path,
) -> anyhow::Result<Greeted> {
    greet_within(stream, repo, ATTACH_BUDGET).await
}

/// [`greet`] with the budget supplied, so the timeout is testable
/// without waiting the production five seconds.
async fn greet_within(
    stream: tokio::net::UnixStream,
    repo: &std::path::Path,
    budget: Duration,
) -> anyhow::Result<Greeted> {
    tokio::time::timeout(budget, admit(stream, repo))
        .await
        .map_err(|_| anyhow::anyhow!("a client did not finish its hello within {budget:?}"))?
}

pub(super) async fn admit(
    stream: tokio::net::UnixStream,
    repo: &std::path::Path,
) -> anyhow::Result<Greeted> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    let mut io = tokio::io::BufReader::new(stream);
    let mut line = String::new();
    if io.read_line(&mut line).await? == 0 {
        anyhow::bail!("a client closed before saying hello");
    }

    let greeting = greeting_of(line.trim_end(), repo);
    if let Greeting::Obsolete(decline) = &greeting {
        // Told WHY before this process goes, so the client is not left
        // reading an abrupt EOF and guessing at it. Then the monitor
        // stands down, and the client's ordinary connect-or-spawn
        // starts the current one.
        let line = serde_json::to_string(decline)?;
        io.get_mut()
            .write_all(format!("{line}\n").as_bytes())
            .await?;
        return Ok(Greeted::StandDown);
    }

    if let Greeting::Client(hello) = &greeting
        && let Who::Control { request } = &hello.who
    {
        let request = request.clone();
        io.get_mut()
            .write_all(
                format!(
                    "{}\n",
                    serde_json::to_string(&Welcome::Ok { proto: PROTO })?
                )
                .as_bytes(),
            )
            .await?;
        let (out, queued) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
        let writer = tokio::spawn(write_to(
            io.into_inner(),
            queued,
            "a control client".to_string(),
        ));
        return Ok(Greeted::Control {
            request,
            out,
            writer,
        });
    }

    // An agent's author is resolved BEFORE admission: welcoming one the
    // roster does not know would register a connection nothing can ever
    // serve. An observer asks for no projection, so there is nothing to
    // resolve and nothing a roster could authorise — refusing it for
    // lacking an identity would only mean a panel could not subscribe.
    let admitted = match greeting {
        Greeting::Decline(d) => Err(d),
        Greeting::Obsolete(_) => unreachable!("returned above"),
        Greeting::Client(hello) => served_for(repo, hello.who.clone()).map(|who| {
            let hello = *hello;
            let (holder, report) = match hello.who {
                Who::Agent {
                    holder, turn_end, ..
                } => (holder, turn_end),
                Who::RecoveryHelper { holder, .. } => (Some(holder), None),
                Who::Observer | Who::Control { .. } => (None, None),
            };
            (who, holder, report, hello.build)
        }),
    };

    let welcome = match &admitted {
        Ok(_) => Welcome::Ok { proto: PROTO },
        Err(decline) => decline.clone(),
    };
    io.get_mut()
        .write_all(format!("{}\n", serde_json::to_string(&welcome)?).as_bytes())
        .await?;

    let (who, holder, report, build) = admitted.map_err(|d| anyhow::anyhow!("{d:?}"))?;
    if let Served::Agent { author, .. } = &who {
        tracing::debug!(%author, "monitor agent wait admitted");
    }
    let (out, queued) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
    let (farewell, last) = tokio::sync::oneshot::channel();
    let (writer, report) = match report {
        // A report is answered on this connection and its result read
        // back from it, so the read side outlives the writer: the result
        // arrives after the last verdict was written.
        Some(report) => {
            let early = io.buffer().to_vec();
            let (read, write) = io.into_inner().into_split();
            let writer = tokio::spawn(write_queued(
                write,
                queued,
                Some(last),
                who.name(),
                WRITE_BUDGET,
                std::future::pending(),
            ));
            (
                writer,
                Some(Box::new(Reporting {
                    report,
                    stepped: false,
                    reader: Some((early, read)),
                })),
            )
        }
        None => (
            tokio::spawn(write_to_within(
                io.into_inner(),
                queued,
                Some(last),
                who.name(),
                WRITE_BUDGET,
            )),
            None,
        ),
    };
    Ok(Greeted::Client(Client {
        who,
        holder,
        out,
        writer: Some(writer),
        shown: Default::default(),
        conn: next_conn(),
        admitted: Admitted {
            at: crate::cli::recovery::unix_now(),
            build,
        },
        report,
        farewell: Some(farewell),
        unavailable: false,
    }))
}

/// Read what a reporting connection says after its hello, one line at a
/// time, until it closes: the delivery result, and then EOF.
pub(super) async fn hear(
    conn: u64,
    (early, read): (Vec<u8>, tokio::net::unix::OwnedReadHalf),
    heard: tokio::sync::mpsc::UnboundedSender<(u64, Heard)>,
) {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt};
    const MAX_LINE: usize = 64 * 1024;
    let mut lines = tokio::io::BufReader::new(std::io::Cursor::new(early).chain(read));
    loop {
        let mut line = String::new();
        match (&mut lines)
            .take(MAX_LINE as u64)
            .read_line(&mut line)
            .await
        {
            Ok(0) | Err(_) => break,
            Ok(_) if !line.ends_with('\n') => break,
            Ok(_) => {
                if heard
                    .send((conn, Heard::Line(line.trim_end().to_string())))
                    .is_err()
                {
                    return;
                }
            }
        }
    }
    let _ = heard.send((conn, Heard::Closed));
}

impl Served {
    /// What to call this reader in a log line.
    pub(super) fn name(&self) -> String {
        match self {
            Served::Agent { author, .. } => author.to_string(),
            Served::Observer => "an observer".to_string(),
            Served::Helper { author } => format!("{author}'s recovery helper"),
        }
    }
}

/// Drive one client's socket, independently of the fold.
///
/// Owning the socket here is what makes the owner's enqueue
/// non-blocking. A write that times out or fails ends the task, which
/// closes the queue — and a closed queue is how the owner learns the
/// client is gone, without ever touching the socket itself.
/// How a client's writer ended.
///
/// Returned rather than inferred. A task that TERMINATED is not a
/// frame that was WRITTEN, and the ways it can terminate are different
/// answers a retirement has to report separately — counting every
/// awaited handle as a success called a closed peer's lost frame a
/// delivery (codex on d2b012d).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Wrote {
    /// The queue emptied and the sender was gone: everything queued
    /// reached the socket.
    Drained,
    /// The peer errored. What was still queued did not land.
    Failed,
    /// The peer stopped reading and a write did not finish inside
    /// `WRITE_BUDGET`.
    TimedOut,
}

pub(super) async fn write_to(
    socket: tokio::net::UnixStream,
    queued: tokio::sync::mpsc::Receiver<String>,
    author: String,
) -> Wrote {
    write_to_within(socket, queued, None, author, WRITE_BUDGET).await
}

/// [`write_to`] with the budget supplied, so giving up is testable
/// without waiting the production two seconds.
async fn write_to_within(
    socket: tokio::net::UnixStream,
    queued: tokio::sync::mpsc::Receiver<String>,
    farewell: Option<tokio::sync::oneshot::Receiver<String>>,
    author: String,
    budget: Duration,
) -> Wrote {
    use tokio::io::AsyncReadExt;
    let (mut read, write) = socket.into_split();
    let unexpected = async move {
        let mut byte = [0u8; 1];
        let _ = read.read(&mut byte).await;
    };
    let wrote = write_queued(write, queued, farewell, author, budget, unexpected).await;
    CLIENT_GONE.notify_one();
    wrote
}

/// Write what is queued to `socket` until the queue closes, or until
/// `peer` says the other end spoke out of turn or went away.
async fn write_queued(
    mut socket: impl tokio::io::AsyncWrite + Unpin,
    mut queued: tokio::sync::mpsc::Receiver<String>,
    mut farewell: Option<tokio::sync::oneshot::Receiver<String>>,
    author: String,
    budget: Duration,
    peer: impl std::future::Future<Output = ()>,
) -> Wrote {
    use tokio::io::AsyncWriteExt;
    tokio::pin!(peer);
    loop {
        let line = tokio::select! {
            line = queued.recv() => match line {
                Some(line) => line,
                // Closed: the last frame, if one was left, after all
                // that was queued before it.
                None => match farewell.take().and_then(|mut last| last.try_recv().ok()) {
                    Some(line) => line,
                    None => return Wrote::Drained,
                },
            },
            () = &mut peer => return Wrote::Failed,
        };
        match tokio::time::timeout(budget, socket.write_all(line.as_bytes())).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                eprintln!("monitor: `{author}` closed: {e}");
                return Wrote::Failed;
            }
            Err(_) => {
                eprintln!("monitor: `{author}` stopped reading");
                return Wrote::TimedOut;
            }
        }
    }
}

/// A session's background `clank wait`, as the monitor holds it: an
/// agent's open connection that holds the session and carries no report.
/// Its registration is the only evidence that the wait runs; it drops when
/// the wait exits.
pub(super) fn registered_waiter(c: &Client) -> Option<(&AgentLabel, &str)> {
    match (&c.who, &c.holder, &c.report) {
        (Served::Agent { author }, Some(holder), None) if !c.out.is_closed() => {
            Some((author, holder.session.as_str()))
        }
        _ => None,
    }
}

/// How long a monitor serves before a missing connection means a missing
/// waiter: a waiter that outlived the last monitor sees its socket close,
/// and attaches again within this.
pub(super) const WAITERS_SETTLE: Duration = EARLIER_WAITERS_RETRY.saturating_add(ATTACH_BUDGET);

/// The longest a waiter of an earlier build, which retried on a clock,
/// paused between attempts. One may still be parked when this monitor
/// starts; a waiter of this build attempts as soon as this one publishes.
const EARLIER_WAITERS_RETRY: Duration = Duration::from_secs(8);

/// The deliverer `c` is, if it is one: an agent's open connection that
/// work can be handed down, a wait or a report that parks, with the tool
/// and session it holds. An observer, a recovery helper, a report answered
/// once and a wait holding no session are none. Whether it holds the
/// binding its label has now is recovery's to say, against the binding it
/// already holds: nothing is read from disk for it.
pub(super) fn waiter(c: &Client) -> Option<(AgentLabel, crate::cli::recovery::Waiter)> {
    let Served::Agent { author } = &c.who else {
        return None;
    };
    let holder = c.holder.as_ref()?;
    if c.out.is_closed() || c.report.as_ref().is_some_and(|r| !r.report.parks()) {
        return None;
    }
    Some((
        author.clone(),
        crate::cli::recovery::Waiter {
            tool: holder.tool,
            session: holder.session.clone(),
            since: c.admitted.at,
            build: c.admitted.build.clone(),
        },
    ))
}

/// The connection table as recovery is told it: each label's deliverers
/// in the order they were admitted, and whether this monitor has `served`
/// long enough for an agent with none to have none.
pub(super) fn waiters(clients: &[Client], served: Duration) -> crate::cli::recovery::Waiters {
    let mut connected: std::collections::BTreeMap<_, Vec<_>> = Default::default();
    for (author, waiter) in clients.iter().filter_map(waiter) {
        connected.entry(author).or_default().push(waiter);
    }
    crate::cli::recovery::Waiters {
        connected,
        settled: served >= WAITERS_SETTLE,
    }
}

/// The parked reports whose hook can wait no longer: each gets the verdict
/// its tool takes at its deadline, and is retired. One woken by it is past
/// the Stop it reported, which the monitor that owns the repo records in
/// `custody`; one that no longer does records nothing.
pub(super) fn expire_reports(
    clients: &mut Vec<Client>,
    mut custody: Option<&mut Custody>,
    now: u64,
) -> Vec<tokio::task::JoinHandle<Wrote>> {
    let mut leaving = Vec::new();
    let (expired, kept): (Vec<Client>, Vec<Client>) =
        std::mem::take(clients).into_iter().partition(|c| {
            c.report
                .as_ref()
                .is_some_and(|r| r.stepped && r.report.deadline().is_some_and(|at| at <= now))
        });
    *clients = kept;
    for client in expired {
        let tool = client
            .report
            .as_ref()
            .map(|r| r.report.tool())
            .unwrap_or(clank_core::Tool::Claude);
        let frame = crate::cli::turn_custody::Turns::deadline_frame(tool);
        if let (
            Served::Agent { author },
            crate::cli::turn_wire::WireVerdict::Notify,
            Some(custody),
        ) = (&client.who, &frame.verdict, custody.as_deref_mut())
        {
            custody.woken(author);
        }
        send_note(&client, &Note::Verdict(frame));
        if let Some(writer) = client.writer {
            leaving.push(writer);
        }
    }
    leaving
}

/// The soonest deadline a held report's hook has: `None` when no held
/// report has one.
pub(super) fn report_deadline(clients: &[Client]) -> Option<u64> {
    clients
        .iter()
        .filter_map(|c| c.report.as_ref().filter(|r| r.stepped))
        .filter_map(|r| r.report.deadline())
        .min()
}

/// Send one note, and say whether that client may be kept.
///
/// `try_send`, never `send`: the owner must not wait on a client. A
/// full queue means this reader has fallen a whole `CLIENT_QUEUE` of
/// ticks behind, and a closed one means its writer task has already
/// given up — both are retirement. A frame that will not serialise is
/// this process's fault rather than the client's, so it costs nothing.
/// Whether `old` is the same agent session's wait as `new`: both an
/// agent's, for one author, with one and the same holder.
fn same_wait(old: &Client, new: &Client) -> bool {
    matches!(
        (&old.who, &new.who),
        (Served::Agent { author: a }, Served::Agent { author: b }) if a == b
    ) && old.holder.is_some()
        && old.holder == new.holder
}

/// Retire `client` because a newer wait of its session was admitted: its
/// writer sends what was queued, then `Superseded`, then ends.
fn supersede(client: Client) -> Option<tokio::task::JoinHandle<Wrote>> {
    if let (Some(farewell), Ok(line)) = (client.farewell, serde_json::to_string(&Note::Superseded))
    {
        let _ = farewell.send(format!("{line}\n"));
    }
    drop(client.out);
    client.writer
}

impl Client {
    /// Drop this client's queue, which ends its writer once it has
    /// written what is queued: that writer, to observe as it ends.
    pub(super) fn hang_up(self) -> Option<tokio::task::JoinHandle<Wrote>> {
        let Client { out, writer, .. } = self;
        drop(out);
        writer
    }
}

pub(super) fn send_note(client: &Client, note: &Note) -> bool {
    let Ok(line) = serde_json::to_string(note) else {
        return true;
    };
    match client.out.try_send(format!("{line}\n")) {
        Ok(()) => true,
        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
            eprintln!(
                "monitor: `{}` is {CLIENT_QUEUE} frames behind; dropping it",
                client.who.name()
            );
            false
        }
        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
            eprintln!("monitor: `{}` is gone; dropping it", client.who.name());
            false
        }
    }
}

/// The connections the loop serves: what greeted ones arrive on, what
/// reporting ones say, and the writers of the ones it retired.
///
/// Its receivers are open to the loop only for its `select!` to receive
/// from.
pub(super) struct Clients {
    pub(super) heard: tokio::sync::mpsc::UnboundedReceiver<(u64, Heard)>,
    heard_tx: tokio::sync::mpsc::UnboundedSender<(u64, Heard)>,
    /// Writers of clients already retired — on a wake, or for falling
    /// behind — whose last frame may still be unwritten. Pruned as they
    /// finish, so this never grows with the number of wakes a long-lived
    /// monitor serves, and awaited by `retire` so a stand-down does not
    /// cut one short.
    leaving: Vec<tokio::task::JoinHandle<Wrote>>,
    list: Vec<Client>,
    pub(super) arrivals: tokio::sync::mpsc::UnboundedReceiver<Greeted>,
    joined: tokio::sync::mpsc::UnboundedSender<Greeted>,
}

impl Clients {
    /// No one connected yet.
    pub(super) fn new() -> Self {
        let (joined, arrivals) = tokio::sync::mpsc::unbounded_channel::<Greeted>();
        let (heard_tx, heard) = tokio::sync::mpsc::unbounded_channel::<(u64, Heard)>();
        Clients {
            heard,
            heard_tx,
            leaving: Vec::new(),
            list: Vec::new(),
            arrivals,
            joined,
        }
    }

    /// `list` connected.
    #[cfg(test)]
    pub(super) fn of(list: Vec<Client>) -> Self {
        Clients {
            list,
            ..Clients::new()
        }
    }

    /// What a test's greeting hands its client over on.
    #[cfg(test)]
    pub(super) fn joining(&self) -> tokio::sync::mpsc::UnboundedSender<Greeted> {
        self.joined.clone()
    }

    pub(super) fn report_deadline(&self) -> Option<u64> {
        report_deadline(&self.list)
    }

    pub(super) fn waiters(&self, served: Duration) -> crate::cli::recovery::Waiters {
        waiters(&self.list, served)
    }

    /// Whether a client's last serve met a log another process held.
    pub(super) fn unavailable(&self) -> bool {
        self.list.iter().any(|c| c.unavailable)
    }

    /// Whether `author` holds a report not yet answered.
    pub(super) fn awaits_verdict(&self, author: &AgentLabel) -> bool {
        self.list.iter().any(|c| {
            matches!(&c.who, Served::Agent { author: a } if a == author)
                && c.report.as_ref().is_some_and(|r| !r.stepped)
        })
    }

    pub(super) fn holds_report(
        &self,
        author: &AgentLabel,
        session: &clank_core::SessionId,
    ) -> bool {
        self.list.iter().any(|c| {
            matches!(&c.who, Served::Agent { author: a } if a == author)
                && c.report
                    .as_ref()
                    .is_some_and(|r| r.report.session() == session)
        })
    }

    /// Send `note` to `author`'s recovery helper holding `session`:
    /// whether one took it.
    pub(super) fn to_helper(&self, author: &AgentLabel, session: &str, note: &Note) -> bool {
        let helper = self.list.iter().find(|c| {
            matches!(&c.who, Served::Helper { author: a } if a == author)
                && c.holder.as_ref().is_some_and(|h| h.session == session)
        });
        helper.is_some_and(|c| send_note(c, note))
    }

    /// Wake the observers: what they show has moved.
    pub(super) fn beat_observers(&self) {
        for client in self.list.iter().filter(|c| c.who == Served::Observer) {
            send_note(client, &Note::Beat);
        }
    }

    /// Greet `stream` off the loop: a peer that connects and says nothing
    /// must not stall folding or anyone else's registration.
    pub(super) fn accept(
        &self,
        stream: tokio::net::UnixStream,
        repo: &std::sync::Arc<std::path::PathBuf>,
        owned: &mut OwnedTasks,
    ) {
        let repo = repo.clone();
        let joined = self.joined.clone();
        owned.greet(tokio::spawn(async move {
            match greet(stream, &repo).await {
                // Ownership transfers on a SUCCESSFUL send. A
                // failed one means the receiver is gone, and
                // dropping the `Greeted` there would detach
                // the writer `admit` just spawned — a socket
                // task nothing accounts for, after the
                // admission cutoff (codex on b37b6b6). So the
                // greeting tears down what it could not hand
                // over, and the cutoff observes THIS task
                // ending.
                Ok(greeted) => {
                    if let Err(returned) = joined.send(greeted)
                        && let Greeted::Client(c) = returned.0
                        && let Some(writer) = c.writer
                    {
                        writer.abort();
                        let _ = writer.await;
                    }
                }
                Err(e) => eprintln!("monitor: a client was not admitted: {e}"),
            }
        }));
    }

    /// A greeted client reached the loop. What its connection says after
    /// its hello is heard from now; one reaching a monitor that no longer
    /// owns the repo is kept, not served, for the retirement this leads to
    /// to present to with everyone else. The client to admit otherwise.
    pub(super) fn arrived(
        &mut self,
        mut client: Client,
        owner: bool,
        owned: &mut OwnedTasks,
    ) -> Option<Client> {
        if let Some(reader) = client.report.as_mut().and_then(|r| r.reader.take()) {
            owned.hear(tokio::spawn(hear(
                client.conn,
                reader,
                self.heard_tx.clone(),
            )));
        }
        if !owner {
            self.list.push(client);
            return None;
        }
        Some(client)
    }

    /// Answer a control client with `note`, keeping its writer until it
    /// has written it.
    pub(super) fn answer(
        &mut self,
        out: tokio::sync::mpsc::Sender<String>,
        writer: tokio::task::JoinHandle<Wrote>,
        note: &Note,
    ) {
        if let Ok(line) = serde_json::to_string(note) {
            let _ = out.try_send(format!("{line}\n"));
        }
        drop(out);
        self.leaving.push(writer);
    }

    /// Admit `client`, serving it at once rather than at the next tick —
    /// the producers have been running since the monitor started, so a
    /// newcomer may already be owed what they logged — but to the
    /// NEWCOMER ALONE. A queue is `CLIENT_QUEUE` deep, so presenting to
    /// everybody on each arrival would let a burst of joins retire
    /// established readers that were keeping up perfectly well (codex on
    /// 0555c8f).
    pub(super) fn admit(
        &mut self,
        client: Client,
        repo: &std::path::Path,
        state: Option<&crate::repo_state::RepoState>,
        sources: &mut Sources,
        custody: &mut Custody,
    ) {
        self.leaving.retain(|w| !w.is_finished());
        let reporting = match (&client.who, &client.report) {
            (Served::Agent { author }, Some(_)) => Some(author.clone()),
            _ => None,
        };
        let child = client
            .report
            .as_ref()
            .is_some_and(|r| !r.report.reaches_agent());
        self.leaving
            .extend(admit_into(repo, &mut self.list, client, state, sources));
        if let Some(author) = reporting {
            // Read afresh, as a newcomer's items are: the cached
            // backlog may still hold a row acknowledged since.
            self.leaving.extend(serve_newcomer(
                repo,
                &mut self.list,
                state,
                &author,
                child,
                sources,
                custody,
            ));
        }
    }

    pub(super) fn present(
        &mut self,
        repo: &std::path::Path,
        state: &crate::repo_state::RepoState,
        only: Option<&AgentLabel>,
        sources: &mut Sources,
        reading: &mut Reading,
    ) {
        let leaving = present_to(repo, &mut self.list, state, only, sources, reading);
        self.leaving.extend(leaving);
    }

    pub(super) fn serve_reports(
        &mut self,
        repo: &std::path::Path,
        state: Option<&crate::repo_state::RepoState>,
        only: Option<&AgentLabel>,
        reading: &mut Reading,
        sources: &mut Sources,
        custody: &mut Custody,
    ) {
        let leaving = serve_reports(repo, &mut self.list, state, only, reading, sources, custody);
        self.leaving.extend(leaving);
    }

    pub(super) fn tend(
        &mut self,
        repo: &std::path::Path,
        state: Option<&crate::repo_state::RepoState>,
        sources: &mut Sources,
        custody: &mut Custody,
    ) {
        let leaving = tend(repo, &mut self.list, state, sources, custody);
        self.leaving.extend(leaving);
    }

    pub(super) fn ended(
        &mut self,
        conn: u64,
        repo: &std::path::Path,
        state: Option<&crate::repo_state::RepoState>,
        sources: &mut Sources,
        custody: &mut Custody,
    ) {
        let leaving = ended(conn, repo, &mut self.list, state, sources, custody);
        self.leaving.extend(leaving);
    }

    pub(super) fn expire_reports(&mut self, custody: Option<&mut Custody>, now: u64) {
        let leaving = expire_reports(&mut self.list, custody, now);
        self.leaving.extend(leaving);
    }

    /// Take what was admitted while the stand-down was being decided.
    ///
    /// A client whose handshake completed moments ago is in the CHANNEL,
    /// not in the list. Left there it is dropped with its writer still
    /// owning a socket — the detached task the whole inventory exists to
    /// prevent (codex on 056c991). Closing first is what makes this
    /// finite: no further sender can add to it while we drain.
    pub(super) fn claim_late_arrivals(&mut self) -> usize {
        self.arrivals.close();
        let mut took = 0;
        while let Ok(late) = self.arrivals.try_recv() {
            match late {
                Greeted::Client(c) => {
                    self.list.push(c);
                    took += 1;
                }
                // A second successor, asking for what is already
                // happening. It was acknowledged on its own connection.
                Greeted::StandDown => {}
                // Told why, and drained with the readers so its writer is
                // owned to the end rather than detached.
                Greeted::Control { out, writer, .. } => {
                    let client = Client {
                        who: Served::Observer,
                        holder: None,
                        out,
                        writer: Some(writer),
                        shown: Default::default(),
                        conn: 0,
                        admitted: Default::default(),
                        unavailable: false,
                        report: None,
                        farewell: None,
                    };
                    send_note(
                        &client,
                        &Note::Refused {
                            why: "the monitor is standing down".to_string(),
                        },
                    );
                    self.list.push(client);
                    took += 1;
                }
            }
        }
        took
    }

    /// Everything connected and leaving, for a retirement to see out.
    pub(super) fn retire(&mut self) -> (Vec<Client>, Vec<tokio::task::JoinHandle<Wrote>>) {
        (
            std::mem::take(&mut self.list),
            std::mem::take(&mut self.leaving),
        )
    }
}

#[cfg(test)]
#[path = "clients_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "superseding_tests.rs"]
mod superseding_tests;
