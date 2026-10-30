//! Provider-aware terminal input shared by nudges, events, recovery, check-ins,
//! the web bridge and user commands: the one boundary every pane write crosses.
//! Callers resolve target identity; accepted terminal bytes do not prove a stopped turn.
//!
//! Four rules hold here and nowhere else:
//!
//! - Ordinary text is an [`Admitted`]: structural control bytes are made
//!   inert and a leading `!` cannot reach a harness's shell-mode trigger.
//! - An ordinary terminal write proceeds only when the pane's input is
//!   positively read as not in shell mode. A harness whose input cannot be
//!   read (Grok, OpenCode) is typed into only by `clank nudge`.
//! - A prompt typed into Claude or Codex by a nudge, an event wake or a
//!   recovery send ([`deliver`]) touches no text but its own, and is
//!   delivered only when the harness's own record says so.
//! - Every write sequence holds the pane's one cross-process lock
//!   ([`lock_pane`]) from its look through its last byte.
//!
//! The user-command writer ([`type_user_command`]) is the only pane writer
//! that takes a raw line, and it takes only a verified one.

use super::composer::Composer;
use super::open_zellij::PaneWrite;
use super::prompt_record::{Offloaded, Receipts, Stored};
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
    Some([chars, enter_argv(pane)])
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
    if !readable(tool) {
        return Err(unreadable(tool));
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

/// Where an ordinary write goes: the pane, the tool it runs, and the agent
/// whose binding names the record that confirms it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Target<'a> {
    pub(crate) session: &'a str,
    pub(crate) pane: &'a str,
    pub(crate) tool: Tool,
    pub(crate) repo: &'a std::path::Path,
    pub(crate) label: &'a clank_core::ids::AgentLabel,
}

impl Target<'_> {
    fn receipts(&self) -> Offloaded<Stored> {
        Offloaded::new(Stored {
            repo: self.repo.to_path_buf(),
            label: self.label.clone(),
        })
    }
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
/// under way, which for Grok escalates toward quit.
///
/// `clank nudge` is the escape hatch for a stuck agent. Into Claude and
/// Codex it types by [`deliver`], as every automatic writer does; Grok and
/// OpenCode, whose input cannot be read, get the interrupt key, the text
/// and Enter unread, and only the transport answers for them.
pub(crate) async fn nudge(target: Target<'_>, text: &Admitted) -> Result<Delivered, Unconfirmed> {
    let _lock = lock_pane_async(target.session, target.pane, Reserve::Wait)
        .await
        .map_err(Unconfirmed::Refused)?;
    let term = Zellij {
        session: target.session,
        pane: target.pane,
    };
    interrupt_then_type(&term, &mut target.receipts(), target, text).await
}

async fn interrupt_and_submit_reserving(
    target: Target<'_>,
    text: &Admitted,
    how: Reserve,
) -> Result<Delivered, Unconfirmed> {
    let _lock = lock_pane_async(target.session, target.pane, how)
        .await
        .map_err(Unconfirmed::Refused)?;
    let term = Zellij {
        session: target.session,
        pane: target.pane,
    };
    interrupt_and_submit_on(&term, &mut target.receipts(), target, text).await
}

/// [`wake_for_events_async`] on a supplied terminal and record, the lock
/// already held, so the keys it sends can be recorded and the records it
/// reads scripted.
async fn interrupt_and_submit_on(
    term: &impl Terminal,
    receipts: &mut impl Receipts,
    target: Target<'_>,
    text: &Admitted,
) -> Result<Delivered, Unconfirmed> {
    if text.is_empty() {
        return Err(Unconfirmed::Empty);
    }
    if !readable(target.tool) {
        return Err(Unconfirmed::Refused(unreadable(target.tool)));
    }
    Ok(deliver(term, receipts, target, text, true).await?)
}

/// The manual nudge's sequence, on a supplied terminal and record.
async fn interrupt_then_type(
    term: &impl Terminal,
    receipts: &mut impl Receipts,
    target: Target<'_>,
    text: &Admitted,
) -> Result<Delivered, Unconfirmed> {
    if text.is_empty() {
        return Err(Unconfirmed::Empty);
    }
    if readable(target.tool) {
        return Ok(deliver(term, receipts, target, text, true).await?);
    }
    let (pane, tool) = (target.pane, target.tool);
    let unconfirmed = |e: PaneWrite| anyhow::anyhow!("{e}");
    term.write(stop_argv(pane, tool))
        .await
        .map_err(|e| Unconfirmed::Interrupt(unconfirmed(e)))?;
    for argv in say_argvs(pane, text.as_str()).expect("the text is not empty") {
        term.write(argv)
            .await
            .map_err(|e| Unconfirmed::Submit(unconfirmed(e)))?;
    }
    Ok(Delivered::Unread)
}

/// Type `text` into the pane and submit it the way its tool expects, and
/// nothing else: never an interrupt key. Bounded; a failure says whether
/// nothing was written or some of it may have been. A refusal, or a pane
/// another writer holds, is `NotStarted`: the monitor's typed sends defer
/// rather than wait.
pub(crate) async fn submit_async(
    target: Target<'_>,
    text: &Admitted,
) -> Result<Delivered, PaneWrite> {
    let _lock = lock_pane_async(target.session, target.pane, Reserve::Defer)
        .await
        .map_err(PaneWrite::NotStarted)?;
    let term = Zellij {
        session: target.session,
        pane: target.pane,
    };
    submit_on(&term, &mut target.receipts(), target, text).await
}

/// [`submit_async`] on a supplied terminal and record, the lock already
/// held.
async fn submit_on(
    term: &impl Terminal,
    receipts: &mut impl Receipts,
    target: Target<'_>,
    text: &Admitted,
) -> Result<Delivered, PaneWrite> {
    if text.is_empty() {
        return Err(PaneWrite::NotStarted("there was nothing to say".into()));
    }
    if !readable(target.tool) {
        return Err(PaneWrite::NotStarted(unreadable(target.tool)));
    }
    deliver(term, receipts, target, text, false)
        .await
        .map_err(|failed| match failed {
            Failed::NotSent(why) => PaneWrite::NotStarted(why),
            Failed::Interrupt(why) | Failed::Indeterminate(why) => PaneWrite::Unknown(why),
        })
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
) -> Result<Delivered, Unconfirmed> {
    interrupt_and_submit_reserving(target, hint, Reserve::Defer).await
}

/// How a prompt reached the harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Delivered {
    /// The harness recorded the whole text.
    Whole,
    /// The harness recorded a non-empty proper suffix of the text: its
    /// start was lost in typing. Delivered, so never resent.
    Altered,
    /// Typed into a harness whose input and record clank cannot read: only
    /// the transport accepted it.
    Unread,
}

/// Why [`deliver`] cannot say the prompt arrived.
#[derive(Debug)]
enum Failed {
    /// Nothing was written.
    NotSent(String),
    /// The interrupt key's delivery was not confirmed; nothing followed it.
    Interrupt(String),
    /// Bytes may have arrived.
    Indeterminate(String),
}

impl From<Failed> for Unconfirmed {
    fn from(failed: Failed) -> Self {
        match failed {
            Failed::NotSent(why) => Unconfirmed::Refused(why),
            Failed::Interrupt(why) => Unconfirmed::Interrupt(anyhow::anyhow!(why)),
            Failed::Indeterminate(why) => Unconfirmed::Submit(anyhow::anyhow!(why)),
        }
    }
}

fn readable(tool: Tool) -> bool {
    matches!(tool, Tool::Claude | Tool::Codex)
}

fn unreadable(tool: Tool) -> String {
    format!(
        "{tool}'s input cannot be read, so whether it is in shell mode cannot be known; \
         nothing was typed"
    )
}

/// The whole attempt, from its first look to its last read.
const ATTEMPT: std::time::Duration = std::time::Duration::from_secs(8);
/// How long typed text, or an input being cleared, has to show.
const SHOWN_WITHIN: std::time::Duration = std::time::Duration::from_secs(2);
/// How long after the submit key the input is looked at again.
const SETTLE: std::time::Duration = std::time::Duration::from_secs(1);
const POLL: std::time::Duration = std::time::Duration::from_millis(100);
/// Past Codex's paste window, so its submit key is a key.
const CODEX_PASTE_WINDOW: std::time::Duration = std::time::Duration::from_millis(200);

/// Ctrl-C, which empties Claude's non-empty idle input and does nothing
/// else (Claude Code 2.1.308).
fn clear_argv(pane: &str) -> Vec<String> {
    ["action", "write", "-p", pane, "3"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// Type `text` into a Claude or Codex pane, submit it, and confirm it by
/// the harness's own record, under the lock the caller holds. `interrupt`
/// presses the tool's interrupt key first, after the first look.
///
/// One deadline bounds the whole attempt: every read, write and pause ends
/// by it, nothing is sent once it has passed, and nothing that finishes at
/// or after it counts. A write cut off by it may have landed.
///
/// Only text that can be clank's own is acted on: the payload whole, or a
/// non-empty proper suffix of it (its start lost on the way in). Any other
/// draft is refused untouched. Claude's input is cleared with Ctrl-C only
/// while no turn runs, since Ctrl-C would interrupt one.
async fn deliver(
    term: &impl Terminal,
    receipts: &mut impl Receipts,
    target: Target<'_>,
    text: &Admitted,
    interrupt: bool,
) -> Result<Delivered, Failed> {
    let mut attempt = Attempt {
        term,
        target,
        payload: text.as_str(),
        deadline: tokio::time::Instant::now() + ATTEMPT,
        wrote: false,
        cleared: false,
    };
    let first = attempt.look(false).await;
    match first.held {
        Held::Empty | Held::Whole => {}
        Held::Suffix if target.tool == Tool::Claude && (interrupt || !first.busy) => {}
        Held::Suffix => return Err(attempt.fail("part of this text, which cannot be cleared now")),
        Held::Arriving | Held::Other(_) => return Err(attempt.fail(first.held.what())),
    }
    let marked = match attempt.within(receipts.mark()).await {
        Some(marked) => marked,
        None => Err("its record could not be read in time".into()),
    };
    let now = if interrupt {
        attempt.interrupt().await?;
        attempt.look(false).await
    } else {
        first
    };
    match now.held {
        Held::Whole => {}
        Held::Empty => attempt.type_and_see().await?,
        Held::Suffix if attempt.may_clear(&now) => {
            attempt.clear().await?;
            attempt.type_and_see().await?;
        }
        Held::Suffix => return Err(attempt.fail("part of this text, which cannot be cleared now")),
        Held::Arriving | Held::Other(_) => return Err(attempt.fail(now.held.what())),
    }
    let submit = match (target.tool, interrupt) {
        // Tab, which Codex queues behind a running turn, where its Enter
        // would steer into it (`resume::EVIDENCE`).
        (Tool::Codex, false) => codex_queue_argv(target.pane),
        (Tool::Codex, true) => codex_submit_argv(target.pane),
        _ => enter_argv(target.pane),
    };
    if target.tool == Tool::Codex {
        attempt.pause(CODEX_PASTE_WINDOW).await;
    }
    attempt.write(submit).await?;
    attempt.confirm(receipts, marked).await
}

fn enter_argv(pane: &str) -> Vec<String> {
    ["action", "write", "-p", pane, "13"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// What the input shows, measured against the payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Held {
    Empty,
    Whole,
    /// A non-empty proper suffix of the payload: clank's own, its start
    /// lost.
    Suffix,
    /// Part of the payload, still arriving.
    Arriving,
    /// Anything else, and why it is not clank's to touch.
    Other(&'static str),
}

impl Held {
    fn what(self) -> &'static str {
        match self {
            Held::Other(what) => what,
            _ => FOREIGN,
        }
    }
}

const FOREIGN: &str = "text clank did not type (a draft was in the way)";

/// One read of the pane.
#[derive(Debug, Clone, Copy)]
struct Look {
    held: Held,
    /// Claude's interrupt hint is on screen.
    busy: bool,
}

struct Attempt<'a, T> {
    term: &'a T,
    target: Target<'a>,
    payload: &'a str,
    deadline: tokio::time::Instant,
    /// A byte may have reached the pane.
    wrote: bool,
    /// Ctrl-C was sent. Claude takes a second Ctrl-C at an empty input as
    /// its exit, and a stale frame can show text in an input already
    /// emptied, so one attempt sends it once at most.
    cleared: bool,
}

impl<T: Terminal> Attempt<'_, T> {
    fn expired(&self) -> bool {
        tokio::time::Instant::now() >= self.deadline
    }

    /// `op`'s output if it starts and finishes before the deadline.
    async fn within<F: std::future::Future>(&self, op: F) -> Option<F::Output> {
        if self.expired() {
            return None;
        }
        let out = tokio::time::timeout_at(self.deadline, op).await.ok()?;
        (!self.expired()).then_some(out)
    }

    /// Sleep `d`, or until the deadline if that comes first.
    async fn pause(&self, d: std::time::Duration) {
        tokio::time::sleep_until(self.deadline.min(tokio::time::Instant::now() + d)).await;
    }

    /// After clank types, its own text may show behind the harness's
    /// paste placeholders, or part-way in; before, only the literal text
    /// can be clank's.
    async fn look(&self, typed: bool) -> Look {
        let tool = self.target.tool;
        let Some(Some(screen)) = self.within(self.term.dump()).await else {
            return Look {
                held: Held::Other("unreadable"),
                busy: false,
            };
        };
        let held = match super::composer::typed(tool, &screen) {
            Some(shown) => measure(&shown, self.payload, typed),
            None if super::composer::composer(tool, &screen) == Composer::Shell => {
                Held::Other("shell mode")
            }
            None => Held::Other("unreadable"),
        };
        Look {
            held,
            busy: tool == Tool::Claude && super::composer::claude_turn_running(&screen),
        }
    }

    fn may_clear(&self, now: &Look) -> bool {
        self.target.tool == Tool::Claude && !now.busy && !self.cleared
    }

    fn fail(&self, what: &str) -> Failed {
        if self.wrote {
            Failed::Indeterminate(format!(
                "its input then held {what}; nothing more was typed"
            ))
        } else {
            Failed::NotSent(format!("its input holds {what}; nothing was typed"))
        }
    }

    fn out_of_time(&self) -> Failed {
        if self.wrote {
            Failed::Indeterminate("the attempt ran out of time; nothing more was typed".into())
        } else {
            Failed::NotSent("the attempt ran out of time; nothing was typed".into())
        }
    }

    async fn write(&mut self, argv: Vec<String>) -> Result<(), Failed> {
        if self.expired() {
            return Err(self.out_of_time());
        }
        match self.within(self.term.write(argv)).await {
            Some(Ok(())) => {
                self.wrote = true;
                Ok(())
            }
            Some(Err(PaneWrite::NotStarted(why))) if !self.wrote => Err(Failed::NotSent(why)),
            Some(Err(e)) => Err(Failed::Indeterminate(e.to_string())),
            None => {
                self.wrote = true;
                Err(Failed::Indeterminate(
                    "a write did not finish within the attempt; it may have landed".into(),
                ))
            }
        }
    }

    async fn interrupt(&mut self) -> Result<(), Failed> {
        if self.expired() {
            return Err(self.out_of_time());
        }
        let key = stop_argv(self.target.pane, self.target.tool);
        match self.within(self.term.write(key)).await {
            Some(Ok(())) => {
                self.wrote = true;
                Ok(())
            }
            Some(Err(e)) => Err(Failed::Interrupt(e.to_string())),
            None => Err(Failed::Interrupt(
                "the interrupt key's write did not finish within the attempt".into(),
            )),
        }
    }

    /// Type the payload once and wait for the input to show it whole. A
    /// suffix, the start lost, is cleared and retyped once while no turn
    /// runs. Text that never shows is not retyped: it may still arrive.
    async fn type_and_see(&mut self) -> Result<(), Failed> {
        let chars = say_argvs(self.target.pane, self.payload)
            .expect("the text is not empty")
            .into_iter()
            .next()
            .expect("the text");
        self.write(chars.clone()).await?;
        let mut typed_at = tokio::time::Instant::now();
        loop {
            let now = self.look(true).await;
            match now.held {
                Held::Whole => return Ok(()),
                Held::Empty | Held::Arriving => {}
                Held::Suffix if self.may_clear(&now) => {
                    self.clear().await?;
                    self.write(chars.clone()).await?;
                    typed_at = tokio::time::Instant::now();
                    continue;
                }
                Held::Suffix => {
                    return Err(Failed::Indeterminate(
                        "its input showed the text with its start lost, and could not be \
                         cleared"
                            .into(),
                    ));
                }
                Held::Other(what) => {
                    return Err(Failed::Indeterminate(format!(
                        "after typing, its input read as {what}"
                    )));
                }
            }
            if self.expired() || tokio::time::Instant::now() >= typed_at + SHOWN_WITHIN {
                return Err(Failed::Indeterminate(
                    "the typed text did not show in its input in time; it may still arrive".into(),
                ));
            }
            self.pause(POLL).await;
        }
    }

    /// Ctrl-C, then wait for the input to read as empty.
    async fn clear(&mut self) -> Result<(), Failed> {
        self.cleared = true;
        self.write(clear_argv(self.target.pane)).await?;
        let sent = tokio::time::Instant::now();
        loop {
            match self.look(false).await.held {
                Held::Empty => return Ok(()),
                Held::Suffix => {}
                other => {
                    return Err(Failed::Indeterminate(format!(
                        "clearing its input left {}",
                        other.what()
                    )));
                }
            }
            if self.expired() || tokio::time::Instant::now() >= sent + SHOWN_WITHIN {
                return Err(Failed::Indeterminate(
                    "its input did not empty after Ctrl-C".into(),
                ));
            }
            self.pause(POLL).await;
        }
    }

    /// Wait for the harness's record of the prompt, appended past the mark.
    ///
    /// Claude only: if no record has come and the input still shows the
    /// whole text a settle delay after the submit key, Enter once more.
    /// That rests on a key contract seen live (Claude Code 2.1.308), not on
    /// the record's absence: Enter on an empty input does nothing, idle,
    /// busy or with a prompt queued. So a second Enter after a stale frame
    /// is a no-op, and after a lost first Enter it sends the text. Codex's
    /// submit keys have no such contract, so it gets one.
    async fn confirm(
        &mut self,
        receipts: &mut impl Receipts,
        marked: Result<(), String>,
    ) -> Result<Delivered, Failed> {
        use super::prompt_record::{attribute, input};
        let tool = self.target.tool;
        let submitted = tokio::time::Instant::now();
        let mut resent = tool != Tool::Claude;
        loop {
            if marked.is_ok() {
                let lines = match self.within(receipts.read()).await {
                    Some(read) => read.map_err(Failed::Indeterminate)?,
                    None => {
                        return Err(Failed::Indeterminate(
                            "submitted, but its record could not be read in time".into(),
                        ));
                    }
                };
                let found = lines
                    .iter()
                    .filter_map(|line| input(tool, line))
                    .find_map(|got| attribute(self.payload, &got));
                if let Some(delivered) = found {
                    return Ok(delivered);
                }
            }
            if !resent && tokio::time::Instant::now() >= submitted + SETTLE {
                resent = true;
                if self.look(true).await.held == Held::Whole {
                    self.write(enter_argv(self.target.pane)).await?;
                }
            }
            if let Err(why) = &marked
                && resent
            {
                return Err(Failed::Indeterminate(format!(
                    "submitted, but nothing can confirm it: {why}"
                )));
            }
            if self.expired() {
                return Err(Failed::Indeterminate(
                    "submitted, but the harness recorded no such prompt in time".into(),
                ));
            }
            self.pause(POLL).await;
        }
    }
}

/// How the input `shown` stands to `payload`, exactly: whitespace inside a
/// row is the draft's own, and a row break stands for a line break in the
/// payload, or for a space or nothing only where the row is too full to
/// have gone on. With `typed`, the harness's paste placeholders stand for
/// some of the payload, and a part of it may still be arriving.
fn measure(shown: &super::composer::Shown, payload: &str, typed: bool) -> Held {
    let Some(last) = shown.rows.len().checked_sub(1) else {
        return Held::Empty;
    };
    let sent = payload.trim_end();
    let mut pattern = Vec::new();
    for (i, row) in shown.rows.iter().enumerate() {
        if typed {
            pattern.extend(pieces(&row.text));
        } else if !row.text.is_empty() {
            pattern.push(Tok::Lit(row.text.clone()));
        }
        if i < last {
            let next = shown.rows[i + 1].text.split(' ').next().unwrap_or_default();
            pattern.push(Tok::Break {
                space: row.end + 1 + next.chars().count() > shown.width,
                join: row.end >= shown.width,
            });
        }
    }
    let suffix = match pattern.first() {
        Some(Tok::Lit(lead)) => starts(sent, lead).any(|i| i > 0 && glob(&pattern, &sent[i..])),
        _ => false,
    };
    if glob(&pattern, sent) {
        Held::Whole
    } else if suffix {
        Held::Suffix
    } else if typed
        && glob(
            &[[Tok::Any(0)].as_slice(), &pattern, &[Tok::Any(0)]].concat(),
            sent,
        )
    {
        Held::Arriving
    } else {
        Held::Other(FOREIGN)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    Lit(String),
    /// At least this many characters, unknown.
    Any(usize),
    /// A row break: a line break, or where the row was full, the space a
    /// wrap took or nothing.
    Break {
        space: bool,
        join: bool,
    },
}

/// `text` as literal text and the paste placeholders Claude and Codex
/// draw in place of a paste: `[Pasted text #N]`, `[Pasted text #N +K
/// lines]`, `[Pasted Content N chars]`.
fn pieces(text: &str) -> Vec<Tok> {
    let mut toks = Vec::new();
    let mut lit = String::new();
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        if let Some(len) = placeholder(rest) {
            if !lit.is_empty() {
                toks.push(Tok::Lit(std::mem::take(&mut lit)));
            }
            toks.push(Tok::Any(1));
            rest = &rest[len..];
        } else {
            lit.push(c);
            rest = &rest[c.len_utf8()..];
        }
    }
    if !lit.is_empty() {
        toks.push(Tok::Lit(lit));
    }
    toks
}

/// The length of the paste placeholder `s` opens with.
fn placeholder(s: &str) -> Option<usize> {
    fn number(s: &str) -> Option<&str> {
        let rest = s.trim_start_matches(|c: char| c.is_ascii_digit());
        (rest.len() < s.len()).then_some(rest)
    }
    let rest = if let Some(r) = s.strip_prefix("[Pasted text #") {
        let r = number(r)?;
        let r = match r.strip_prefix(" +") {
            Some(lines) => number(lines)?.strip_prefix(" lines")?,
            None => r,
        };
        r.strip_prefix(']')?
    } else {
        number(s.strip_prefix("[Pasted Content ")?)?.strip_prefix(" chars]")?
    };
    Some(s.len() - rest.len())
}

/// Every place `lit` starts in `s`, overlapping ones included.
fn starts<'a>(s: &'a str, lit: &'a str) -> impl Iterator<Item = usize> + 'a {
    (0..=s.len()).filter(move |&i| s.is_char_boundary(i) && s[i..].starts_with(lit))
}

/// Whether `toks` spans the whole of `s`.
fn glob(toks: &[Tok], s: &str) -> bool {
    match toks.split_first() {
        None => s.is_empty(),
        Some((Tok::Lit(lit), rest)) => s.strip_prefix(lit.as_str()).is_some_and(|r| glob(rest, r)),
        Some((Tok::Break { space, join }, rest)) => {
            s.strip_prefix('\n').is_some_and(|r| glob(rest, r))
                || (*space && s.strip_prefix(' ').is_some_and(|r| glob(rest, r)))
                || (*join && glob(rest, s))
        }
        Some((Tok::Any(min), rest)) => match rest.split_first() {
            None => s.chars().count() >= *min,
            Some((Tok::Any(more), after)) => {
                glob(&[[Tok::Any(min + more)].as_slice(), after].concat(), s)
            }
            Some((Tok::Lit(lit), _)) => {
                starts(s, lit).any(|i| s[..i].chars().count() >= *min && glob(rest, &s[i..]))
            }
            Some((Tok::Break { .. }, _)) => (0..=s.len())
                .filter(|&i| s.is_char_boundary(i) && s[..i].chars().count() >= *min)
                .any(|i| glob(rest, &s[i..])),
        },
    }
}

/// Why an interrupt-then-submit did not complete, as what the caller may
/// conclude about the terminal.
#[derive(Debug)]
pub(crate) enum Unconfirmed {
    /// Nothing was sent: there was no message.
    Empty,
    /// Nothing was sent: the pane's input was not one clank may type into,
    /// or its lock could not be taken.
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
            tokio::time::sleep(CODEX_PASTE_WINDOW).await;
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
    use crate::cli::prompt_record::{
        self,
        fixtures::{claude_enqueue, claude_user, codex_user},
    };
    use std::cell::{Cell, RefCell};
    use std::time::Duration;

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
            repo: std::path::Path::new("/nowhere"),
            label: Box::leak(Box::new(clank_core::ids::AgentLabel::parse("a").unwrap())),
        }
    }

    const ENTER: &str = "13";
    const ESC: &str = "27";
    const CTRL_C: &str = "3";
    const TAB: &str = "9";
    const KITTY_ENTER: &str = "27 91 49 51 59 49 58 49 117";
    /// What a write of text is recorded as among the keys.
    const TEXT: &str = "text";

    fn key(argv: &[String]) -> String {
        if argv[1] == "write-chars" {
            TEXT.to_string()
        } else {
            argv[4..].join(" ")
        }
    }

    /// `text` in `tool`'s input, its lines as the harness draws them.
    fn input(tool: Tool, text: &str) -> String {
        let glyph = if tool == Tool::Claude { '❯' } else { '›' };
        let width = if tool == Tool::Claude { 32 } else { 40 } - 2;
        let rows: Vec<String> = text
            .split('\n')
            .flat_map(|line| wrapped(line, width))
            .enumerate()
            .map(|(i, row)| match i {
                0 => format!("{glyph} {row}"),
                _ => format!("  {row}"),
            })
            .collect();
        let rows: Vec<&str> = rows.iter().map(String::as_str).collect();
        if tool == Tool::Claude {
            claude(&rows)
        } else {
            codex(&rows)
        }
    }

    /// `line` in rows of `width` the way a terminal input wraps it: at the
    /// last space that fits, the space taken by the break, and a word
    /// longer than a row split where the row ends.
    fn wrapped(line: &str, width: usize) -> Vec<String> {
        let mut rows = vec![String::new()];
        for (i, word) in line.split(' ').enumerate() {
            let row = rows.last_mut().unwrap();
            let len = row.chars().count();
            if i > 0 && len + 1 + word.chars().count() <= width {
                row.push(' ');
            } else if i > 0 {
                rows.push(String::new());
            }
            for c in word.chars() {
                if rows.last().unwrap().chars().count() == width {
                    rows.push(String::new());
                }
                rows.last_mut().unwrap().push(c);
            }
        }
        rows
    }

    /// Claude's screen while a turn runs.
    fn busy(text: &str) -> String {
        format!(
            "✻ Thinking… (3s · esc to interrupt)\n{}",
            input(Tool::Claude, text)
        )
    }

    /// What a scripted pane has been sent, and when.
    struct Seen {
        writes: Vec<(Duration, Vec<String>)>,
        at: Duration,
    }

    impl Seen {
        fn count(&self, k: &str) -> usize {
            self.writes.iter().filter(|(_, a)| key(a) == k).count()
        }

        /// How long since `k` was last sent.
        fn since(&self, k: &str) -> Option<Duration> {
            let (t, _) = self.writes.iter().rev().find(|(_, a)| key(a) == k)?;
            Some(self.at - *t)
        }

        /// The text a well-behaved harness holds in its input now: the
        /// last typed, until a submit key or Ctrl-C takes it.
        fn pending(&self) -> Option<String> {
            let mut held = None;
            for (_, argv) in &self.writes {
                match key(argv).as_str() {
                    TEXT => held = Some(argv.last().unwrap().clone()),
                    ENTER | TAB | KITTY_ENTER | CTRL_C => held = None,
                    _ => {}
                }
            }
            held
        }

        /// What a well-behaved harness has accepted: each text submitted.
        fn accepted(&self) -> Vec<String> {
            let mut held = None;
            let mut accepted = Vec::new();
            for (_, argv) in &self.writes {
                match key(argv).as_str() {
                    TEXT => held = Some(argv.last().unwrap().clone()),
                    ENTER | TAB | KITTY_ENTER => accepted.extend(held.take()),
                    CTRL_C => held = None,
                    _ => {}
                }
            }
            accepted
        }
    }

    type Script<T> = Box<dyn Fn(&Seen) -> T>;

    /// A pane and its agent's binding, played from scripts of what the
    /// screen shows and what the harness records, given what was sent and
    /// when. Records are appended to a real file as they fall due, and
    /// read back through `prompt_record`. A fixture of clank's sequence,
    /// not evidence of what a harness does.
    struct Harness {
        tool: Tool,
        start: tokio::time::Instant,
        written: RefCell<Vec<(Duration, Vec<String>)>>,
        screen: Script<Option<String>>,
        records: Script<Vec<String>>,
        session: Script<String>,
        answer: Box<dyn Fn(usize) -> Result<(), PaneWrite>>,
        shrink: Option<Duration>,
        /// How long each dump and write takes; `None` never answers.
        lag: Option<Duration>,
        /// How long each read of the record takes.
        read_lag: Duration,
        dir: tempfile::TempDir,
        appended: Cell<usize>,
    }

    impl Harness {
        /// A harness that shows what is typed, takes it on its submit key
        /// and records it.
        fn new(tool: Tool) -> Self {
            Harness {
                tool,
                start: tokio::time::Instant::now(),
                written: RefCell::new(Vec::new()),
                screen: Box::new(move |s| Some(input(tool, &s.pending().unwrap_or_default()))),
                records: Box::new(move |s| s.accepted().iter().map(|t| record(tool, t)).collect()),
                session: Box::new(|_| "s1".into()),
                answer: Box::new(|_| Ok(())),
                shrink: None,
                lag: Some(Duration::ZERO),
                read_lag: Duration::ZERO,
                dir: tempfile::tempdir().unwrap(),
                appended: Cell::new(0),
            }
        }

        fn screen(mut self, screen: impl Fn(&Seen) -> Option<String> + 'static) -> Self {
            self.screen = Box::new(screen);
            self
        }

        fn records(mut self, records: impl Fn(&Seen) -> Vec<String> + 'static) -> Self {
            self.records = Box::new(records);
            self
        }

        /// Lines already in the record before the attempt.
        fn before(self, lines: &[String]) -> Self {
            std::fs::write(
                self.path(),
                lines.iter().map(|l| format!("{l}\n")).collect::<String>(),
            )
            .unwrap();
            self
        }

        fn path(&self) -> std::path::PathBuf {
            self.dir.path().join("record.jsonl")
        }

        fn seen(&self) -> Seen {
            Seen {
                writes: self.written.borrow().clone(),
                at: self.start.elapsed(),
            }
        }

        fn keys(&self) -> Vec<String> {
            self.written.borrow().iter().map(|(_, a)| key(a)).collect()
        }
    }

    fn record(tool: Tool, text: &str) -> String {
        match tool {
            Tool::Codex => codex_user(text),
            _ => claude_user(text),
        }
    }

    impl Terminal for Harness {
        async fn dump(&self) -> Option<String> {
            self.lag().await;
            (self.screen)(&self.seen())
        }
        async fn write(&self, argv: Vec<String>) -> Result<(), PaneWrite> {
            let n = self.written.borrow().len();
            self.written.borrow_mut().push((self.start.elapsed(), argv));
            self.lag().await;
            (self.answer)(n)
        }
    }

    impl Harness {
        /// How long each terminal operation takes: `None` never answers.
        async fn lag(&self) {
            match self.lag {
                Some(lag) => tokio::time::sleep(lag).await,
                None => std::future::pending().await,
            }
        }

        /// Append the records now due, as the harness would have written
        /// them by now.
        fn sync(&self) -> Seen {
            use std::io::Write;
            let seen = self.seen();
            let due = (self.records)(&seen);
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.path())
                .unwrap();
            for line in due.iter().skip(self.appended.get()) {
                writeln!(file, "{line}").unwrap();
            }
            self.appended.set(self.appended.get().max(due.len()));
            if self.shrink.is_some_and(|at| seen.at >= at) {
                file.set_len(0).unwrap();
            }
            seen
        }
    }

    impl prompt_record::Binding for Harness {
        fn key(&self) -> Result<String, String> {
            Ok((self.session)(&self.sync()))
        }
        fn record(&self) -> Result<(String, Option<std::path::PathBuf>), String> {
            Ok(((self.session)(&self.sync()), Some(self.path())))
        }
    }

    /// `h`'s record, read through the real reader on the test's own
    /// thread, each read taking `h.read_lag`.
    struct Reading<'a> {
        h: &'a Harness,
        tail: Option<prompt_record::Tail>,
    }

    impl Receipts for Reading<'_> {
        async fn mark(&mut self) -> Result<(), String> {
            self.tail = Some(prompt_record::Tail::mark(self.h)?);
            Ok(())
        }
        async fn read(&mut self) -> Result<Vec<String>, String> {
            tokio::time::sleep(self.h.read_lag).await;
            self.tail.as_mut().expect("marked").read(self.h)
        }
    }

    fn reading(h: &Harness) -> Reading<'_> {
        Reading { h, tail: None }
    }

    /// The writer paths that type a prompt into a Claude or Codex pane.
    #[derive(Debug, Clone, Copy)]
    enum Path {
        /// Recovery's typed sends and check-ins.
        Submit,
        /// Event wakes.
        Wake,
        /// `clank nudge`.
        Nudge,
    }

    const PATHS: [Path; 3] = [Path::Submit, Path::Wake, Path::Nudge];

    /// What a path's result lets its caller conclude.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Got {
        Delivered(Delivered),
        NotSent,
        Indeterminate,
    }

    impl Path {
        async fn send(self, h: &Harness, text: &str) -> Got {
            let (target, text) = (target(h.tool), Admitted::new(text));
            let unconfirmed = |r: Result<Delivered, Unconfirmed>| match r {
                Ok(d) => Got::Delivered(d),
                Err(Unconfirmed::Refused(_) | Unconfirmed::Empty) => Got::NotSent,
                Err(Unconfirmed::Interrupt(_) | Unconfirmed::Submit(_)) => Got::Indeterminate,
            };
            match self {
                Path::Submit => match submit_on(h, &mut reading(h), target, &text).await {
                    Ok(d) => Got::Delivered(d),
                    Err(PaneWrite::NotStarted(_)) => Got::NotSent,
                    Err(PaneWrite::Unknown(_)) => Got::Indeterminate,
                },
                Path::Wake => {
                    unconfirmed(interrupt_and_submit_on(h, &mut reading(h), target, &text).await)
                }
                Path::Nudge => {
                    unconfirmed(interrupt_then_type(h, &mut reading(h), target, &text).await)
                }
            }
        }

        /// The keys this path sends before any text: the interrupt.
        fn lead(self) -> Vec<String> {
            match self {
                Path::Submit => Vec::new(),
                Path::Wake | Path::Nudge => vec![ESC.to_string()],
            }
        }

        /// `keys` after this path's interrupt.
        fn then(self, keys: &[&str]) -> Vec<String> {
            let mut all = self.lead();
            all.extend(keys.iter().map(|k| k.to_string()));
            all
        }

        fn submit_key(self, tool: Tool) -> &'static str {
            match (tool, self) {
                (Tool::Codex, Path::Submit) => TAB,
                (Tool::Codex, _) => KITTY_ENTER,
                _ => ENTER,
            }
        }
    }

    const PAYLOAD: &str = "Experiment rules: stay inside the scratch dir";

    /// Interrupt, then the message, then its own submit key, for every
    /// provider whose input can be read, and the harness's record of it
    /// confirms it whole.
    #[tokio::test(start_paused = true)]
    async fn every_path_types_once_submits_once_and_is_confirmed_by_the_record() {
        for tool in [Tool::Claude, Tool::Codex] {
            for path in PATHS {
                let h = Harness::new(tool);
                assert_eq!(
                    path.send(&h, PAYLOAD).await,
                    Got::Delivered(Delivered::Whole),
                    "{tool:?} {path:?}"
                );
                assert_eq!(
                    h.keys(),
                    path.then(&[TEXT, path.submit_key(tool)]),
                    "{tool:?} {path:?}"
                );
            }
        }
    }

    /// A frame still showing the empty input after the text was typed is
    /// waited past, not typed into again.
    #[tokio::test(start_paused = true)]
    async fn a_stale_empty_input_is_waited_past() {
        for path in PATHS {
            let h = Harness::new(Tool::Claude).screen(|s| {
                let late = s
                    .since(TEXT)
                    .is_some_and(|t| t < Duration::from_millis(500));
                let shown = s.pending().filter(|_| !late).unwrap_or_default();
                Some(input(Tool::Claude, &shown))
            });
            assert_eq!(
                path.send(&h, PAYLOAD).await,
                Got::Delivered(Delivered::Whole)
            );
            assert_eq!(h.keys(), path.then(&[TEXT, ENTER]), "{path:?}");
        }
    }

    /// Text that shows only after its budget may still arrive, so it is
    /// neither submitted nor retyped.
    #[tokio::test(start_paused = true)]
    async fn text_shown_too_late_is_not_retyped() {
        for path in PATHS {
            let h = Harness::new(Tool::Claude).screen(|s| {
                let late = s.since(TEXT).is_some_and(|t| t < Duration::from_secs(5));
                let shown = s.pending().filter(|_| !late).unwrap_or_default();
                Some(input(Tool::Claude, &shown))
            });
            assert_eq!(path.send(&h, PAYLOAD).await, Got::Indeterminate, "{path:?}");
            assert_eq!(h.keys(), path.then(&[TEXT]), "{path:?}");
        }
    }

    /// The first character lost on the way in, while idle: the rest is
    /// clank's own, so it is cleared and the text typed once more.
    #[tokio::test(start_paused = true)]
    async fn a_lost_first_character_while_idle_is_cleared_and_retyped() {
        for path in PATHS {
            let h = Harness::new(Tool::Claude).screen(|s| {
                let shown = match s.pending() {
                    Some(t) if s.count(TEXT) == 1 => t[1..].to_string(),
                    other => other.unwrap_or_default(),
                };
                Some(input(Tool::Claude, &shown))
            });
            assert_eq!(
                path.send(&h, PAYLOAD).await,
                Got::Delivered(Delivered::Whole)
            );
            assert_eq!(
                h.keys(),
                path.then(&[TEXT, CTRL_C, TEXT, ENTER]),
                "{path:?}"
            );
        }
    }

    /// The same while a turn runs: Ctrl-C would interrupt it, so nothing
    /// is cleared and the outcome is indeterminate.
    #[tokio::test(start_paused = true)]
    async fn a_lost_first_character_while_a_turn_runs_sends_no_ctrl_c() {
        let h = Harness::new(Tool::Claude).screen(|s| {
            Some(busy(
                &s.pending().map(|t| t[1..].to_string()).unwrap_or_default(),
            ))
        });
        assert_eq!(Path::Submit.send(&h, PAYLOAD).await, Got::Indeterminate);
        assert_eq!(h.keys(), [TEXT]);
    }

    /// A record agreeing on the first line and not a later one is someone
    /// else's prompt: the input emptying does not confirm this one, and it
    /// is not resent.
    #[tokio::test(start_paused = true)]
    async fn a_record_matching_only_the_first_line_confirms_nothing() {
        let payload = "first line\nsecond line";
        for path in PATHS {
            let h = Harness::new(Tool::Claude).records(|s| {
                s.accepted()
                    .iter()
                    .map(|_| claude_user("first line\nother"))
                    .collect()
            });
            assert_eq!(path.send(&h, payload).await, Got::Indeterminate, "{path:?}");
            assert_eq!(h.keys(), path.then(&[TEXT, ENTER]), "{path:?}");
        }
    }

    /// The first Enter taken while the frame still shows the text, and its
    /// record written only after the settle delay: one more Enter at most,
    /// and one confirmation.
    #[tokio::test(start_paused = true)]
    async fn a_late_record_behind_a_stale_frame_takes_at_most_one_more_enter() {
        let h = Harness::new(Tool::Claude)
            .screen(|s| {
                let stale = s
                    .since(ENTER)
                    .is_some_and(|t| t < Duration::from_millis(1500));
                let shown = match s.writes.iter().any(|(_, a)| key(a) == ENTER) {
                    true if stale => PAYLOAD.to_string(),
                    true => String::new(),
                    false => s.pending().unwrap_or_default(),
                };
                Some(input(Tool::Claude, &shown))
            })
            .records(|s| {
                let first = s.writes.iter().find(|(_, a)| key(a) == ENTER);
                first
                    .filter(|(t, _)| s.at >= *t + Duration::from_millis(1200))
                    .map(|_| vec![claude_user(PAYLOAD)])
                    .unwrap_or_default()
            });
        assert_eq!(
            Path::Submit.send(&h, PAYLOAD).await,
            Got::Delivered(Delivered::Whole)
        );
        assert_eq!(h.keys(), [TEXT, ENTER, ENTER]);
    }

    /// The Enter lost, the whole text still in the input: Claude gets one
    /// more Enter; Codex, on its Tab and its kitty Enter alike, none.
    #[tokio::test(start_paused = true)]
    async fn a_lost_submit_key_is_pressed_once_more_on_claude_only() {
        for tool in [Tool::Claude, Tool::Codex] {
            for path in PATHS {
                let h = Harness::new(tool)
                    .screen(move |s| {
                        let typed = s.writes.iter().any(|(_, a)| key(a) == TEXT);
                        Some(input(tool, if typed { PAYLOAD } else { "" }))
                    })
                    .records(|_| Vec::new());
                assert_eq!(path.send(&h, PAYLOAD).await, Got::Indeterminate);
                let submit = path.submit_key(tool);
                let expected: &[&str] = match tool {
                    Tool::Claude => &[TEXT, submit, ENTER],
                    _ => &[TEXT, submit],
                };
                assert_eq!(h.keys(), path.then(expected), "{tool:?} {path:?}");
            }
        }
    }

    /// The same prompt recorded before the attempt began is not this
    /// attempt's.
    #[tokio::test(start_paused = true)]
    async fn an_identical_record_from_before_the_attempt_confirms_nothing() {
        for path in PATHS {
            let h = Harness::new(Tool::Claude)
                .before(&[claude_user(PAYLOAD), claude_enqueue(PAYLOAD)])
                .records(|_| Vec::new());
            assert_eq!(path.send(&h, PAYLOAD).await, Got::Indeterminate, "{path:?}");
            assert_eq!(h.keys(), path.then(&[TEXT, ENTER]), "{path:?}");
        }
    }

    /// Another prompt recorded during the attempt, a person's, is passed
    /// over and the wait goes on to this one's; alone, it confirms
    /// nothing.
    #[tokio::test(start_paused = true)]
    async fn another_prompt_recorded_meanwhile_is_skipped() {
        let h = Harness::new(Tool::Claude).records(|s| {
            let mut lines = vec![claude_user("a person's own prompt")];
            if s.since(ENTER)
                .is_some_and(|t| t >= Duration::from_millis(500))
            {
                lines.push(claude_user(PAYLOAD));
            }
            lines
        });
        assert_eq!(
            Path::Submit.send(&h, PAYLOAD).await,
            Got::Delivered(Delivered::Whole)
        );
        assert_eq!(h.keys(), [TEXT, ENTER]);
        let alone = Harness::new(Tool::Claude).records(|s| {
            s.accepted()
                .iter()
                .map(|_| claude_user("a person's own prompt"))
                .collect()
        });
        assert_eq!(Path::Submit.send(&alone, PAYLOAD).await, Got::Indeterminate);
    }

    /// Hook feedback and a compaction summary are not input, even holding
    /// the very text.
    #[tokio::test(start_paused = true)]
    async fn hook_feedback_and_a_compaction_summary_are_skipped() {
        let h = Harness::new(Tool::Claude).records(|s| {
            if s.count(ENTER) == 0 {
                return Vec::new();
            }
            ["isMeta", "isCompactSummary"]
                .map(|flag| {
                    format!(
                        r#"{{"type":"user","{flag}":true,"message":{{"content":"{PAYLOAD}"}}}}"#
                    )
                })
                .to_vec()
        });
        assert_eq!(Path::Submit.send(&h, PAYLOAD).await, Got::Indeterminate);
    }

    /// Claude's enqueue, written when it takes the prompt busy, and the
    /// user turn it becomes are one acceptance.
    #[tokio::test(start_paused = true)]
    async fn an_enqueue_and_its_user_turn_are_one_acceptance() {
        let h = Harness::new(Tool::Claude).records(|s| {
            s.accepted()
                .iter()
                .flat_map(|t| [claude_enqueue(t), claude_user(t)])
                .collect()
        });
        assert_eq!(
            Path::Submit.send(&h, PAYLOAD).await,
            Got::Delivered(Delivered::Whole)
        );
        assert_eq!(h.keys(), [TEXT, ENTER]);
    }

    /// A record of the text with its start lost is this attempt's, altered.
    #[tokio::test(start_paused = true)]
    async fn a_suffix_recorded_is_delivered_altered() {
        for path in PATHS {
            let h = Harness::new(Tool::Claude)
                .records(|s| s.accepted().iter().map(|t| claude_user(&t[1..])).collect());
            assert_eq!(
                path.send(&h, PAYLOAD).await,
                Got::Delivered(Delivered::Altered),
                "{path:?}"
            );
            assert_eq!(h.keys(), path.then(&[TEXT, ENTER]), "{path:?}");
        }
    }

    /// A record that shrinks, or a binding that changes, mid-attempt: what
    /// follows cannot be attributed to it.
    #[tokio::test(start_paused = true)]
    async fn a_shrunk_record_or_a_new_binding_is_indeterminate() {
        let mut shrunk = Harness::new(Tool::Claude)
            .before(&[claude_user("earlier")])
            .records(|_| Vec::new());
        shrunk.shrink = Some(Duration::from_millis(300));
        let tried = submit_on(
            &shrunk,
            &mut reading(&shrunk),
            target(Tool::Claude),
            &Admitted::new(PAYLOAD),
        )
        .await;
        assert!(
            matches!(&tried, Err(PaneWrite::Unknown(why)) if why.contains("shrank")),
            "{tried:?}"
        );
        let mut rebound = Harness::new(Tool::Claude);
        rebound.session = Box::new(|s| if s.count(TEXT) > 0 { "s2" } else { "s1" }.to_string());
        assert_eq!(
            Path::Submit.send(&rebound, PAYLOAD).await,
            Got::Indeterminate
        );
    }

    /// The whole payload already in the input is sent with the submit key,
    /// not typed again.
    #[tokio::test(start_paused = true)]
    async fn a_draft_of_the_whole_payload_gets_only_the_submit_key() {
        for path in PATHS {
            let h = Harness::new(Tool::Claude)
                .screen(|s| {
                    Some(input(
                        Tool::Claude,
                        if s.count(ENTER) > 0 { "" } else { PAYLOAD },
                    ))
                })
                .records(|s| match s.count(ENTER) {
                    0 => Vec::new(),
                    _ => vec![claude_user(PAYLOAD)],
                });
            assert_eq!(
                path.send(&h, PAYLOAD).await,
                Got::Delivered(Delivered::Whole)
            );
            assert_eq!(h.keys(), path.then(&[ENTER]), "{path:?}");
        }
    }

    /// A proper suffix of the payload in an idle input, short or long, is
    /// clank's own leftover: cleared, then the text typed.
    #[tokio::test(start_paused = true)]
    async fn a_suffix_draft_while_idle_is_cleared_then_typed() {
        for cut in [PAYLOAD.len() - 3, 1] {
            for path in PATHS {
                let h = Harness::new(Tool::Claude).screen(move |s| {
                    let shown = match s.count(CTRL_C) {
                        0 => PAYLOAD[cut..].to_string(),
                        _ => s.pending().unwrap_or_default(),
                    };
                    Some(input(Tool::Claude, &shown))
                });
                assert_eq!(
                    path.send(&h, PAYLOAD).await,
                    Got::Delivered(Delivered::Whole)
                );
                assert_eq!(
                    h.keys(),
                    path.then(&[CTRL_C, TEXT, ENTER]),
                    "{cut} {path:?}"
                );
            }
        }
    }

    /// A draft that is not clank's own text is refused untouched, before
    /// any key: the payload with more after it, a suffix with a character
    /// changed, a person's own words, and the payload with a space
    /// dropped, doubled, moved or turned into a line break.
    #[tokio::test(start_paused = true)]
    async fn a_foreign_draft_is_refused_with_no_key_sent() {
        let changed = format!("q{}", &PAYLOAD[2..]);
        let more = format!("{PAYLOAD} and more");
        let dropped = PAYLOAD.replacen(' ', "", 1);
        let doubled = PAYLOAD.replacen(' ', "  ", 1);
        let moved = PAYLOAD.replacen("stay inside", "stayi nside", 1);
        let broken = PAYLOAD.replacen(' ', "\n", 1);
        let drafts = [&more, &changed, &dropped, &doubled, &moved, &broken];
        for draft in drafts.iter().map(|d| d.as_str()).chain(["half a thought"]) {
            for tool in [Tool::Claude, Tool::Codex] {
                for path in PATHS {
                    let draft = draft.to_string();
                    let h = Harness::new(tool).screen(move |_| Some(input(tool, &draft)));
                    assert_eq!(
                        path.send(&h, PAYLOAD).await,
                        Got::NotSent,
                        "{tool:?} {path:?}"
                    );
                    assert!(h.keys().is_empty(), "{tool:?} {path:?}");
                }
            }
        }
    }

    /// How long `path` took, and what it came to, bounded well past the
    /// attempt so a send that outlives it fails rather than hangs.
    async fn timed(path: Path, h: &Harness) -> (Duration, Option<Got>) {
        let start = tokio::time::Instant::now();
        let got = tokio::time::timeout(ATTEMPT * 4, path.send(h, PAYLOAD))
            .await
            .ok();
        (start.elapsed(), got)
    }

    /// A terminal taking three seconds a read and a write: the attempt ends
    /// by its deadline, never sends its submit key, and claims nothing.
    #[tokio::test(start_paused = true)]
    async fn a_slow_terminal_never_gets_a_submit_key_past_the_deadline() {
        for tool in [Tool::Claude, Tool::Codex] {
            for path in PATHS {
                let mut h = Harness::new(tool);
                h.lag = Some(Duration::from_secs(3));
                let (took, got) = timed(path, &h).await;
                assert!(took <= ATTEMPT, "{tool:?} {path:?}: {took:?}");
                assert_eq!(got, Some(Got::Indeterminate), "{tool:?} {path:?}");
                let submit = path.submit_key(tool);
                assert!(!h.keys().contains(&submit.to_string()), "{tool:?} {path:?}");
                assert!(
                    h.written.borrow().iter().all(|(at, _)| *at < ATTEMPT),
                    "{tool:?} {path:?}"
                );
            }
        }
    }

    /// A terminal that never answers: the attempt ends at its deadline,
    /// having typed nothing.
    #[tokio::test(start_paused = true)]
    async fn an_unanswering_terminal_ends_the_attempt_at_its_deadline() {
        for path in PATHS {
            let mut h = Harness::new(Tool::Claude);
            h.lag = None;
            let (took, got) = timed(path, &h).await;
            assert_eq!(took, ATTEMPT, "{path:?}");
            assert_eq!(got, Some(Got::NotSent), "{path:?}");
            assert!(h.keys().is_empty(), "{path:?}");
        }
    }

    /// A read of the record that finishes only at the deadline confirms
    /// nothing, though it holds the prompt.
    #[tokio::test(start_paused = true)]
    async fn a_record_read_ending_at_the_deadline_confirms_nothing() {
        let mut h = Harness::new(Tool::Claude).records(|s| match s.at >= Duration::from_secs(5) {
            true => s.accepted().iter().map(|t| claude_user(t)).collect(),
            false => Vec::new(),
        });
        h.read_lag = (ATTEMPT - POLL) / 2;
        let (took, got) = timed(Path::Submit, &h).await;
        assert_eq!(took, ATTEMPT);
        assert_eq!(got, Some(Got::Indeterminate));
        assert_eq!(h.keys(), [TEXT, ENTER]);
    }

    /// An input that cannot be read after the text was typed is
    /// indeterminate: the text may be there.
    #[tokio::test(start_paused = true)]
    async fn an_input_unreadable_mid_attempt_is_indeterminate() {
        for path in PATHS {
            let h = Harness::new(Tool::Claude)
                .screen(|s| (s.count(TEXT) == 0).then(|| input(Tool::Claude, "")));
            assert_eq!(path.send(&h, PAYLOAD).await, Got::Indeterminate, "{path:?}");
            assert_eq!(h.keys(), path.then(&[TEXT]), "{path:?}");
        }
    }

    /// Without a binding to read, nothing can confirm the prompt: Claude's
    /// second Enter still rests on its key contract, and the outcome is
    /// indeterminate.
    #[tokio::test(start_paused = true)]
    async fn without_a_record_nothing_is_confirmed() {
        struct Unbound;
        impl Receipts for Unbound {
            async fn mark(&mut self) -> Result<(), String> {
                Err("it has no bound session".into())
            }
            async fn read(&mut self) -> Result<Vec<String>, String> {
                panic!("an unmarked record is not read")
            }
        }
        let h = Harness::new(Tool::Claude);
        let tried = submit_on(
            &h,
            &mut Unbound,
            target(Tool::Claude),
            &Admitted::new(PAYLOAD),
        )
        .await;
        assert!(
            matches!(&tried, Err(PaneWrite::Unknown(why)) if why.contains("no bound session")),
            "{tried:?}"
        );
        assert_eq!(h.keys(), [TEXT, ENTER]);
    }

    /// `text` as `tool`'s input shows it.
    fn shown(tool: Tool, text: &str) -> crate::cli::composer::Shown {
        crate::cli::composer::typed(tool, &input(tool, text)).unwrap()
    }

    /// The harness's paste placeholders stand for some of the payload once
    /// clank has typed it, never before; a part of it is still arriving.
    #[test]
    fn placeholders_stand_for_typed_text_only() {
        let sent = "aaa bbb ccc ddd";
        let of = |text: &str, typed| measure(&shown(Tool::Claude, text), sent, typed);
        assert_eq!(of("[Pasted text #1 +3 lines] ddd", true), Held::Whole);
        assert_eq!(of("[Pasted Content 12 chars]", true), Held::Whole);
        assert_eq!(of("aa[Pasted text #2] ccc", true), Held::Arriving);
        assert_eq!(of("[Pasted text #1]", false), Held::Other(FOREIGN));
        assert_eq!(of("aa bbb ccc ddd", true), Held::Suffix);
        assert_eq!(of("aaa bb", true), Held::Arriving);
        assert_eq!(of("aaa bb", false), Held::Other(FOREIGN));
        assert_eq!(of("[Pasted text #1] eee", true), Held::Other(FOREIGN));
    }

    /// Whitespace inside a row is the draft's own, and a row break is a
    /// space or nothing only where the row was too full to go on: a draft
    /// with its spaces moved, doubled, dropped, or broken into lines is not
    /// the payload, while the payload wrapped is.
    #[test]
    fn whitespace_is_measured_exactly() {
        let sent = "run tests now";
        for tool in [Tool::Claude, Tool::Codex] {
            let of = |text: &str| measure(&shown(tool, text), sent, false);
            assert_eq!(of(sent), Held::Whole, "{tool:?}");
            assert_eq!(of("un tests now"), Held::Suffix, "{tool:?}");
            for other in [
                "runtests now",
                "run  tests now",
                "ru ntests now",
                "run tests\nnow",
                "run\ntests now",
                "untests now",
            ] {
                assert_eq!(of(other), Held::Other(FOREIGN), "{tool:?} {other:?}");
            }
        }
        let long = "a line long enough that the input must wrap it over its rows";
        let word = "x".repeat(70);
        for tool in [Tool::Claude, Tool::Codex] {
            assert!(shown(tool, long).rows.len() > 1, "{tool:?}");
            assert_eq!(measure(&shown(tool, long), long, false), Held::Whole);
            assert_eq!(measure(&shown(tool, &word), &word, false), Held::Whole);
            let squeezed = long.replacen(' ', "", 1);
            assert_eq!(
                measure(&shown(tool, &squeezed), long, false),
                Held::Other(FOREIGN),
                "{tool:?}"
            );
        }
    }

    /// An interrupt whose delivery is UNKNOWN is not called safe to
    /// repeat, and says that no message was submitted.
    ///
    /// The transport reports a timeout after it may already have written
    /// the key as "acceptance is unknown". The earlier wording wrapped that
    /// in "it is safe to retry", producing "safe to retry: … acceptance is
    /// unknown" — and a retry can send a second interrupt into a
    /// cancellation already under way, which for Grok escalates toward quit
    /// (codex on 307bbb3).
    #[test]
    fn an_interrupt_of_unknown_delivery_promises_nothing() {
        let said = format!(
            "{:#}",
            Unconfirmed::Interrupt(anyhow::anyhow!(
                "terminal input transport timed out after 3s; acceptance is unknown; stderr: "
            ))
        );
        assert!(!said.contains("safe to retry"), "{said}");
        assert!(said.contains("no message was submitted"), "{said}");
        assert!(said.contains("do not repeat it blindly"), "{said}");
        assert!(said.contains("acceptance is unknown"), "{said}");
    }

    /// `clank nudge` into a harness whose input cannot be read is blunt:
    /// its interrupt key, the text and Enter, with only the transport to
    /// answer for them.
    #[tokio::test]
    async fn a_nudge_types_unread_only_where_the_input_cannot_be_read() {
        for (tool, interrupt) in [(Tool::OpenCode, ESC), (Tool::Grok, "3")] {
            let h = Harness::new(tool).screen(|_| None);
            assert_eq!(
                Path::Nudge.send(&h, "hello").await,
                Got::Delivered(Delivered::Unread),
                "{tool:?}"
            );
            assert_eq!(h.keys(), [interrupt, TEXT, ENTER], "{tool:?}");
        }
    }

    /// A failure writing the INTERRUPT is reported as one, and nothing
    /// follows it: an unconfirmed MESSAGE would invite a resend.
    #[tokio::test]
    async fn a_failed_interrupt_write_is_an_interrupt_failure_and_types_nothing() {
        for path in [Path::Wake, Path::Nudge] {
            let mut h = Harness::new(Tool::Claude);
            h.answer = Box::new(|_| {
                Err(PaneWrite::Unknown(
                    "terminal input transport timed out; acceptance is unknown".into(),
                ))
            });
            let text = Admitted::new("hello");
            let error = match path {
                Path::Wake => {
                    interrupt_and_submit_on(&h, &mut reading(&h), target(Tool::Claude), &text).await
                }
                _ => interrupt_then_type(&h, &mut reading(&h), target(Tool::Claude), &text).await,
            }
            .unwrap_err();
            assert_eq!(h.keys(), [ESC], "{path:?}: only the interrupt");
            assert!(matches!(error, Unconfirmed::Interrupt(_)), "{path:?}");
        }
    }

    /// A send that never started wrote nothing; one that failed after
    /// anything was written may have typed it.
    #[tokio::test(start_paused = true)]
    async fn a_failed_submit_says_whether_anything_was_written() {
        let mut first = Harness::new(Tool::Claude);
        first.answer = Box::new(|_| Err(PaneWrite::NotStarted("no zellij".into())));
        assert_eq!(Path::Submit.send(&first, "hi").await, Got::NotSent);
        let mut second = Harness::new(Tool::Claude);
        second.answer = Box::new(|n| match n {
            0 => Ok(()),
            _ => Err(PaneWrite::NotStarted("gone".into())),
        });
        assert_eq!(
            Path::Submit.send(&second, "hi").await,
            Got::Indeterminate,
            "the text was typed"
        );
    }

    /// An empty message sends no key at all: interrupting an agent to tell
    /// it nothing is worse than not nudging.
    #[tokio::test]
    async fn an_empty_message_sends_nothing() {
        for path in PATHS {
            let h = Harness::new(Tool::Claude);
            assert_eq!(path.send(&h, "").await, Got::NotSent, "{path:?}");
            assert!(h.keys().is_empty(), "{path:?}");
        }
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
        for (tool, path) in [(Tool::Claude, Path::Wake), (Tool::Codex, Path::Submit)] {
            let h = Harness::new(tool);
            assert_eq!(path.send(&h, raw).await, Got::Delivered(Delivered::Whole));
            let typed = h
                .written
                .borrow()
                .iter()
                .find(|(_, a)| key(a) == TEXT)
                .cloned();
            assert_eq!(typed.unwrap().1.last().unwrap(), expect, "{tool:?}");
        }
        let say = fake(idle(Tool::Claude));
        say_on(&say, "7", Tool::Claude, &Admitted::new(raw))
            .await
            .unwrap();
        assert_eq!(say.written.borrow()[0].last().unwrap(), expect);
    }

    /// Claude and Codex: every path class is refused, writing nothing, on a
    /// shell composer, an unreadable dump, a dump that never came, and for
    /// Codex a `!` draft. Only the web bridge's `say` still types after a
    /// person's draft.
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
            for path in PATHS {
                let shown = screen.clone();
                let h = Harness::new(tool).screen(move |_| shown.clone());
                assert_eq!(
                    path.send(&h, "x").await,
                    Got::NotSent,
                    "{tool:?} {screen:?}"
                );
                assert!(h.keys().is_empty(), "{tool:?} {path:?}");
            }
            let say = fake(screen.clone());
            assert!(say_on(&say, "7", tool, &Admitted::new("x")).await.is_err());
            assert!(say.written.borrow().is_empty());
        }
        for (tool, screen) in [
            (Tool::Claude, claude(&["❯ half a thought"])),
            (Tool::Codex, codex(&["› half a thought"])),
        ] {
            let say = fake(Some(screen));
            say_on(&say, "7", tool, &Admitted::new("x")).await.unwrap();
            assert_eq!(say.written.borrow().len(), 2, "{tool:?}");
        }
    }

    /// Grok and OpenCode: every automatic path is refused for its tool,
    /// writing nothing and never asking for a dump it could not read.
    #[tokio::test(start_paused = true)]
    async fn grok_and_opencode_are_never_typed_into() {
        let unreadable = |why: &str| why.contains("input cannot be read");
        for tool in [Tool::Grok, Tool::OpenCode] {
            let h = Harness::new(tool).screen(|_| panic!("its input cannot be read"));
            let text = Admitted::new("x");
            let tried = interrupt_and_submit_on(&h, &mut reading(&h), target(tool), &text).await;
            assert!(
                matches!(&tried, Err(Unconfirmed::Refused(why)) if unreadable(why)),
                "{tool:?}: {tried:?}"
            );
            let tried = submit_on(&h, &mut reading(&h), target(tool), &text).await;
            assert!(
                matches!(&tried, Err(PaneWrite::NotStarted(why)) if unreadable(why)),
                "{tool:?}: {tried:?}"
            );
            let term = fake(None);
            assert!(say_on(&term, "7", tool, &text).await.is_err());
            assert!(
                h.keys().is_empty() && term.written.borrow().is_empty(),
                "{tool:?}"
            );
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
                            say_on(&term, "7", Tool::Claude, &Admitted::new("x"))
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
                let label = clank_core::ids::AgentLabel::parse("a").unwrap();
                let target = Target {
                    session: &s,
                    pane: &p,
                    tool: Tool::Claude,
                    repo: std::path::Path::new("/nowhere"),
                    label: &label,
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
