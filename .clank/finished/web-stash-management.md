# web-stash-management
# Inspect and operate on stashed plans in the web interface

## Goal

Make the Clank TUI's stash management actions available in the web interface.

## Work

Inventory the TUI stash controls and expose the same stashed-plan details and actions in the web interface, including any restore, discard, or edit operation the TUI offers. Reuse the underlying stash operations and validation, with confirmation for destructive actions. Keep server-side authorization and refusal messages consistent with other web actions.

## Acceptance

Automated browser and dummy-repo tests inspect a stashed plan, perform each TUI stash action, and confirm the resulting state in the TUI. Refused and stale actions explain what happened without changing another plan.
