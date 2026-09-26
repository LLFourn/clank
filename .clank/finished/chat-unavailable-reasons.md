# chat-unavailable-reasons
# Explain unavailable chat and restore Codex transcript access

## Goal

Rename the transcript view to Chat. If Chat is disabled, clicking it must explain why. Restore access to the Codex conversation where the transcript is currently missing.

## Work

Trace the Codex transcript availability check and viewer path. Fix the missing transcript if the data is available. Give every disabled Chat state a concrete reason shown in a popup on click, and use Chat consistently in the web interface.

## Acceptance

Automated browser fixtures show that Chat replaces the transcript name throughout the web interface, and that Codex Chat opens and displays the conversation when transcript data exists. Cover each reason Chat can be unavailable with a fixture that clicks its control and sees the actual reason in a popup; a silent disabled tab fails. Verification requires no person at the keyboard and no live agent login.
