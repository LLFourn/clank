# web-queued-plan-editor
# Write and edit queued plans in the web interface

## Goal

Let a user create and edit queued plans as Markdown documents from the web interface.

## Work

Add discoverable controls for creating a queued plan, opening its Markdown source, editing it, and changing its priority. Reuse the queue's existing validation, handle resolution, and save operations; preserve the existing promote and unqueue controls. Keep server-side authorization and refusal messages consistent with other web actions.

## Acceptance

Automated browser and dummy-repo tests create and edit a queued Markdown plan, change its priority, and confirm the resulting document and queue order are visible in the TUI. A stale or invalid handle cannot overwrite another plan, and a failed save explains why.
