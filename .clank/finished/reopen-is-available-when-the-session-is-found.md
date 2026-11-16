# reopen-is-available-when-the-session-is-found
# Reopen is available when the session is found

## Problem

The agent detail menu offers `reopen pane` only for `Presence::Missing`.
Both `Live` and `Unknown` hide it. A stale presence report or failed pane
listing therefore removes the recovery action even when Clank can find the
Zellij session. The user requires the action to ALWAYS be available whenever
that session can be found, for every agent role.

## Change

- Separate session availability from the cached presence of an individual
  agent's pane. A found session makes `reopen pane` available for live,
  missing, exited, and unknown pane states. A failed pane listing must not
  turn a known session into an unavailable menu action.
- Carry the necessary session-discovery fact from the existing Zellij worker
  to the menu. Do not run discovery or subprocesses from rendering or input
  handling, or introduce periodic polling to support this action.
- On selection, use the existing targeted reopen path's fresh discovery.
  A live pane produces an informative `already open` result without closing
  or duplicating it. A missing or exited pane is reopened for that agent
  only. Failed or ambiguous discovery produces a visible explanation and
  permits retry; it must not silently remove the action while the session
  is still known.
- Adjust the menu description: offering the action no longer asserts that
  the pane is gone. Keep action selection stable across presence updates.
- When no session can be found, clearly explain why reopening is unavailable.
  Do not equate unknown pane presence with absence of a session.

## Validation

Update the existing menu and worker tests rather than adding redundant
variants. Cover all roster roles and live/missing/unknown presence with a
found session; a pane-listing failure with a still-known session; and no
discoverable session. Verify that presence transitions do not remove the
action or change the selected action. Retain targeted reopen coverage for
already-live, missing, exited, ambiguous, and unanswered cases, ensuring
other agents are untouched. Use existing Rust fixtures and fake I/O; no live
Zellij session, new test binaries, Python, or additional shell fixtures.

Implement in one commit. Run focused TUI menu/render/reconciler tests and
the applicable Zellij boundary gates.

## Scope

This changes action availability and feedback, not pane ownership rules,
agent session resumption, forced restarts of live agents, or `clank open`.

## Implemented

The existing worker reports session discovery independently of pane presence
and listing reachability. Agent detail menus use that fact for every role,
including when the pane is live or its presence is unknown. The description
asks to check and open if needed; the existing targeted reopen retains its
fresh listing, already-open response, and ownership checks. When discovery
has found no session, the page explains why reopen is unavailable below the
binding details, preserving the session ID in small panes.

Validation: 75 input tests, 196 rendering tests, 80 Zellij worker tests, and
8 focused integration/boundary checks passed. Formatting and diff checks
passed. Existing fixture tests now cover stable availability through live,
missing and failed-listing transitions, every roster role, no-session
feedback, and selection stability. No polling or live Zellij mutations were
added. Installation from source was requested by the user and is in progress.
