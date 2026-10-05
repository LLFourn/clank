//! Provider-aware terminal input shared by nudges, events and remote controls.
//! Callers resolve target identity; accepted terminal bytes do not prove a stopped turn.

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

pub(crate) fn stop_pane(
    session: &str,
    pane: &str,
    tool: clank_core::vocab::Tool,
) -> anyhow::Result<()> {
    let argv = stop_argv(pane, tool);
    super::open_zellij::write_pane_action(session, pane, &argv)
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
pub async fn interrupt_and_submit_async(
    session: &str,
    pane: &str,
    tool: clank_core::Tool,
    text: &str,
) -> anyhow::Result<()> {
    interrupt_and_submit_with(pane, tool, text, |argv| async move {
        super::open_zellij::write_pane_action_async(session, pane, &argv).await
    })
    .await
}

/// [`interrupt_and_submit_async`] with the terminal write supplied.
///
/// So the REAL bytes, per provider, can be recorded in order without a
/// terminal. The order is also asserted on [`interrupt_then_submit`]
/// alone, but that cannot see which operation is handed in as which: a
/// production path that passed the submit as the interrupt would satisfy
/// it. This is the seam that sees the keys themselves.
async fn interrupt_and_submit_with<W, F>(
    pane: &str,
    tool: clank_core::Tool,
    text: &str,
    write: W,
) -> anyhow::Result<()>
where
    W: Fn(Vec<String>) -> F,
    F: std::future::Future<Output = anyhow::Result<()>>,
{
    interrupt_then_submit(
        || write(stop_argv(pane, tool)),
        || async {
            let Some(argvs) = say_argvs(pane, text) else {
                return Ok(());
            };
            for (index, mut argv) in argvs.into_iter().enumerate() {
                if index == 1 && tool == clank_core::Tool::Codex {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    argv = codex_submit_argv(pane);
                }
                write(argv).await?;
            }
            Ok(())
        },
        text.is_empty(),
    )
    .await
}

/// A fresh prompt is the handoff across native cancellation hooks. User content
/// stays in its durable inbox: this only asks the target to read pending work.
/// Role is deliberately resolved by Clank when the target reads, never captured
/// in terminal text before interruption. No receipt or acknowledgement is forged.
pub(crate) const EVENT_WAKE: &str = "Clank has pending events for this session. Run `clank events list`, read the pending events with `clank events read`, and handle them before acknowledging. Follow any required-reply contract. Your current role and work are determined by Clank.";

pub(crate) async fn wake_for_events_async(
    session: &str,
    pane: &str,
    tool: clank_core::Tool,
) -> anyhow::Result<()> {
    interrupt_and_submit_async(session, pane, tool, EVENT_WAKE).await
}

/// The order, separated from the terminal so it can be asserted.
///
/// Nothing at all happens for an empty message: interrupting an agent to
/// tell it nothing is the one outcome worse than the bug this order fixes.
async fn interrupt_then_submit<I, S>(
    interrupt: impl FnOnce() -> I,
    submit: impl FnOnce() -> S,
    empty: bool,
) -> anyhow::Result<()>
where
    I: std::future::Future<Output = anyhow::Result<()>>,
    S: std::future::Future<Output = anyhow::Result<()>>,
{
    use anyhow::Context;
    anyhow::ensure!(!empty, "an empty nudge must not interrupt the agent");
    interrupt().await.context(
        "the interrupt key's delivery was not confirmed, so no message was submitted; it may \
         still have interrupted the turn, so do not repeat it blindly — a second interrupt \
         during a cancellation can escalate (for Grok, toward quit)",
    )?;
    submit().await.context(
        "the interrupt was delivered, but the message was not confirmed (some input may already have been typed); do not resend it automatically",
    )
}

/// Type `text` into `pane` and submit it the way `tool` expects.
///
/// PANE ONLY: nothing here reads a stored session binding, which is
/// what lets `clank nudge` use it when the binding is the suspect
/// thing. The web bridge wraps this with the app-server attempt it is
/// willing to trust; the submit-sequence knowledge lives here once.
pub(crate) fn say_to_pane_for_tool(
    session: &str,
    pane: &str,
    tool: clank_core::vocab::Tool,
    text: &str,
) -> anyhow::Result<()> {
    if tool == clank_core::vocab::Tool::Codex {
        say_to_codex_pane(session, pane, text)
    } else {
        say_to_pane(session, pane, text)
    }
}

/// Say `text` to `pane` in `session` and press Enter.
pub(crate) fn say_to_pane(session: &str, pane: &str, text: &str) -> anyhow::Result<()> {
    say_to_pane_with_delay(session, pane, text, false)
}

/// Codex can treat Enter immediately after injected text as a pasted
/// newline. Its terminal fallback waits past the paste-burst window,
/// then sends a kitty-protocol Enter press. A held Stop hook may still
/// queue the prompt; `codex_chat` handles that when available.
pub(crate) fn say_to_codex_pane(session: &str, pane: &str, text: &str) -> anyhow::Result<()> {
    say_to_pane_with_delay(session, pane, text, true)
}

fn say_to_pane_with_delay(
    session: &str,
    pane: &str,
    text: &str,
    delay: bool,
) -> anyhow::Result<()> {
    let Some(argvs) = say_argvs(pane, text) else {
        return Ok(());
    };
    for (i, mut argv) in argvs.into_iter().enumerate() {
        if i == 1 && delay {
            std::thread::sleep(std::time::Duration::from_millis(200));
            argv = codex_submit_argv(pane);
        }
        super::open_zellij::write_pane_action(session, pane, &argv)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

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
    /// The interrupt key is FIRST and is the provider's own (Ctrl+C for
    /// Grok, Esc for the rest); the message and its submit follow, Codex's
    /// paste-safe Enter included. There is no shared pause between the
    /// interrupt and the message: nothing establishes how long a provider
    /// needs, so the busy case is best-effort and the doc says so rather
    /// than a timer pretending otherwise.
    #[tokio::test(start_paused = true)]
    async fn every_provider_is_interrupted_with_its_own_key_before_the_message() {
        use clank_core::Tool;
        let key = |argv: &Vec<String>| argv[4..].join(" ");
        for (tool, interrupt, submit) in [
            (Tool::Claude, "27", "13"),
            (Tool::Grok, "3", "13"),
            (Tool::OpenCode, "27", "13"),
            (Tool::Codex, "27", "27 91 49 51 59 49 58 49 117"),
        ] {
            let written = RefCell::new(Vec::<Vec<String>>::new());
            interrupt_and_submit_with("7", tool, "hello", |argv| {
                written.borrow_mut().push(argv);
                async { Ok(()) }
            })
            .await
            .unwrap();
            let written = written.into_inner();
            assert_eq!(written.len(), 3, "{tool:?}: {written:?}");
            assert_eq!(key(&written[0]), interrupt, "{tool:?} is interrupted first");
            assert_eq!(written[1][1], "write-chars", "{tool:?}: then the message");
            assert_eq!(written[1].last().unwrap(), "hello");
            assert_eq!(key(&written[2]), submit, "{tool:?}: then its own submit");
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
        let written = RefCell::new(0);
        let error = interrupt_and_submit_with("7", clank_core::Tool::Grok, "hello", |_| {
            *written.borrow_mut() += 1;
            async { anyhow::bail!("terminal input transport timed out; acceptance is unknown") }
        })
        .await
        .unwrap_err();
        assert_eq!(*written.borrow(), 1, "only the interrupt was attempted");
        let said = format!("{error:#}");
        assert!(
            said.contains("interrupt key's delivery was not confirmed"),
            "{said}"
        );
        assert!(!said.contains("message was not confirmed"), "{said}");
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
}
