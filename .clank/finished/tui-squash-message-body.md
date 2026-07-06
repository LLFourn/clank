# tui-squash-message-body
# TUI squash composes a valid WHAT+WHY message (bug: one-line input always rejected)

## Bug (lloyd, live)

Squashing a finished plan from the TUI plan-actions page always fails
with finish's mandatory-message help ("A bare `finish` or a subject
with no body is rejected"). Root cause, verified:
`validate_finish_message` (finish.rs:310) requires a subject AND a
`\n\n`-separated WHY body of at least MIN_WHY_BODY_CHARS — the TUI's
squash flow (`submit_plan_input`, SquashMessage arm) submits the
single input line verbatim as the squash message, which can never
satisfy the body requirement. The flow was structurally dead on
arrival; it only reached users on non-autosquashed finished plans
(the squash row correctly hides on single-commit ones).

## Fix

The plan being squashed already carries a WHY: its finalize commit's
message body. Compose the squash message instead of sending the raw
line:

- squash message = `<typed subject>\n\n<finalize commit's body>`.
- Legacy finished plans (pre-mandatory-messages, body empty or the
  placeholder shape): fall back to a provenance body — "collapsed to
  one commit from clank status --tui; original finish predates
  finish-message-mandatory." — long enough to pass validation and
  honest about why no richer WHY exists.
- Prefill stays the finalize SUBJECT (already implemented); the body
  rides along invisibly. The input screen's hint line changes to say
  the WHY is kept from the finish message.

Single-source note: compose in ONE place (the SquashMessage submit
arm) and add a test against `validate_finish_message` itself with the
composed shape, so the TUI can't drift from the validator again.

## Tests

- Pure: composed message (subject + real body) passes
  validate_finish_message; composed legacy fallback passes; the raw
  one-liner (the bug) is pinned as REJECTED by the validator.
- Integration: finished multi-commit plan (finished with --no-squash,
  real WHAT+WHY message) → submit-level compose + finish --squash
  succeeds and the squashed commit's message carries typed subject +
  original body. Legacy-shaped plan (placeholder finish message) →
  provenance body path succeeds.

## Non-goals

A second input line for a hand-typed WHY (the one-line widget stays);
changing validate_finish_message; the squash-row visibility question
(dimmed "already one commit" row) is separate and awaits lloyd's call.

## Acceptance

From the TUI, squashing a multi-commit finished plan with a typed
subject succeeds; the squash commit's body is the original finalize
body (or the provenance line for legacy plans); clippy/fmt/suites
green.
