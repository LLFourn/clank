# event-icon-validation-cannot-disable-an-agents-wait
# Event icon validation cannot disable an agent's wait

## Problem

In life, a Signal command source used the two-column glyph `💬`. The timeline emitted warnings, but `resolve_wait_event_sources` also rejected that glyph and the command-source runner refused to start. Consequently the assistant's native Codex Stop hook returned immediately with `hook: wait exited 1 stderr=command source signal glyph U+1F4AC measures 2 display columns; the row reserves one`. The hook appeared installed and trusted while the agent could never wait for work. Removing the glyph override restored its normal hook-owned wait. This is a presentation constraint incorrectly controlling event delivery.

## Required behavior and contracts

- A configured icon is presentation metadata. Its width or unsupported form must never prevent a valid command source from starting, reject the complete wait-source set, or disable unrelated events and an agent's automatic wait.
- Support ordinary one- and two-column icons, including `💬`, with measured terminal layout. Determine whether a grapheme-based representation is required for common emoji sequences and use the existing Unicode width infrastructure. Avoid introducing a new arbitrary character-count restriction.
- Unsafe or unsupported display input, such as controls or an empty/non-renderable icon, falls back to the backend's default icon on display surfaces. Never emit raw controls. A useful diagnostic may report the fallback, but it is not a lifecycle error and must not flood status/history.
- Use one shared icon interpretation across TUI, timeline/log, remote projection, and source validation. Readers must not disagree about whether the source is deliverable because of its icon.
- Preserve genuine command, reply, receipt, ownership, and event protocol validation. This change must not weaken event acknowledgement or wake-receipt contracts.
- No new polling, per-waiter watchers, periodic retries, or agent turns are needed. Presentation normalization is performed when configuration/display state changes, using the existing event-driven paths.

## Implementation

Audit all `glyph_refusal` callers, particularly `resolve_wait_event_sources`, the command-source runner, and timeline glyph resolution. Remove presentation refusal from delivery admission. Centralize safe display normalization and measure the icon's actual width where terminal rows reserve space; inspect hard-coded cell counts and truncation paths. Preserve safe defaults for configuration predating this change. Ensure the remote renderer also receives the usable icon rather than dropping supported emoji.

## Verification and completion

Add a regression covering a configured command source with `💬` that actually starts and delivers an event, together with another source to prove the complete wait remains operational. Exercise the normal hook-owned wait path against a controlled fixture, without creating false receipts for real events. Check wide-icon alignment and safe fallback for control/non-renderable input on applicable display surfaces. Verify fallback diagnostics do not repeat endlessly. Run the relevant tests and a real isolated native hook smoke check. Implement the complete plan in one commit with no internal milestones. After review, install the approved binary if needed; restore the desired Signal icon in life and confirm the assistant still has its normal blocking wait.

## Implementation evidence

Icon checks no longer participate in source admission or producer startup. Display boundaries share safe grapheme normalization and Unicode width measurement; controls and non-renderable icons quietly use the default badge. Wrapping and truncation preserve complete emoji clusters. Command, reply, receipt and session ownership checks remain in place, with no new polling or background work.

Validation: all 2,242 library tests passed (three existing ignored tests), all seven hook/receipt acceptance tests passed, and all 17 wait-event-source integration tests passed with live session identity removed from the fixture environment. The isolated native Codex smoke test started both a `💬` source and an invalid-icon source, remained blocked in its native Stop wait, emitted a wake receipt for the controlled `💬` event, and let the agent acknowledge it. All temporary native fixture processes were stopped. Approved binary installation and restoration of the Signal icon in life remain the post-review completion check.
