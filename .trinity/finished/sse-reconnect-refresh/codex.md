APPROVE

Reviewed the full series (74d30fe plan intro → 8866217 plan
revision → 43a5a0c implementation). The plan landed exactly as
spec'd:

- `frontend/src/store.rs::ReconnectState` is a pure helper with a
  consumed `was_disconnected` flag. `on_open` reads-and-clears,
  so the very first open after page load returns false (no
  spurious refresh), and every open that follows at least one
  error returns true exactly once. Unit-tested against all four
  cases enumerated in the plan: first-open no refresh, error-then-
  open refreshes, duplicate-open no refresh, two-errors-one-open
  refreshes once.
- `EventStore.connected: RwSignal<bool>` is the single UI source
  of truth for connection state. The DOM-touching `connect_sse`
  installs `onopen` + `onerror` alongside the existing
  `onmessage` and routes both through `ReconnectState`. If
  `EventSource::new` itself fails (rare; URL parse / security
  errors), `connected` is flipped to false and the function
  returns — no handle to install handlers on.
- `frontend/src/main.rs::ConnectionBadge` renders a "reconnecting…"
  pill in the app shell while `!connected.get()`, gated through
  Leptos's `<Show>` so it only mounts during outages. CSS at
  `frontend/style.css` positions it next to the existing
  mute-toggle in the top-right corner.
- No daemon code change. No SSE wire-shape change. The
  `frontend/dist/` bundle changes, so picking up the new behavior
  under the embedded-bundle flow requires the usual
  `just build && just restart`; the `serve-dev --frontend-dist
  frontend/dist` loop picks it up without a daemon rebuild.

Verification passed:
- `cargo test -p trinity-frontend reconnect` — 4 passed
- `cargo check -p trinity-frontend --target wasm32-unknown-unknown`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo fmt -- --check`
- `cd frontend && trunk build`
- `cargo test --workspace --exclude trinity-frontend`

Approved as-is.
