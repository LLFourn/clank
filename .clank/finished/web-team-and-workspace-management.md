# web-team-and-workspace-management
# Manage the repo team from the web interface

## Goal

Make the Clank TUI's repo roster actions available in the web interface.

## Work

Expose the TUI's repo roster actions—add by library name or tool, remove, change review tier, promote to master, and swap agents—through discoverable web controls. Show each agent's configured invocation and bound-session status. Reuse the underlying roster operations and validation. The web interface must not bind or rebind an agent session: the agent and its start-session hook own that action. Keep server-side authorization and error reporting consistent with existing web actions. Auto mode and session or pane recovery are separate queued work.

## Acceptance

Automated browser and dummy-repo tests cover each repo roster action and the agent details. Add, swap, remove, and role changes appear in the TUI's roster, and failures explain why an action was refused. No web action changes a session binding.
