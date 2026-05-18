# sse-reconnect-refresh

Make the Leptos frontend recover cleanly when the daemon restarts:
force a one-shot refresh of subscribed resources on SSE reconnect,
and surface a "reconnecting" state in the UI.

## Why

Today `EventSource("/events")` auto-reconnects (browser default
~3 s), but `frontend/src/store.rs::connect_sse` only sets an
`onmessage` handler. There is no `onopen` (no refresh signal on
reconnect) and no `onerror` (no UI indication that the daemon went
away). Consequences after `just restart`:

- If a plan was modified during the outage, the UI keeps showing
  the pre-outage data until the next live event happens to arrive
  — which may be never if the user is sitting still on a plan page.
- Network errors from in-flight `fetch_*` calls during the outage
  bubble up as `FetchError::Network` with no recovery path.
- User has no signal at all that anything is wrong.

Server-side, `events_route` in `src/server/http.rs:146` is push-only
and intentionally does **not** replay the broadcast ring on connect
("every page reload would re-fire chimes + reloads for every
historical event"). So the freshness fix must live on the client.

## What

Two small frontend additions, no daemon change:

1. **Force a refresh on reconnect.** `connect_sse` already owns the
   single `EventSource` for the tab. Add an `onopen` handler that:
   - On the very first open (initial page load), do nothing —
     resources fetch their initial data via their normal path.
   - On every subsequent open (reconnect), bump `EventStore.tick`
     once so every `LocalResource` keyed on `tick` re-runs its
     fetcher.

   Track "have we ever disconnected" with an `Rc<Cell<bool>>`
   captured by the closures.

2. **Surface the connection state.** Add a
   `connected: RwSignal<bool>` to `EventStore` (default `true`).
   `onerror` sets it `false`; `onopen` sets it `true`. Render a
   small "reconnecting…" badge in the app shell while
   `connected.get() == false`.

## Files touched

- `frontend/src/store.rs` — add `connected` to `EventStore`,
  rewrite `connect_sse` to install `onopen` + `onerror` alongside
  the existing `onmessage`.
- `frontend/src/main.rs` (or wherever the top-level shell renders
  — search for the existing chime/mute toggle position) — render
  the reconnecting badge near it.
- `frontend/style/*.css` (or whatever the existing badge classes
  use) — add a one-class badge.

No test file changes needed for behavior — SSE reconnect is a
browser-driven behavior that's hard to assert in a unit test.

## Acceptance criteria

- Kill the daemon. Frontend within ~3 s shows a visible
  "reconnecting" indicator.
- Restart the daemon (`just restart`). Within ~3 s of the daemon
  binding the listener, the indicator disappears and every
  visible resource re-fetches.
- On initial page load, no double-fetch / no spurious "refresh"
  flash — the first `onopen` does not bump `tick`.
- Daemon SSE shape is unchanged. No new wire fields. No daemon
  recompile required.

## Non-goals

- Server-side event replay / `Last-Event-ID` support. The current
  client-driven refresh is enough for the local-dev tool's needs.
- Retrying failed `fetch_*` calls during the outage. The user
  will see the route-level error if they click during the gap;
  the next refresh fixes it.
- Detecting reconnects via heuristics (visibilitychange, focus).
  EventSource's own onopen is sufficient.

## Rules

- Single source of truth for connection state: the `connected`
  signal on `EventStore`. Components read it, do not derive
  parallel state.
- No `set_interval` polling. EventSource fires `onopen` /
  `onerror`; rely on those.
- Keep `connect_sse` the only place that touches the EventSource
  handle — closures `forget()` and `Box::leak` continue to be the
  ownership model.

## Testing

- Manual: `just serve`, open the page, kill the daemon with
  `pkill -f "target/release/trinity serve"`, observe badge.
  `just restart` (or restart the daemon directly), observe badge
  clears and any open plan page refreshes if data changed
  in between.
- `cargo check -p trinity-frontend --target wasm32-unknown-unknown`
- `cd frontend && trunk build`
- `just check` (clippy, fmt, workspace tests).

## Trade-off honest record

The simplest fix is the right fix here: client-side refresh on
reconnect. The alternative (server replay via `Last-Event-ID`)
would require a persistent monotonic event log, deduplication on
the client, and a new wire field — far too much machinery for a
local dev tool whose downtime is measured in seconds.

The cost is that during a reconnect we refresh the route's data
unconditionally, even if nothing on that route actually changed.
For a localhost daemon that's a few extra `fetch` calls per
reconnect — invisible.
