# timeline-renders-grok-commands-and-image-thumbnails-cleanly
# Timeline renders Grok commands and image thumbnails cleanly

## Problem and outcome

The remote timeline renders Grok terminal calls as raw run_terminal_command
arguments and XML-like task output. Codex image attachments appear surrounded by
separate bubbles containing opening image metadata and closing image tags.
Large images dominate the conversation. Normalize these into the shared command
presentation and compact, expandable attachment thumbnails.

Implement the complete plan in one implementation commit. There are no internal
named milestones; provider normalization and its shared presentation must be
reviewed together.

## Grok commands

- Trace actual Grok transcript tool call/result envelopes and adapt them into the
  shared command model used by other agents. Preserve call/result association,
  ordering and stable identity across streaming, transcript reload and reconnect.
- Group commands using the existing concise presentation, such as Ran 3 commands,
  with useful individual command labels and expandable actual output. Show the
  command rather than run_terminal_command or a raw argument map as the primary
  presentation. Avoid duplicated descriptions and implementation fields.
- Parse recognised background-task result metadata into a compact running,
  completed, failed or cancelled presentation as evidence permits. Keep task IDs,
  output paths and original payload inspectable as details instead of exposing
  XML-like envelopes as ordinary output. Unknown or malformed tool results must
  retain readable fallback content, not silently disappear.
- Correctly represent background registration separately from command completion.
  Neither a running task nor a successful registration proves the command finished.
  Include the screenshot's clank wait background call as a regression fixture.

## Codex images and timeline attachments

- Trace the actual Codex transcript representation of image attachments, including
  image marker text interleaved with attachment items and accompanying prose.
  Recognise structural opening/closing markers and metadata as one attachment;
  do not render them as separate chat bubbles. Preserve surrounding user text,
  multiple attachments, ordering and literal user-authored text that merely looks
  like markup. Fix the normalization boundary rather than globally hiding strings.
- Show compact aspect-ratio-preserving thumbnails in the timeline, with sensible
  responsive maximum display dimensions. These are presentation constraints, not
  arbitrary rejection limits on image file size or resolution. Preserve original
  image quality for expanded viewing. Prevent image load/layout shifts from
  displacing the reading position or live scroll anchoring.
- Pressing a thumbnail opens an expanded image viewer with a clear close action,
  Escape support, keyboard access and appropriate focus restoration. Support
  mobile viewing and portrait, landscape and very tall images without overflowing
  the viewport. Keep descriptive attachment labels and accessible alternatives.
- Missing or unavailable images show one useful attachment placeholder with a
  recoverable error; no dangling metadata bubbles. Avoid exposing local absolute
  paths as ordinary conversation text and retain necessary debug metadata safely.
- Reuse existing authenticated attachment transport. Do not broaden filesystem
  access to arbitrary paths supplied in transcript text. Lazy-load thumbnails
  where useful, without polling or loading all full-resolution originals eagerly.

## Validation

Use representative Grok terminal/background fixtures and actual Codex attachment
fixtures from the reported shapes. Cover partial streaming, reconnect/reload,
multiple images with prose, literal markup, missing attachments and unknown task
results. Verify shared command grouping remains correct for other agents. Provide
visual previews at desktop and mobile widths showing collapsed command groups,
compact timeline thumbnails and expanded viewing. Distinguish fixture checks
from live harness acceptance and document any remaining provider limitation.

## Implementation and validation

Grok terminal calls now use their command as the visible label. The actual native
`run_terminal_command` registration envelope and `get_command_or_subagent_output`
task headings were checked against this repository's Grok transcript metadata.
Registration remains running until a recorded terminal observation says otherwise;
later observations update the original call in both incremental feeds and replay.
Several task observations, out-of-order results and repeated registration are
covered. Unknown or malformed results retain ordinary readable output. Original
task metadata and call arguments remain in expandable details. This reads recorded
provider state; it does not add harness polling or change Grok's wait lifecycle.

Codex's adjacent structured opening marker, image block and closing marker become
one attachment. Surrounding prose and literal marker-like text are preserved.
Timeline frames carry descriptors rather than pixels; retained originals share
their bytes across snapshots. Visible thumbnails are requested through the existing
authenticated, same-origin action/bridge route. Requests name a retained session,
generation, turn, index and content key, never a filesystem path. Thumbnail decoding
runs on demand, one worker at a time, with cached small previews; full originals
are fetched only when expanded. No image-size acceptance cutoff, new watcher or
polling loop was added. Fixed thumbnail slots prevent loading/retry layout shifts.

Validation passed: 81 CLI web tests (one existing ignored test), 25 web library
tests, seven bridge integration tests and the custom host example test. Coverage
includes parser fixtures, replay/incremental parity, missing attachments, stale
requests, rebinding while a request waits, authorization/origin refusal and large
action transport. These are fixture and browser checks, not a fresh end-to-end
native harness launch. Unknown provider task formats deliberately use the readable
fallback rather than inferring completion.

The reproducible browser check is `scripts/check-timeline-ui.cjs` (requires Node
and Playwright with Chromium). It checks desktop/mobile sizing, command groups,
aspect ratios, expansion, actual-size viewing, Escape/focus restoration, retries,
replay and an original-image reply arriving after the conversation changes. The
review previews are `/tmp/clank-timeline-preview/desktop-timeline.png`,
`desktop-commands.png`, `desktop-expanded.png`, `mobile-timeline.png` and
`mobile-expanded.png` in that directory. Formatting and whitespace checks passed.
