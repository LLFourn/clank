# status-tui-block-ask-scroll

Make blocked-plan ask text readable in `clank status --tui` when the prompt is longer than the terminal height.

## Problem

When a plan is blocked, the TUI surfaces the block ask/reason in the header area. Some ask prompts are long enough that the terminal cannot show the full text, so the user cannot read the complete question from the TUI.

## Goal

Render blocked-plan ask text in its own wrapped area beneath the main fixed header and before the commit log. It may participate in the same scrollable content region as the commit log, but it should be visually distinct from the main header and always scrollable when it overflows.

## Implementation Notes

- Keep the compact status/header summary fixed at the top.
- Move the long block ask body out of the fixed header path.
- Add a block ask section immediately below the header and above the log rows.
- Wrap the ask text to the available terminal width.
- Include this section in the scrollable body so normal scroll keys can reveal all wrapped lines.
- Preserve the existing commit log scroll behavior and live refresh behavior.
- If there is no active block ask, do not reserve empty vertical space.

## Acceptance

- A long block ask can be read completely in `clank status --tui` on a short terminal by scrolling.
- The main header remains visible and compact.
- The block ask appears before the commit log and is visually separated from log entries.
- Commit log scrolling still works after the ask section.
- Existing status TUI tests remain green; add or update pure rendering tests for wrapped block ask placement and scrolling without spawning external binaries.