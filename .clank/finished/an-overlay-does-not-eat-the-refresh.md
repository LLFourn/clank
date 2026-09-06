# an-overlay-does-not-eat-the-refresh

> While I was on the plan page a new commit came through but when I
> went back to the main page with esc it wasn't there. I restarted it
> and it came back. Does going into menu items cause you to miss
> events? — lloyd

## Yes, for overlays

`clank status --tui` has two kinds of "page". The plan-actions page,
the agent page and the event page are MODES of the one event loop:
the watcher's `Ev::Refresh` reaches the loop bottom, flags
`refresh_pending`, and the trailing-edge flush rebuilds the snapshot
whatever the mode — the per-mode refresh arm then rebinds the cursor.
Those pages do not lose events.

The commit-detail, queued-plan, stashed-plan and error OVERLAYS are
not modes. `detail: Option<Overlay>` runs its own `recv_timeout` loop
at the top of every pass and `continue`s past everything below it. Its
`Ok(Ev::Refresh)` arm re-fetches the overlay's OWN data (the commit,
the queue markdown) and nothing else: `refresh_pending` is never set,
the flush never runs, and the wake is gone. When the overlay closes,
the main page paints the snapshot from before it opened, and stays
stale until the NEXT watcher wake — which, on a quiet repo, is never.
Restarting the TUI is the only way to see the commit. `Ev::Resize`
is dropped the same way (`log.request_fill()` is not called), so a
pane resized behind an overlay comes back with a short log.

The fix is one line of state per arm, not a redesign: an overlay's
`Ev::Refresh` also sets `refresh_pending = true`, and its `Ev::Resize`
also calls `log.request_fill()`. The flush stays deferred until the
overlay closes — the loop bottom is not reached while it is open —
and `deferred_wait` already shortens the next recv so a pending flush
fires promptly. The signature gate then decides whether anything
actually changed, exactly as it does for a wake that lands on the main
page.

## Why not rebuild under the overlay

The overlay loop could run the flush itself, but the rebuild rebinds
cursors by mode and the overlay sits above an arbitrary mode; doing
that work while the page is hidden buys nothing the operator can see
and duplicates the loop-bottom logic. Carrying the flag is the whole
fix.

## Tests

The overlay loop is the event loop and is not driven in tests, so the
arm is EXTRACTED: a pure `fn overlay_event(ev, ...) -> OverlayStep`
(or equivalent) that the loop calls, returning what the overlay did
with the event AND whether the main page's refresh/fill flags must be
raised. Tests:

- `Ev::Refresh` behind an overlay leaves `refresh_pending` raised, so
  the first pass after the overlay closes flushes.
- `Ev::Resize` behind an overlay leaves the log fill requested.
- The overlay's own re-fetch still happens (the commit page still
  updates in place).

Mutation: drop the flag in the arm; the first test fails.

## Out of scope

- Plan-actions / agent / event pages: already mode-driven and
  refreshed; verified by reading, no change.
