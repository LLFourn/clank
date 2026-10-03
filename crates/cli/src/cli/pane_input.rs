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

pub(crate) async fn interrupt_pane_async(
    session: &str,
    pane: &str,
    tool: clank_core::Tool,
) -> anyhow::Result<()> {
    super::open_zellij::write_pane_action_async(session, pane, &stop_argv(pane, tool)).await
}

pub(crate) async fn submit_async(
    session: &str,
    pane: &str,
    tool: clank_core::Tool,
    text: &str,
) -> anyhow::Result<()> {
    let Some(argvs) = say_argvs(pane, text) else {
        return Ok(());
    };
    for (index, mut argv) in argvs.into_iter().enumerate() {
        if index == 1 && tool == clank_core::Tool::Codex {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            argv = codex_submit_argv(pane);
        }
        super::open_zellij::write_pane_action_async(session, pane, &argv).await?;
    }
    Ok(())
}

/// Submit once, then request interruption. Terminal acceptance does not prove
/// consumption or cancellation. Errors retain partial submission evidence.
pub async fn submit_and_interrupt_async(
    session: &str,
    pane: &str,
    tool: clank_core::Tool,
    text: &str,
) -> anyhow::Result<()> {
    submit_then_interrupt(
        || submit_async(session, pane, tool, text),
        || interrupt_pane_async(session, pane, tool),
        text.is_empty(),
    )
    .await
}

/// A fresh prompt is the handoff across native cancellation hooks. User content
/// stays in its durable inbox: this only asks the target to read pending work.
/// Role is deliberately resolved by Clank when the target reads, never captured
/// in terminal text before interruption. No receipt or acknowledgement is forged.
pub(crate) const EVENT_WAKE: &str = "Clank has pending events for this session. Run `clank events list`, read the pending events with `clank events show`, and handle them before acknowledging. Follow any required-reply contract. Your current role and work are determined by Clank.";

pub(crate) async fn wake_for_events_async(
    session: &str,
    pane: &str,
    tool: clank_core::Tool,
) -> anyhow::Result<()> {
    submit_and_interrupt_async(session, pane, tool, EVENT_WAKE).await
}

async fn submit_then_interrupt<S, I>(
    submit: impl FnOnce() -> S,
    interrupt: impl FnOnce() -> I,
    empty: bool,
) -> anyhow::Result<()>
where
    S: std::future::Future<Output = anyhow::Result<()>>,
    I: std::future::Future<Output = anyhow::Result<()>>,
{
    use anyhow::Context;
    anyhow::ensure!(!empty, "an empty nudge must not interrupt the agent");
    submit().await.context(
        "message submission was not confirmed; interruption was not attempted (some input may already have been typed)",
    )?;
    interrupt().await.context(
        "message was submitted, but interruption failed; do not resend the submitted message automatically",
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

    #[tokio::test]
    async fn cancellation_follows_successful_submission_once() {
        let calls = RefCell::new(Vec::new());
        submit_then_interrupt(
            || async {
                calls.borrow_mut().push("submit");
                Ok(())
            },
            || async {
                calls.borrow_mut().push("interrupt");
                Ok(())
            },
            false,
        )
        .await
        .unwrap();
        assert_eq!(*calls.borrow(), ["submit", "interrupt"]);
    }

    #[tokio::test]
    async fn uncertain_submission_never_triggers_cancellation() {
        let error = submit_then_interrupt(
            || async { anyhow::bail!("Enter was refused") },
            || async { panic!("must not interrupt after failed submission") },
            false,
        )
        .await
        .unwrap_err();
        assert!(format!("{error:#}").contains("some input may already have been typed"));
        assert!(format!("{error:#}").contains("Enter was refused"));
    }

    #[tokio::test]
    async fn failed_interruption_preserves_partial_submission_outcome() {
        let error = submit_then_interrupt(
            || async { Ok(()) },
            || async { anyhow::bail!("pane disappeared") },
            false,
        )
        .await
        .unwrap_err();
        assert!(format!("{error:#}").contains("message was submitted"));
        assert!(format!("{error:#}").contains("pane disappeared"));
    }

    #[tokio::test]
    async fn empty_nudge_has_no_side_effects() {
        assert!(
            submit_then_interrupt(
                || async { panic!("must not submit") },
                || async { panic!("must not interrupt") },
                true,
            )
            .await
            .is_err()
        );
    }
}
