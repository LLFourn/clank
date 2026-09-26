# web-agent-session-actions
# Bring agent session and pane recovery to the web interface

## Goal

Expose the Clank TUI's clear-session and reopen-pane actions in the web interface.

## Work

Show whether an agent has a bound session and a live pane. Offer clear-session only for a bound agent and reopen only when its pane is missing. Clear a session with the TUI's compare-and-clear operation and a confirmation so a binding that changes during the prompt is not removed. Route reopen through the TUI's pane reconciler; the web server must not create or bind an agent session itself. Report refusals and outcomes beside the action.

## Acceptance

Automated browser and dummy-repo tests cover both actions, their availability rules, a binding changed during confirmation, and a refused reopen. The resulting session and pane state appears in the TUI. No live agent login or person at the keyboard is required.
