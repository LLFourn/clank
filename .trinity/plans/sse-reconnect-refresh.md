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

1. **Force a refresh on reconnect.** `connect_sse` owns the single
   `EventSource` for the tab. Drive a small state machine:
   - `connected: RwSignal<bool>` defaults to `true`.
   - `was_disconnected: Rc<Cell<bool>>` defaults to `false`. This
     is a **consumed** flag, not an "ever disconnected" flag.
   - `EventSource::new` failure (rare; URL parse / security
     errors): set `connected = false` and return. No retry — there
     is no handle to install handlers on.
   - `onerror`: `connected = false`; `was_disconnected.set(true)`.
     The browser auto-retries (~3 s default).
   - `onopen`: `connected = true`; if
     `was_disconnected.replace(false)` returned `true`, bump
     `EventStore.tick` exactly once.

   This handles three cases uniformly:
   - Page load with daemon up → first `onopen`,
     `was_disconnected` is `false`, no refresh. Resources fetch
     their initial data via their normal mount path.
   - Page load with daemon down → `EventSource` opens, browser
     fires `onerror` immediately, sets `was_disconnected = true`.
     When the daemon comes up the first `onopen` fires, the
     consumed flag triggers exactly one `tick` bump → resources
     fetch.
   - Live reconnect mid-session → identical to case 2.

   Factor the state-machine itself into a pure helper
   (`ReconnectState` with `on_error(&mut self)` and
   `on_open(&mut self) -> bool` where the bool says "caller
   should bump tick"). The DOM-touching `connect_sse` wraps it.
   Helper is unit-testable without `EventSource`.

2. **Surface the connection state.** Render a "reconnecting…"
   badge in the app shell while `connected.get() == false`. The
   `connected` signal lives on `EventStore`, already provided
   via context; the badge reads it in the same place that today
   reads `muted` for the chime toggle.

## Files touched

- `frontend/src/store.rs` — add `connected` to `EventStore`,
  rewrite `connect_sse` to install `onopen` + `onerror` alongside
  the existing `onmessage`.
- `frontend/src/main.rs` (or wherever the top-level shell renders
  — search for the existing chime/mute toggle position) — render
  the reconnecting badge near it.
- `frontend/style.css` — add a single class for the badge,
  matching the style of the existing chime/mute toggle.

Unit tests on the pure `ReconnectState` helper in
`frontend/src/store.rs`. Browser-driven `EventSource` behavior
itself stays manual; the state machine that decides "bump tick or
not" does not.

## Acceptance criteria

- Kill the daemon. Frontend within ~3 s shows a visible
  "reconnecting" indicator.
- Restart the daemon (`just restart`). Within ~3 s of the daemon
  binding the listener, the indicator disappears and every
  visible resource re-fetches.
- Page-load while the daemon is down: indicator appears
  immediately, and the first successful connect refreshes once.
- On initial page load with the daemon already up, no
  double-fetch / no spurious "refresh" flash — the first
  `onopen` does not bump `tick`.
- No daemon code change and no SSE wire-shape change. The
  `frontend/dist/` bundle does change, so the daemon still needs
  a recompile/restart to pick up the new wasm under the embedded
  flow (`just build && just restart`); the `serve-dev
  --frontend-dist frontend/dist` loop picks it up without a
  daemon rebuild.
- `ReconnectState` unit tests cover: first open does not refresh;
  error then open refreshes once; open after open without an
  intervening error does not refresh; two errors then one open
  refreshes once.

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

- `cargo test -p trinity-frontend reconnect` — unit tests on
  `ReconnectState` (host target; no wasm runner needed for a
  pure helper).
- Manual: `just serve`, open the page, kill the daemon with
  `pkill -f "target/release/trinity serve"`, observe badge.
  `just restart` (or restart the daemon directly), observe badge
  clears and any open plan page refreshes if data changed
  in between. Also try loading the page with the daemon already
  killed and confirm the first reconnect refreshes once.
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
