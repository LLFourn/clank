# codex-chat-submit-and-wake
# Make Codex chat submission and wake reliable

## Goal

Submitting a prompt to Codex from Clank must submit it, rather than insert a newline. Work arriving while Codex is idle must wake the session without hiding a newly submitted message behind a held Stop hook.

## Work

Trace the current Zellij input path and Codex Stop-hook delivery path. Check current supported Codex app-server and hook interfaces against official documentation and local behavior; determine whether a more direct, Claude-like prompt and wake integration is available. Implement the best supported approach, including correct prompt submission and ordering when a Stop hook is active. Keep a fallback if Codex still requires a held Stop hook, and make its limitations visible.

## Acceptance

A prompt submitted through Clank reaches a live Codex session as a prompt, including while the Stop hook is waiting. Pending Clank work still wakes Codex. Verify both paths in a live session and document any remaining platform limitation.

## Live verification (2026-09-26)

- **Held Stop hook, app-server path:** In an isolated Clank-bound Codex 0.157.0 session, `ps` showed `clank wait --deliver-to-session 01a0dd3b-589d-70f0-bbd0-008794ba9f13` parked inside the Stop hook. A temporary ignored Rust smoke test called `codex_chat::send` with `Reply with exactly: RUST_DIRECT_SMOKE_OK`. The live Codex pane showed that user prompt, answered `RUST_DIRECT_SMOKE_OK`, and returned to `Clank: checking for pending review …`. The test passed; its temporary test code was removed.
- **Work wake:** While that same session was parked again, `clank send --repo /private/tmp/clank-codex-chat-smoke --to codex` created an inbox item. The Codex pane woke and began handling it (read the master skill and attempted `clank inbox show`). This verified delivery through the existing Stop-hook wait.
- **Terminal fallback:** In a separate isolated Codex 0.157.0 pane, a temporary ignored Rust smoke test called `open_zellij::say_to_codex_pane` with `Reply with exactly: FALLBACK_FUNCTION_OK`. The test passed, and the pane showed a submitted user prompt, the answer `FALLBACK_FUNCTION_OK`, and an empty composer. The test code, temporary sessions and temporary folder trust entries were removed.

Codex's background Stop hook cannot start a new turn when its wait completes, so the synchronous hook remains. If the local app-server control socket or turn-list API is unavailable, the terminal fallback submits the prompt but may queue it behind that hook; the remote Stop button releases it.
