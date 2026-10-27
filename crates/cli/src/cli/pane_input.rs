//! Provider-aware terminal input shared by nudges, events, recovery, check-ins,
//! the web bridge and user commands: the one boundary every pane write crosses.
//! Callers resolve target identity; accepted terminal bytes do not prove a stopped turn.
//!
//! Three rules hold here and nowhere else:
//!
//! - Ordinary text is an [`Admitted`]: structural control bytes are made
//!   inert and a leading `!` cannot reach a harness's shell-mode trigger.
//! - An ordinary terminal write proceeds only when the pane's input is
//!   positively read as not in shell mode. A harness whose input cannot be
//!   read (Grok, OpenCode) is never typed into.
//! - Every write sequence holds the pane's one cross-process lock
//!   ([`lock_pane`]) from its look through its last byte.
//!
//! The user-command writer ([`type_user_command`]) is the only pane writer
//! that takes a raw line, and it takes only a verified one.

use super::composer::Composer;
use super::open_zellij::PaneWrite;
use clank_core::Tool;

/// The two actions that say `text` to a pane and press Enter, as
/// argv after the session prefix; nothing for empty text. The text
/// follows `--`, so a message beginning with `-` is a message, not a
/// flag. Enter is byte 13 through `write`, not a newline through
/// `write-chars`: a newline is what a multi-line paste is made of,
/// and a chat prompt treats those differently from Return.
/// Codex uses its own submit sequence below; a bare CR can insert a
/// newline even after the paste-burst window has elapsed.
///
/// Not a pane MUTATION in the ownership gate's sense: typing into a
/// pane leaves its place in the layout to the reconciler, which is
/// what that gate protects.
pub(crate) fn say_argvs(pane: &str, text: &str) -> Option<[Vec<String>; 2]> {
    if text.is_empty() {
        return None;
    }
    let chars = ["action", "write-chars", "-p", pane, "--", text]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let enter = ["action", "write", "-p", pane, "13"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    Some([chars, enter])
}

/// Kitty keyboard protocol's Enter press: CSI 13;1:1u. Codex's TUI
/// distinguishes this from the raw CR used by shells and other agents.
pub(crate) fn codex_submit_argv(pane: &str) -> Vec<String> {
    [
        "action", "write", "-p", pane, "27", "91", "49", "51", "59", "49", "58", "49", "117",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Tab: Codex submits it at once when idle, and queues it behind a running
/// turn.
pub(crate) fn codex_queue_argv(pane: &str) -> Vec<String> {
    ["action", "write", "-p", pane, "9"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// Use Ctrl+C for Grok, matching the installed harness and observed behavior.
/// Other harnesses use Escape. Select the key from the pane's tool,
/// never from an inherited environment variable in the web server.
pub(crate) fn stop_argv(pane: &str, tool: clank_core::vocab::Tool) -> Vec<String> {
    let byte = if tool == clank_core::vocab::Tool::Grok {
        "3"
    } else {
        "27"
    };
    ["action", "write", "-p", pane, byte]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// Interrupt `pane`'s turn with its tool's key, under the pane's lock.
/// Writes no text, so it needs no reading of the input.
pub(crate) fn stop_pane(session: &str, pane: &str, tool: Tool) -> anyhow::Result<()> {
    run_sync(async {
        let _lock = lock_pane(session, pane)?;
        Zellij { session, pane }
            .write(stop_argv(pane, tool))
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
    })
}

/// Text an ordinary path may type into, or hand natively to, a harness:
/// built only by [`Admitted::new`], so a caller cannot pass raw text.
///
/// - CRLF and lone CR become LF: a CR would submit mid-message and start
///   the rest on an empty prompt. LF stays, as the multi-line mechanism.
/// - Tab becomes a space: Tab is Codex's submit and queue key.
/// - Every other control character, ESC, DEL and C1 included, becomes
///   U+FFFD, so the change is visible: Esc interrupts, and a CSI sequence
///   can be a key such as the kitty Enter.
/// - A `!` opening the text after its leading whitespace gets U+00B7 `·`
///   before it, so no harness reads it as the shell-mode trigger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Admitted(String);

impl Admitted {
    pub(crate) fn new(raw: &str) -> Self {
        let mut text = neutralised(raw);
        let lead = text.len() - text.trim_start().len();
        if text[lead..].starts_with('!') {
            text.insert(lead, BANG_GUARD);
        }
        Admitted(text)
    }

    /// The admitted text.
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// The fixed non-whitespace character put before a leading `!`: every
/// harness's trigger is a `!` as the input's first character, and nothing
/// trims a non-whitespace character away.
const BANG_GUARD: char = '\u{b7}';

fn neutralised(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push('\n');
            }
            '\n' => out.push('\n'),
            '\t' => out.push(' '),
            c if c.is_control() => out.push('\u{fffd}'),
            c => out.push(c),
        }
    }
    out
}

/// A live observation that Claude Code runs a user command typed as
/// `!<line>` into its empty input followed by Enter as one bash command,
/// for lines up to `max_command` characters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UserCommandEvidence {
    /// The Claude Code version observed, as the harness reports it.
    pub(crate) version: &'static str,
    /// The longest line observed to run whole.
    pub(crate) max_command: usize,
    /// What was typed and seen: quotes, `$`, a line at the bound, how the
    /// shell mode drew in an `--ansi` dump, and whether a Stop hook fired.
    pub(crate) how: &'static str,
}

/// The committed observation for the user-command keystrokes. None until a
/// human records one, so every user command is refused at dispatch.
pub(crate) const USER_COMMAND_EVIDENCE: Option<UserCommandEvidence> = None;

/// Whether an ordinary sequence may type into the pane now: one dump, taken
/// under the lock the caller holds, must read its input as taking ordinary
/// text.
async fn admit_write(term: &impl Terminal, tool: Tool) -> Result<(), String> {
    if matches!(tool, Tool::Grok | Tool::OpenCode) {
        return Err(format!(
            "{tool}'s input cannot be read, so whether it is in shell mode cannot be known; \
             nothing was typed"
        ));
    }
    let read = term
        .dump()
        .await
        .map(|screen| super::composer::composer(tool, &screen))
        .unwrap_or(Composer::Unreadable);
    if read.takes_ordinary_text() {
        Ok(())
    } else {
        Err(format!(
            "its input reads as {}, not as a prompt that takes ordinary text; nothing \
             was typed",
            match read {
                Composer::Shell => "shell mode",
                _ => "unreadable",
            }
        ))
    }
}

/// One pane's terminal, as the write sequences reach it.
pub(crate) trait Terminal {
    /// The pane's screen with its colours kept, or `None` when it could not
    /// be read within its budget.
    async fn dump(&self) -> Option<String>;
    /// One bounded write action; a failure says whether anything ran.
    async fn write(&self, argv: Vec<String>) -> Result<(), PaneWrite>;
}

/// A zellij pane.
pub(crate) struct Zellij<'a> {
    pub(crate) session: &'a str,
    pub(crate) pane: &'a str,
}

impl Terminal for Zellij<'_> {
    async fn dump(&self) -> Option<String> {
        super::open_zellij::dump_pane_screen(self.session, self.pane).await
    }

    async fn write(&self, argv: Vec<String>) -> Result<(), PaneWrite> {
        super::open_zellij::write_pane_action_classified(self.session, self.pane, &argv).await
    }
}

/// The one write lock of a pane, held from a sequence's look through its
/// last byte, across processes. Released when dropped.
pub(crate) struct PaneLock {
    _file: std::fs::File,
}

/// Block until this process holds the lock of `pane` in `session`. Async
/// callers take it through `event_interrupt::blocking`, off the executor.
pub(crate) fn lock_pane(session: &str, pane: &str) -> anyhow::Result<PaneLock> {
    super::monitor::ensure_runtime_dir().map_err(anyhow::Error::msg)?;
    let dir = super::monitor::runtime_dir(unsafe { libc::getuid() });
    lock_pane_in(&dir, session, pane)
}

fn lock_pane_in(dir: &std::path::Path, session: &str, pane: &str) -> anyhow::Result<PaneLock> {
    Ok(reserve_in(dir, session, pane, Reserve::Wait)?.expect("a waiting lock is taken"))
}

/// How a writer takes a pane's lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reserve {
    /// Block until it is free.
    Wait,
    /// Take it only if it is free now: for the monitor's own writers, whose
    /// authorisation would be stale behind a user command typed meanwhile.
    Defer,
}

/// The lock of `pane` in `session`, or `None` when `how` is `Defer` and
/// another writer holds it.
fn reserve_in(
    dir: &std::path::Path,
    session: &str,
    pane: &str,
    how: Reserve,
) -> anyhow::Result<Option<PaneLock>> {
    use anyhow::Context;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    let path = pane_lock_path(dir, session, pane);
    // Never unlinked: replacing a locked inode would admit a second holder.
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .with_context(|| format!("opening pane lock `{}`", path.display()))?;
    let op = match how {
        Reserve::Wait => libc::LOCK_EX,
        Reserve::Defer => libc::LOCK_EX | libc::LOCK_NB,
    };
    loop {
        // SAFETY: valid owned fd; flock has no memory effects.
        if unsafe { libc::flock(file.as_raw_fd(), op) } == 0 {
            return Ok(Some(PaneLock { _file: file }));
        }
        let error = std::io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(libc::EWOULDBLOCK) if how == Reserve::Defer => return Ok(None),
            _ => return Err(error).with_context(|| format!("locking `{}`", path.display())),
        }
    }
}

fn pane_lock_path(dir: &std::path::Path, session: &str, pane: &str) -> std::path::PathBuf {
    // Length-delimited, so no two (session, pane) pairs share a name.
    let mut hash = blake3::Hasher::new();
    for field in [session, pane] {
        hash.update(&(field.len() as u64).to_le_bytes());
        hash.update(field.as_bytes());
    }
    dir.join(format!("pane-{}.lock", &hash.finalize().to_hex()[..32]))
}

async fn lock_pane_async(session: &str, pane: &str, how: Reserve) -> Result<PaneLock, String> {
    let taken = super::event_interrupt::blocking(|| {
        super::monitor::ensure_runtime_dir().map_err(anyhow::Error::msg)?;
        let dir = super::monitor::runtime_dir(unsafe { libc::getuid() });
        reserve_in(&dir, session, pane, how)
    });
    match taken {
        Ok(Some(lock)) => Ok(lock),
        Ok(None) => Err(RESERVED.to_string()),
        Err(e) => Err(format!("{e:#}")),
    }
}

/// Why a deferring writer typed nothing.
const RESERVED: &str = "its pane is reserved by another clank writer; nothing was typed";

/// Run `work` to completion from synchronous code that is off the async
/// executor: on the runtime it belongs to, or a small one of its own.
fn run_sync<F: std::future::Future>(work: F) -> F::Output {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => handle.block_on(work),
        Err(_) => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a current-thread runtime")
            .block_on(work),
    }
}

/// Where an ordinary write goes: the pane, and the tool it runs.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Target<'a> {
    pub(crate) session: &'a str,
    pub(crate) pane: &'a str,
    pub(crate) tool: Tool,
}

/// Interrupt the agent's current turn, then submit `text`.
///
/// In THAT order. It used to submit first and interrupt after, and for a
/// deliberate reason: Claude queues an Enter pressed during a busy turn
/// and sends queued messages after Esc, so for a BUSY agent the message
/// survived. An IDLE one — the usual reason for a nudge, since the usual
/// cause is a stop hook that failed to park — started a turn on the
/// message and was then interrupted out of that very turn. Observed in
/// `~/src/life`, where the instruction to repair a stale binding arrived
/// and was cancelled before it could be acted on.
///
/// What this order GUARANTEES is narrow and exact: clank's own interrupt
/// never follows clank's own message. For an IDLE agent that is the whole
/// fix, because the interrupt key is inert at an idle prompt for every
/// provider below, so the message starts a turn nothing cancels.
///
/// For a BUSY agent the terminal fallback is BEST-EFFORT, and says so. The
/// message is typed straight after the interrupt key, with no fixed pause:
/// a terminal gives no "ready" acknowledgement, and an arbitrary delay
/// would only move the race rather than end it (codex on 307bbb3). What
/// becomes of input that arrives while a turn is still being cancelled is
/// the provider's, and it is established for one of them:
///
/// - Claude Code: its interactive-mode documentation says Enter while busy
///   QUEUES the message and Escape sends queued messages next — the
///   behaviour the old order relied on. Documented, not live-verified for
///   input landing mid-cancellation.
/// - Codex, Grok, opencode: not established. A message typed during a
///   cancellation may be queued, kept as a draft, or lost with the turn.
///   For Grok specifically, queue survival across a cancellation has never
///   been verified.
///
/// Interrupting an idle agent first is harmless for every provider clank
/// drives, which is what makes this order safe. Taken from each
/// installed binary's own help text, so the claim can be re-checked when
/// one is upgraded:
///
/// - Claude Code (Esc): interrupts a running turn. At an idle prompt one
///   Esc arms a double-press that opens rewind; the typed message after
///   it disarms that.
/// - Codex (Esc): interrupts a running turn. Idle, one Esc arms "again to
///   edit previous message"; typing disarms it. Its quit is Ctrl+C, which
///   clank never sends it.
/// - Grok (Ctrl+C): cancels a running turn. Idle with an empty prompt it
///   has no action — Grok quits on Ctrl+Q. Two limits it documents itself:
///   with a draft in the composer the first press clears the draft
///   instead of cancelling, and a further Ctrl+C while a cancel is in
///   progress "escalates toward quit".
/// - opencode (Esc): interrupts a running turn; idle, no documented
///   action. Its Ctrl+C arms an exit, which is why clank sends it Esc.
///
/// One caller never sends two interrupt keys in a row: the message always
/// sits between them. Two callers racing at one pane could, and nothing
/// here prevents that.
///
/// Accepted bytes prove neither that a turn stopped nor that the message
/// was read. On failure nothing is resent, and neither failure is called
/// safe to repeat. An interrupt whose delivery was not confirmed stops the
/// sequence before any message is typed — but the transport reports a
/// timeout as "acceptance is unknown", so the key may have landed, and a
/// blind retry could send a second interrupt into a cancellation already
/// under way, which for Grok escalates toward quit. A submission that was
/// not confirmed may already be partly typed.
///
/// `clank nudge` sends this without reading the pane first: it is the
/// escape hatch for a stuck agent, so a person's draft in the input is
/// submitted with the text, and an input in shell mode runs it.
pub(crate) async fn nudge(target: Target<'_>, text: &Admitted) -> Result<(), Unconfirmed> {
    let _lock = lock_pane_async(target.session, target.pane, Reserve::Wait)
        .await
        .map_err(Unconfirmed::Refused)?;
    let term = Zellij {
        session: target.session,
        pane: target.pane,
    };
    interrupt_then_type(&term, target, text).await
}

async fn interrupt_and_submit_reserving(
    target: Target<'_>,
    text: &Admitted,
    how: Reserve,
) -> Result<(), Unconfirmed> {
    let _lock = lock_pane_async(target.session, target.pane, how)
        .await
        .map_err(Unconfirmed::Refused)?;
    let term = Zellij {
        session: target.session,
        pane: target.pane,
    };
    interrupt_and_submit_on(&term, target, text).await
}

/// [`interrupt_and_submit_async`] on a supplied terminal, the lock already
/// held.
///
/// So the REAL bytes, per provider, can be recorded in order without a
/// terminal. The order is also asserted on [`interrupt_then_submit`]
/// alone, but that cannot see which operation is handed in as which: a
/// production path that passed the submit as the interrupt would satisfy
/// it. This is the seam that sees the keys themselves.
async fn interrupt_and_submit_on(
    term: &impl Terminal,
    target: Target<'_>,
    text: &Admitted,
) -> Result<(), Unconfirmed> {
    if text.is_empty() {
        return Err(Unconfirmed::Empty);
    }
    admit_write(term, target.tool)
        .await
        .map_err(Unconfirmed::Refused)?;
    interrupt_then_type(term, target, text).await
}

async fn interrupt_then_type(
    term: &impl Terminal,
    target: Target<'_>,
    text: &Admitted,
) -> Result<(), Unconfirmed> {
    if text.is_empty() {
        return Err(Unconfirmed::Empty);
    }
    let (pane, tool) = (target.pane, target.tool);
    let write = |argv| async { term.write(argv).await.map_err(|e| anyhow::anyhow!("{e}")) };
    interrupt_then_submit(
        || write(stop_argv(pane, tool)),
        || async {
            let Some(argvs) = say_argvs(pane, text.as_str()) else {
                return Ok(());
            };
            for (index, mut argv) in argvs.into_iter().enumerate() {
                if index == 1 && tool == Tool::Codex {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    argv = codex_submit_argv(pane);
                }
                write(argv).await?;
            }
            Ok(())
        },
        false,
    )
    .await
}

/// Type `text` into the pane and submit it the way its tool expects, and
/// nothing else: never an interrupt key. Bounded; a failure says whether
/// nothing was written or some of it may have been. A refusal by the
/// admission rule, or a pane another writer holds, is `NotStarted`: the
/// monitor's typed sends defer rather than wait.
pub(crate) async fn submit_async(target: Target<'_>, text: &Admitted) -> Result<(), PaneWrite> {
    let _lock = lock_pane_async(target.session, target.pane, Reserve::Defer)
        .await
        .map_err(PaneWrite::NotStarted)?;
    let term = Zellij {
        session: target.session,
        pane: target.pane,
    };
    submit_on(&term, target, text).await
}

/// [`submit_async`] on a supplied terminal, the lock already held, so the
/// keys it sends can be recorded.
async fn submit_on(
    term: &impl Terminal,
    target: Target<'_>,
    text: &Admitted,
) -> Result<(), PaneWrite> {
    let Some(argvs) = say_argvs(target.pane, text.as_str()) else {
        return Err(PaneWrite::NotStarted("there was nothing to say".into()));
    };
    admit_write(term, target.tool)
        .await
        .map_err(PaneWrite::NotStarted)?;
    for (index, mut argv) in argvs.into_iter().enumerate() {
        if index == 1 && target.tool == Tool::Codex {
            // Past Codex's paste window, so the key is a key: Tab, which
            // Codex queues behind a running turn, where Enter would steer
            // into it (`resume::EVIDENCE`).
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            argv = codex_queue_argv(target.pane);
        }
        match term.write(argv).await {
            Ok(()) => {}
            // Only before anything was written is a failure a no-send.
            Err(PaneWrite::NotStarted(e)) if index == 0 => return Err(PaneWrite::NotStarted(e)),
            Err(e) => return Err(PaneWrite::Unknown(e.to_string())),
        }
    }
    Ok(())
}

/// A fresh prompt is the handoff across native cancellation hooks. User content
/// stays in its durable inbox: `hint` only names the pending events to read.
/// Role is deliberately resolved by Clank when the target reads, never captured
/// in terminal text before interruption. No receipt or acknowledgement is forged.
///
/// A pane another writer holds is `Refused`, leaving delivery pending: the
/// monitor runs these, and must not wait on a user command's reservation.
pub(crate) async fn wake_for_events_async(
    target: Target<'_>,
    hint: &Admitted,
) -> Result<(), Unconfirmed> {
    interrupt_and_submit_reserving(target, hint, Reserve::Defer).await
}

/// The order, separated from the terminal so it can be asserted.
///
/// Nothing at all happens for an empty message: interrupting an agent to
/// tell it nothing is the one outcome worse than the bug this order fixes.
async fn interrupt_then_submit<I, S>(
    interrupt: impl FnOnce() -> I,
    submit: impl FnOnce() -> S,
    empty: bool,
) -> Result<(), Unconfirmed>
where
    I: std::future::Future<Output = anyhow::Result<()>>,
    S: std::future::Future<Output = anyhow::Result<()>>,
{
    if empty {
        return Err(Unconfirmed::Empty);
    }
    interrupt().await.map_err(Unconfirmed::Interrupt)?;
    submit().await.map_err(Unconfirmed::Submit)
}

/// Why an interrupt-then-submit did not complete, as what the caller may
/// conclude about the terminal.
#[derive(Debug)]
pub(crate) enum Unconfirmed {
    /// Nothing was sent: there was no message.
    Empty,
    /// Nothing was sent: the admission rule could not establish that the
    /// pane takes ordinary text, or its lock could not be taken.
    Refused(String),
    /// The interrupt's delivery was not confirmed. No message was
    /// submitted, but the turn may have been interrupted.
    Interrupt(anyhow::Error),
    /// The interrupt was delivered and the message was not confirmed; some
    /// of it may have been typed.
    Submit(anyhow::Error),
}

impl std::fmt::Display for Unconfirmed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Unconfirmed::Empty => write!(f, "an empty nudge must not interrupt the agent"),
            Unconfirmed::Refused(why) => write!(f, "refused: {why}"),
            Unconfirmed::Interrupt(e) => write!(
                f,
                "the interrupt key's delivery was not confirmed, so no message was submitted; \
                 it may still have interrupted the turn, so do not repeat it blindly — a second \
                 interrupt during a cancellation can escalate (for Grok, toward quit): {e:#}"
            ),
            Unconfirmed::Submit(e) => write!(
                f,
                "the interrupt was delivered, but the message was not confirmed (some input may \
                 already have been typed); do not resend it automatically: {e:#}"
            ),
        }
    }
}

impl std::error::Error for Unconfirmed {}

/// Type `text` into `pane` and submit it the way `tool` expects, with no
/// interrupt, from synchronous code off the executor.
///
/// PANE ONLY: nothing here reads a stored session binding. The web bridge
/// wraps this with the app-server attempt it is willing to trust; the
/// submit-sequence knowledge lives here once. Codex's terminal fallback
/// waits past its paste-burst window, then sends a kitty-protocol Enter.
pub(crate) fn say_to_pane_for_tool(
    session: &str,
    pane: &str,
    tool: Tool,
    text: &Admitted,
) -> anyhow::Result<()> {
    run_sync(async {
        let _lock = lock_pane(session, pane)?;
        say_on(&Zellij { session, pane }, pane, tool, text).await
    })
}

async fn say_on(
    term: &impl Terminal,
    pane: &str,
    tool: Tool,
    text: &Admitted,
) -> anyhow::Result<()> {
    let Some(argvs) = say_argvs(pane, text.as_str()) else {
        return Ok(());
    };
    admit_write(term, tool).await.map_err(anyhow::Error::msg)?;
    for (i, mut argv) in argvs.into_iter().enumerate() {
        if i == 1 && tool == Tool::Codex {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            argv = codex_submit_argv(pane);
        }
        term.write(argv).await.map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    Ok(())
}

/// Type a verified user command into the pane: `!` and the line as one
/// `write-chars`, which Claude's empty input consumes as the switch to bash
/// mode, then Enter as byte 13.
///
/// The caller holds the pane's lock and has read the input as empty under
/// it; this checks nothing itself. `NotStarted` only when the first write
/// never ran.
pub(crate) async fn type_user_command(
    term: &impl Terminal,
    pane: &str,
    command: &super::user_command::envelope::Command,
) -> Result<(), PaneWrite> {
    let line = format!("!{}", command.line());
    let Some(argvs) = say_argvs(pane, &line) else {
        return Err(PaneWrite::NotStarted("there was nothing to type".into()));
    };
    for (index, argv) in argvs.into_iter().enumerate() {
        match term.write(argv).await {
            Ok(()) => {}
            Err(PaneWrite::NotStarted(e)) if index == 0 => return Err(PaneWrite::NotStarted(e)),
            Err(e) => return Err(PaneWrite::Unknown(e.to_string())),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::composer::fixtures::{claude, codex};
    use std::cell::RefCell;

    /// A pane that answers dumps with `screen` and writes with `answer`,
    /// recording every write in order.
    struct Fake<A: Fn(usize) -> Result<(), PaneWrite>> {
        screen: Option<String>,
        answer: A,
        written: RefCell<Vec<Vec<String>>>,
    }

    impl<A: Fn(usize) -> Result<(), PaneWrite>> Terminal for Fake<A> {
        async fn dump(&self) -> Option<String> {
            self.screen.clone()
        }
        async fn write(&self, argv: Vec<String>) -> Result<(), PaneWrite> {
            let n = self.written.borrow().len();
            self.written.borrow_mut().push(argv);
            (self.answer)(n)
        }
    }

    fn fake(screen: Option<String>) -> Fake<impl Fn(usize) -> Result<(), PaneWrite>> {
        Fake {
            screen,
            answer: |_| Ok(()),
            written: RefCell::new(Vec::new()),
        }
    }

    /// The screen each tool shows with an empty, ordinary input. Grok and
    /// OpenCode have no readable input, so theirs is never consulted.
    fn idle(tool: Tool) -> Option<String> {
        match tool {
            Tool::Claude => Some(claude(&["❯ "])),
            Tool::Codex => Some(codex(&["› "])),
            Tool::Grok | Tool::OpenCode => None,
        }
    }

    fn target(tool: Tool) -> Target<'static> {
        Target {
            session: "s",
            pane: "7",
            tool,
        }
    }

    /// Interrupt, then submit — once each, in that order.
    ///
    /// The reverse is the bug: an idle agent started a turn on the message
    /// and the interrupt that followed cancelled that very turn.
    #[tokio::test]
    async fn the_turn_is_interrupted_before_the_message_is_submitted() {
        let calls = RefCell::new(Vec::new());
        interrupt_then_submit(
            || async {
                calls.borrow_mut().push("interrupt");
                Ok(())
            },
            || async {
                calls.borrow_mut().push("submit");
                Ok(())
            },
            false,
        )
        .await
        .unwrap();
        assert_eq!(*calls.borrow(), ["interrupt", "submit"]);
    }

    /// An interrupt whose delivery is UNKNOWN submits nothing, and is not
    /// called safe to repeat.
    ///
    /// The transport reports a timeout after it may already have written
    /// the key as "acceptance is unknown". The earlier wording wrapped that
    /// in "it is safe to retry", producing "safe to retry: … acceptance is
    /// unknown" — and a retry can send a second interrupt into a
    /// cancellation already under way, which for Grok escalates toward quit
    /// (codex on 307bbb3).
    #[tokio::test]
    async fn an_interrupt_of_unknown_delivery_submits_nothing_and_promises_nothing() {
        let error = interrupt_then_submit(
            || async {
                anyhow::bail!(
                    "terminal input transport timed out after 3s; acceptance is unknown; stderr: "
                )
            },
            || async { panic!("must not submit after an unconfirmed interrupt") },
            false,
        )
        .await
        .unwrap_err();
        let said = format!("{error:#}");
        assert!(!said.contains("safe to retry"), "{said}");
        assert!(said.contains("no message was submitted"), "{said}");
        assert!(said.contains("do not repeat it blindly"), "{said}");
        assert!(
            said.contains("acceptance is unknown"),
            "the transport's own words: {said}"
        );
    }

    /// A submission that fails after the interrupt is reported as possibly
    /// partial, and never resent.
    #[tokio::test]
    async fn an_unconfirmed_submission_is_never_resent() {
        let submits = RefCell::new(0);
        let error = interrupt_then_submit(
            || async { Ok(()) },
            || async {
                *submits.borrow_mut() += 1;
                anyhow::bail!("Enter was refused")
            },
            false,
        )
        .await
        .unwrap_err();
        let said = format!("{error:#}");
        assert!(said.contains("the interrupt was delivered"), "{said}");
        assert!(
            said.contains("some input may already have been typed"),
            "{said}"
        );
        assert!(said.contains("do not resend"), "{said}");
        assert_eq!(*submits.borrow(), 1, "exactly one attempt");
    }

    /// Dropping the sequence while the interrupt is still in flight types
    /// nothing: the owned I/O ends with its owner.
    #[tokio::test]
    async fn a_sequence_cancelled_mid_interrupt_submits_nothing() {
        let submitted = std::sync::atomic::AtomicBool::new(false);
        let sequence = interrupt_then_submit(
            std::future::pending::<anyhow::Result<()>>,
            || async {
                submitted.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            },
            false,
        );
        let raced = tokio::time::timeout(std::time::Duration::from_millis(20), sequence).await;
        assert!(raced.is_err(), "still interrupting when its owner gave up");
        assert!(!submitted.load(std::sync::atomic::Ordering::SeqCst));
    }

    /// The keys actually written, in order, for every provider — through
    /// the same function both production callers reach.
    ///
    /// The interrupt key is FIRST and is the provider's own; the message
    /// and its submit follow, Codex's paste-safe Enter included. Grok's
    /// interrupt is Ctrl+C, the rest Esc.
    #[tokio::test(start_paused = true)]
    async fn every_provider_is_interrupted_with_its_own_key_before_the_message() {
        let key = |argv: &Vec<String>| argv[4..].join(" ");
        assert_eq!(key(&stop_argv("7", Tool::Grok)), "3");
        assert_eq!(key(&stop_argv("7", Tool::OpenCode)), "27");
        for (tool, interrupt, submit) in [
            (Tool::Claude, "27", "13"),
            (Tool::Codex, "27", "27 91 49 51 59 49 58 49 117"),
        ] {
            let term = fake(idle(tool));
            interrupt_and_submit_on(&term, target(tool), &Admitted::new("hello"))
                .await
                .unwrap();
            let written = term.written.into_inner();
            assert_eq!(written.len(), 3, "{tool:?}: {written:?}");
            assert_eq!(key(&written[0]), interrupt, "{tool:?} is interrupted first");
            assert_eq!(written[1][1], "write-chars", "{tool:?}: then the message");
            assert_eq!(written[1].last().unwrap(), "hello");
            assert_eq!(key(&written[2]), submit, "{tool:?}: then its own submit");
        }
    }

    /// `clank nudge` is blunt: whatever the pane shows, even nothing
    /// readable, every tool gets its interrupt key, the text and its submit.
    #[tokio::test]
    async fn a_nudge_types_into_any_tool_without_reading_the_pane() {
        let key = |argv: &Vec<String>| argv[4..].join(" ");
        for (tool, interrupt, submit) in [
            (Tool::Claude, "27", "13"),
            (Tool::Codex, "27", "27 91 49 51 59 49 58 49 117"),
            (Tool::OpenCode, "27", "13"),
            (Tool::Grok, "3", "13"),
        ] {
            let term = fake(None);
            interrupt_then_type(&term, target(tool), &Admitted::new("hello"))
                .await
                .unwrap();
            let written = term.written.into_inner();
            assert_eq!(written.len(), 3, "{tool:?}: {written:?}");
            assert_eq!(key(&written[0]), interrupt, "{tool:?}");
            assert_eq!(written[1].last().unwrap(), "hello", "{tool:?}");
            assert_eq!(key(&written[2]), submit, "{tool:?}");
        }
    }

    /// A failure writing the INTERRUPT is reported as one, and nothing
    /// follows it.
    ///
    /// This is what tells the production path's two operations apart. Key
    /// ORDER alone cannot: a path that wrote the interrupt key as the first
    /// step of its submit produced exactly the same three keys. It reports
    /// the failure as an unconfirmed MESSAGE, though, and that is wrong in
    /// the way that matters — it invites a resend.
    #[tokio::test]
    async fn a_failed_interrupt_write_is_an_interrupt_failure_and_types_nothing() {
        let term = Fake {
            screen: idle(Tool::Claude),
            answer: |_| {
                Err(PaneWrite::Unknown(
                    "terminal input transport timed out; acceptance is unknown".into(),
                ))
            },
            written: RefCell::new(Vec::new()),
        };
        let error = interrupt_and_submit_on(&term, target(Tool::Claude), &Admitted::new("hello"))
            .await
            .unwrap_err();
        assert_eq!(term.written.borrow().len(), 1, "only the interrupt");
        let said = format!("{error:#}");
        assert!(
            said.contains("interrupt key's delivery was not confirmed"),
            "{said}"
        );
        assert!(!said.contains("message was not confirmed"), "{said}");
    }

    /// A submit-only send writes the message and the key its harness
    /// queues on, and no interrupt key: Codex's Tab, never its Enter.
    #[tokio::test(start_paused = true)]
    async fn a_submit_only_send_writes_no_interrupt() {
        for (tool, submit) in [(Tool::Claude, "13"), (Tool::Codex, "9")] {
            let term = fake(idle(tool));
            submit_on(&term, target(tool), &Admitted::new("hello"))
                .await
                .unwrap();
            let written = term.written.into_inner();
            assert_eq!(written.len(), 2, "{tool:?}: {written:?}");
            assert_eq!(written[0][1], "write-chars", "{tool:?}");
            assert_eq!(written[1][4..].join(" "), submit, "{tool:?}");
        }
    }

    /// A send that never started wrote nothing; one that failed after
    /// anything was written may have typed it.
    #[tokio::test(start_paused = true)]
    async fn a_failed_submit_says_whether_anything_was_written() {
        let first = Fake {
            screen: idle(Tool::Claude),
            answer: |_| Err(PaneWrite::NotStarted("no zellij".into())),
            written: RefCell::new(Vec::new()),
        };
        let tried = submit_on(&first, target(Tool::Claude), &Admitted::new("hi")).await;
        assert!(matches!(tried, Err(PaneWrite::NotStarted(_))));
        let second = Fake {
            screen: idle(Tool::Claude),
            answer: |n| match n {
                0 => Ok(()),
                _ => Err(PaneWrite::NotStarted("gone".into())),
            },
            written: RefCell::new(Vec::new()),
        };
        let tried = submit_on(&second, target(Tool::Claude), &Admitted::new("hi")).await;
        assert!(
            matches!(tried, Err(PaneWrite::Unknown(_))),
            "the text was typed"
        );
    }

    #[tokio::test]
    async fn empty_nudge_has_no_side_effects() {
        assert!(
            interrupt_then_submit(
                || async { panic!("must not interrupt") },
                || async { panic!("must not submit") },
                true,
            )
            .await
            .is_err()
        );
    }

    /// The structural bytes: CR and CRLF become LF and LF stays; Tab becomes
    /// a space; ESC, other C0, DEL and C1 become U+FFFD.
    #[test]
    fn structural_bytes_are_neutralised_and_lines_kept() {
        let said = Admitted::new("a\r\nb\rc\nd\te\u{1b}[13;1:1uf\u{7}\u{7f}\u{85}g");
        assert_eq!(
            said.as_str(),
            "a\nb\nc\nd e\u{fffd}[13;1:1uf\u{fffd}\u{fffd}\u{fffd}g"
        );
        assert!(!said.as_str().contains('\r'));
    }

    /// A leading `!`, after any leading whitespace, gets `·` before it; a
    /// `!` anywhere else, or after an LF
    /// inside the message, is left as text. A neutralised control byte
    /// before a `!` is not whitespace, so nothing is added there.
    #[test]
    fn a_leading_bang_is_encoded() {
        assert_eq!(Admitted::new("!git push").as_str(), "\u{b7}!git push");
        assert_eq!(Admitted::new("  \n\t!rm").as_str(), "  \n \u{b7}!rm");
        assert_eq!(Admitted::new("\r!x").as_str(), "\n\u{b7}!x");
        assert_eq!(Admitted::new("say !this").as_str(), "say !this");
        assert_eq!(Admitted::new("line\n!two").as_str(), "line\n!two");
        assert_eq!(Admitted::new("\u{1b}!x").as_str(), "\u{fffd}!x");
    }

    /// Every ordinary sequence types the ADMITTED text: a raw `!` line and
    /// a CR handed in arrive encoded, LF kept.
    #[tokio::test(start_paused = true)]
    async fn every_ordinary_sequence_types_the_admitted_text() {
        let raw = "!git push\rnext\nline";
        let expect = "\u{b7}!git push\nnext\nline";
        let interrupt = fake(idle(Tool::Claude));
        interrupt_and_submit_on(&interrupt, target(Tool::Claude), &Admitted::new(raw))
            .await
            .unwrap();
        assert_eq!(interrupt.written.borrow()[1].last().unwrap(), expect);
        let submit = fake(idle(Tool::Codex));
        submit_on(&submit, target(Tool::Codex), &Admitted::new(raw))
            .await
            .unwrap();
        assert_eq!(submit.written.borrow()[0].last().unwrap(), expect);
        let say = fake(idle(Tool::Claude));
        say_on(&say, "7", Tool::Claude, &Admitted::new(raw))
            .await
            .unwrap();
        assert_eq!(say.written.borrow()[0].last().unwrap(), expect);
    }

    /// Claude and Codex: every path class is refused, writing nothing, on a
    /// shell composer, an unreadable dump, a dump that never came, and for
    /// Codex a `!` draft. A draft on an ordinary prompt still takes text.
    #[tokio::test(start_paused = true)]
    async fn claude_and_codex_refuse_a_pane_not_read_as_ordinary() {
        let refused = [
            (Tool::Claude, Some(claude(&["! git push"]))),
            (Tool::Claude, Some(claude(&["! "]))),
            (Tool::Claude, Some("❯ old\n╭ Allow? ╮".to_string())),
            (Tool::Claude, None),
            (Tool::Codex, Some(codex(&["› !git"]))),
            (Tool::Codex, Some("› \n never ends".to_string())),
            (Tool::Codex, None),
        ];
        for (tool, screen) in refused {
            let interrupt = fake(screen.clone());
            let tried =
                interrupt_and_submit_on(&interrupt, target(tool), &Admitted::new("x")).await;
            assert!(
                matches!(tried, Err(Unconfirmed::Refused(_))),
                "{tool:?} {screen:?}"
            );
            assert!(interrupt.written.borrow().is_empty());
            let submit = fake(screen.clone());
            let tried = submit_on(&submit, target(tool), &Admitted::new("x")).await;
            assert!(matches!(tried, Err(PaneWrite::NotStarted(_))), "{tool:?}");
            assert!(submit.written.borrow().is_empty());
            let say = fake(screen.clone());
            assert!(say_on(&say, "7", tool, &Admitted::new("x")).await.is_err());
            assert!(say.written.borrow().is_empty());
        }
        for (tool, screen) in [
            (Tool::Claude, claude(&["❯ half a thought"])),
            (Tool::Codex, codex(&["› half a thought"])),
        ] {
            let submit = fake(Some(screen));
            submit_on(&submit, target(tool), &Admitted::new("x"))
                .await
                .unwrap();
            assert_eq!(submit.written.borrow().len(), 2, "{tool:?}");
        }
    }

    /// Grok and OpenCode: every path is refused for its tool, writing
    /// nothing and never asking for a dump it could not read.
    #[tokio::test(start_paused = true)]
    async fn grok_and_opencode_are_never_typed_into() {
        let unreadable = |why: &str| why.contains("input cannot be read");
        for tool in [Tool::Grok, Tool::OpenCode] {
            let term = fake(None);
            let tried = interrupt_and_submit_on(&term, target(tool), &Admitted::new("x")).await;
            assert!(
                matches!(&tried, Err(Unconfirmed::Refused(why)) if unreadable(why)),
                "{tool:?}: {tried:?}"
            );
            let tried = submit_on(&term, target(tool), &Admitted::new("x")).await;
            assert!(
                matches!(&tried, Err(PaneWrite::NotStarted(why)) if unreadable(why)),
                "{tool:?}: {tried:?}"
            );
            assert!(say_on(&term, "7", tool, &Admitted::new("x")).await.is_err());
            assert!(term.written.borrow().is_empty(), "{tool:?}");
        }
    }

    /// The user-command writer types `!` and the line as one write, then
    /// Enter as byte 13; a failure before anything ran is `NotStarted`,
    /// one after the text is `Unknown`.
    #[tokio::test]
    async fn the_user_command_keystrokes() {
        let command = crate::cli::user_command::envelope::Command::for_test("echo \"$HOME\"");
        let term = fake(None);
        type_user_command(&term, "7", &command).await.unwrap();
        let written = term.written.into_inner();
        assert_eq!(
            written,
            vec![
                say_argvs("7", "!echo \"$HOME\"").unwrap()[0].clone(),
                vec![
                    "action".into(),
                    "write".into(),
                    "-p".into(),
                    "7".into(),
                    "13".into()
                ],
            ]
        );
        let never = Fake {
            screen: None,
            answer: |_| Err(PaneWrite::NotStarted("no zellij".into())),
            written: RefCell::new(Vec::new()),
        };
        assert!(matches!(
            type_user_command(&never, "7", &command).await,
            Err(PaneWrite::NotStarted(_))
        ));
        let half = Fake {
            screen: None,
            answer: |n| match n {
                0 => Ok(()),
                _ => Err(PaneWrite::NotStarted("gone".into())),
            },
            written: RefCell::new(Vec::new()),
        };
        assert!(matches!(
            type_user_command(&half, "7", &command).await,
            Err(PaneWrite::Unknown(_))
        ));
    }

    /// Two sequences on one pane, one ordinary and one a user command, each
    /// under the pane's lock from different threads, never interleave; a
    /// different pane's lock is a different lock.
    #[test]
    fn one_panes_writers_never_interleave() {
        let dir = tempfile::tempdir().unwrap();
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        struct Logged(std::sync::Arc<std::sync::Mutex<Vec<String>>>, &'static str);
        impl Terminal for Logged {
            async fn dump(&self) -> Option<String> {
                Some(claude(&["❯ "]))
            }
            async fn write(&self, argv: Vec<String>) -> Result<(), PaneWrite> {
                self.0
                    .lock()
                    .unwrap()
                    .push(format!("{}:{}", self.1, argv[1]));
                std::thread::sleep(std::time::Duration::from_millis(20));
                Ok(())
            }
        }
        let held = lock_pane_in(dir.path(), "s", "7").unwrap();
        let threads: Vec<_> = ["ordinary", "command"]
            .into_iter()
            .map(|who| {
                let (dir, log) = (dir.path().to_path_buf(), log.clone());
                std::thread::spawn(move || {
                    let _lock = lock_pane_in(&dir, "s", "7").unwrap();
                    let term = Logged(log, who);
                    run_sync(async {
                        if who == "ordinary" {
                            submit_on(&term, target(Tool::Claude), &Admitted::new("x"))
                                .await
                                .unwrap();
                        } else {
                            let c = crate::cli::user_command::envelope::Command::for_test("ls");
                            type_user_command(&term, "7", &c).await.unwrap();
                        }
                    });
                })
            })
            .collect();
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(log.lock().unwrap().is_empty(), "both wait for the holder");
        let other = lock_pane_in(dir.path(), "s", "8");
        assert!(other.is_ok(), "another pane is not blocked");
        drop(held);
        for t in threads {
            t.join().unwrap();
        }
        let log = log.lock().unwrap().clone();
        assert_eq!(log.len(), 4, "{log:?}");
        assert_eq!(
            log[0].split(':').next(),
            log[1].split(':').next(),
            "{log:?}"
        );
        assert_eq!(
            log[2].split(':').next(),
            log[3].split(':').next(),
            "{log:?}"
        );
        assert_ne!(
            log[0].split(':').next(),
            log[2].split(':').next(),
            "{log:?}"
        );
    }

    /// A deferring writer takes a free pane and gives way to a held one,
    /// never waiting: the monitor's own writers must not block behind a user
    /// command's reservation, nor type on an authorisation made before it.
    #[test]
    fn a_deferring_writer_gives_way_to_a_held_pane() {
        let dir = tempfile::tempdir().unwrap();
        let held = lock_pane_in(dir.path(), "s", "7").unwrap();
        assert!(
            reserve_in(dir.path(), "s", "7", Reserve::Defer)
                .unwrap()
                .is_none()
        );
        assert!(
            reserve_in(dir.path(), "s", "8", Reserve::Defer)
                .unwrap()
                .is_some()
        );
        drop(held);
        assert!(
            reserve_in(dir.path(), "s", "7", Reserve::Defer)
                .unwrap()
                .is_some()
        );
    }

    /// The monitor's own writers, recovery's typed sends and check-ins
    /// (`submit_async`) and event wakes (`wake_for_events_async`), give way
    /// to a pane another writer holds, typing nothing, rather than wait.
    /// Driven through those entry points with the real runtime lock held on
    /// a test-only pane, so they return before any terminal is reached.
    #[test]
    fn the_monitors_writers_defer_on_a_reserved_pane() {
        let session = format!("clank-test-{}", std::process::id());
        let pane = "reserved-pane";
        let held = lock_pane(&session, pane).unwrap();
        let (done, answers) = std::sync::mpsc::channel();
        let (s, p) = (session.clone(), pane.to_string());
        std::thread::spawn(move || {
            run_sync(async {
                let target = Target {
                    session: &s,
                    pane: &p,
                    tool: Tool::Claude,
                };
                let text = Admitted::new("x");
                let submitted = submit_async(target, &text).await;
                let woke = wake_for_events_async(target, &text).await;
                let _ = done.send((submitted, woke));
            })
        });
        let Ok((submitted, woke)) = answers.recv_timeout(std::time::Duration::from_secs(10)) else {
            std::mem::forget(held);
            panic!("a monitor writer waited on a reserved pane");
        };
        assert!(
            matches!(&submitted, Err(PaneWrite::NotStarted(why)) if why == RESERVED),
            "{submitted:?}"
        );
        assert!(
            matches!(&woke, Err(Unconfirmed::Refused(why)) if why == RESERVED),
            "{woke:?}"
        );
        drop(held);
        let dir = super::super::monitor::runtime_dir(unsafe { libc::getuid() });
        let _ = std::fs::remove_file(pane_lock_path(&dir, &session, pane));
    }

    /// Lock names are length-delimited: no two (session, pane) pairs alias.
    #[test]
    fn pane_lock_names_do_not_alias() {
        let dir = std::path::Path::new("/nowhere");
        assert_ne!(
            pane_lock_path(dir, "ab", "c"),
            pane_lock_path(dir, "a", "bc")
        );
        assert_eq!(pane_lock_path(dir, "s", "7"), pane_lock_path(dir, "s", "7"));
    }
}
