# grok-terminal-interrupt
# Add interruption controls for Grok terminal sessions

## Goal

When Grok is being used through the remote terminal interface, the user must be able to interrupt the current activity so the agent can read queued messages.

## Work

Inspect the terminal input bridge and Grok control semantics. Grok documents Ctrl+C as cancellation while a turn is running and Esc as a non-cancelling key; make the remote Stop control send the appropriate interrupt to the actual Grok pane. Check how queued messages become active after interruption and repair that path if needed. Preserve ordinary terminal input behavior and give visible feedback when delivery fails.

## Acceptance

An automated dummy-repo/pane test verifies that a busy Grok session receives the interrupt through the remote control, the queued message can become active, and delivery failure is shown in the interface. Verification requires no person at the keyboard or live Grok login.
