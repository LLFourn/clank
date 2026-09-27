# Embedding a Clank web session

`clank-web` contains the browser page, its HTTP service, and both sides of
the repo session protocol. It has no dependency on the Clank CLI, a repo
directory, Zellij, Cloudflare, or a particular browser login. The default
host in `clank-web-cloudflare` embeds it; the
[custom host example](examples/README.md) shows a separate application
with its own listener and credentials.

## Host boundary

Create one `bridge::Session` for each explicit session handle. Route a
WebSocket upgrade for the repo connector to `Arc<Session>::accept`, passing
a host-owned credential check. Mount `service::WebService` under the
`PageOptions::base_path` chosen for the browser page. Its
`BrowserAdmission` implementation must validate the browser's own session
and authorize that specific handle for **every** request. Return `None`
to refuse access. Give each admitted browser a `BrowserLease`; sending
`true` on its watch or dropping the watch sender revokes that browser's
open event stream as well as later requests. Browser and connector
credentials are independent.

`BrowserAuth::Host` supplies the host's login and optional sign-out
paths to the page. The host serves those paths itself and decides how
to establish and revoke a browser login. `WebService` serves only the
mounted page, its assets, event stream, and repo operations; it does
not create a login policy. The default host instead uses
`BrowserAuth::ClankDoor` and keeps Door, device sessions, QR links,
token rotation, and tunnel setup in `clank-web-cloudflare`.

The host owns the listener, public TLS or tunnel, process lifetime, and
any administrative controls. A repo connector credential grants the
right to publish one named session. It must not grant browser access
or host administration. A custom host can expose its own management
page or leave those optional controls unavailable. The TUI's
`web.attach` entry supplies a WebSocket endpoint, handle, connector
credential, and browser URL for an existing host; see the example for
the concrete configuration.

The default child also sends a separate admin capability over its
private readiness pipe. The TUI uses that capability on the child's
loopback `/admin/*` routes to mint one-time login links, read and rotate
the token, and list or revoke browser sessions. The repo connector
credential and browser cookie cannot call those routes. An attached
host grants no administration unless it supplies its own policy.

## Connector and transport

The repo process initiates a versioned WebSocket connection with
`bridge::connect` (or `connect_signaled` when the caller needs an
attached signal). The host upgrades the socket and checks the
handshake in `Session::accept`. A mismatched protocol version,
handle, or credential fails before a repo action can run. A second
live connector for the same handle is refused. Use `wss://` across
computers and a TLS reverse proxy or encrypted tunnel for the browser;
the built-in host uses an authenticated loopback `ws://` connector
behind its own browser tunnel. `WebService` does not own TLS or a
network listener, so a host can integrate it with its existing
connectivity and authentication. The current reference connector
framing is WebSocket; using another framing requires an adapter at
the protocol boundary.

The repo-side `Backend` returns a snapshot and a live subscription
from one ordering source. Its snapshot for the shipped page is
`BrowserFrames`: `replay` holds the current SSE-shaped status, pane,
terminal, and transcript frames; `live` is empty. Later `Update`s
contain `BrowserDelta { live }`. The host merges those deltas into
the retained replay for new browsers. Each update has the next
sequence number; a gap causes the connector to send a fresh snapshot.
On reconnect the host replaces the old image. Browser streams see a
disconnect, and repo actions refuse while no current snapshot exists.

`Backend::dispatch` receives a typed `BrowserRequest` wrapped as an
`Action`; `Backend::upload` receives bounded bytes and media type.
The repo adapter must recheck the current pane, harness session,
roster, and editable resource revision before mutation. A completed
bridge request reports the adapter's `BrowserReply` status and body;
it is not proof that an agent recorded an input. A lost connection
after dispatch has an unknown outcome and is never automatically
retried. Uploads are limited to 12 MiB and sent in 64 KiB chunks;
control messages are limited to 1 MiB.

## Browser route inventory

All paths below are relative to the mount. `WebService` checks browser
admission for every one. `POST` actions and the `/events` WebSocket
also check the browser's `Origin` against `Host`.

| Route | Repo request or service result |
| --- | --- |
| `GET /`, `/whoami`, `/bridge/status` | Page, admitted browser check, connector state |
| `GET /events` | Snapshot replay and live status, pane, terminal, and transcript frames |
| `GET /xterm.js`, `/xterm.css`, `/plate.woff2` | Bundled page assets |
| `POST /say`, `/stop` | `Say` / `Stop`, including expected pane, label, and harness session |
| `POST /queue`, `/stash`, `/agents` | `Queue` / `Stash` / `Agent`, retaining the request's handle and revision fields |
| `GET /agents/candidates` | `AgentCandidates` |
| `POST /blocks/answer`, `/agents/reopen` | `BlockAnswer` / `Reopen` |
| `GET /html/<approved resource>` | `Site`; repo-generated plan, queue, stash, commit, index, or style resource |
| `POST /upload` | `Backend::upload`; the resulting attachment path belongs to the repo machine |

The default host adds login, device-session, `/instance`, tunnel, and
administration routes outside this service. They are host policy, not
repo actions. The child-process test in
`crates/cli/tests/it/web_host_process.rs` exercises the browser route
inventory against a separate host process whose working directory
does not contain the repo fixture. The package tests in
`crates/web/tests/bridge.rs` exercise ordering, limits, and the
connector's disconnect and reconnect behavior.
