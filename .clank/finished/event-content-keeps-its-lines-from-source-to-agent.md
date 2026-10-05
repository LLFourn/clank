# event-content-keeps-its-lines-from-source-to-agent
# Event content keeps its lines from source to agent

## Problem and required outcome

Life's master reported that command-source ingestion calls the TUI's `one_line`
helper on event content. Every line after the first is discarded before the
event is stored or delivered. Lloyd agrees that event content must not be forced
into one line. Preserve the complete message from producer through durable storage,
wake delivery, replay and `clank events read`.

## Contracts and implementation

- Preserve newlines, blank lines, indentation, tabs and Unicode in command-event
  content. A JSONL producer escapes these inside its JSON string; embedded message
  newlines are not extra producer records.
- Separate terminal escape/control sanitization from line compaction. Retain the
  controls needed for text layout (including line breaks and tabs); prevent terminal
  escape sequences and unsafe control bytes from reaching terminal displays. Define
  the sanitization precisely and share it where necessary without truncating text.
- Audit all event backends and the path from parsed content to WAL, wait/hook
  emission, read output, replay and remote projection. No presentation helper may
  replace the authoritative stored or agent-facing body with a preview.
- Timeline rows may use a compact preview to fit their layout, but the preview is
  derived only at that display boundary. Full content remains readable in the
  event detail/read view, with its original line structure. Do not impose a global
  single-line rule on timeline details or browser event content.
- Keep producer identity, deduplication, acknowledgement, required replies and wake
  receipts unchanged. Do not recover already-lost historical content by inventing
  it; existing events remain readable as stored. Add no polling or background work.
- Coordinate with the master of `/Users/llfourn/src/life` after the fix is reviewed
  and installed. Life can then remove its producer workaround that flattens
  newlines to ` / ` in `scripts/life-signal.py`.

## Verification and delivery

Use a real command-source fixture carrying paragraphs, blank lines, indented text,
tabs, Unicode and unsafe escape/control input. Assert the complete expected text
survives ingestion, durable replay, hook wake and receipt-aware `events read`.
Check compact previews cannot overwrite the body, other sources still deliver,
and acknowledgement/reply requirements remain intact. Implement the complete plan
in one commit with no internal named milestones; install the reviewed fix and
reply to Life's master with the confirmed resulting behavior.
