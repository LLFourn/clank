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
`BrowserFrames`: `replay` holds structured `BrowserFrame { event, data }`
status, pane, terminal, and transcript frames; `live` is empty. Later `Update`s
contain `BrowserDelta { live }`. The host merges those deltas into
the retained replay for new browsers. Each update has the next
sequence number; a gap causes the connector to send a fresh snapshot.
On reconnect the host replaces the old image. The last complete image remains
available on disconnect; repo actions refuse until a new connector supplies its current snapshot. Browser frames
remain SSE-shaped text, with JSON encoded once at the host boundary.

`Backend::dispatch` receives a typed `BrowserRequest` wrapped as an
`Action`; `Backend::upload` receives bounded bytes and media type.
The repo adapter must recheck the current pane, harness session,
roster, and editable resource revision before mutation. A completed
bridge request reports the adapter's `BrowserReply` status and body;
it is not proof that an agent recorded an input. A lost connection
after dispatch has an unknown outcome and is never automatically
retried. Uploads are limited to 12 MiB and sent in 64 KiB chunks;
only the pre-authentication JSON handshake is limited to 1 MiB.

Protocol 2 keeps the JSON hello/ready bootstrap so old peers receive an
explicit upgrade refusal. Connector and host must be upgraded together;
protocol mismatches stop connector retries. After admission, logical
messages use a shared wincode schema and Begin/Data/End transfers. Data
blocks contain at most 64 KiB; that is a buffering unit, not a maximum
snapshot, update, command, or result size. The codec checks variable lengths
against bytes actually staged, without wincode's default allocation ceiling.

Each direction owns one transfer. Encoded messages up to one block stay in
memory; larger ones spill to unnamed temporary files with block-sized
buffers. Codec jobs run off the async socket driver and are joined on
cancellation. No worker or polling loop runs while the connection is idle.
A host publishes only after End and complete decoding; offsets, transfer
IDs and sequence numbers prevent incomplete or reordered bytes from replacing
its image. Publication acknowledgements limit the connector to one update
in flight; broadcast lag replaces the backlog with a fresh atomic snapshot.

Encoded staging overhead uses a constant number of blocks; spill files scale
with encoded content. Owned source state, decoded candidates, replay merging,
broadcast retention and browser text frames still use memory proportional to
actual content. Chunking does not remove those costs or impose a session cap.
Filesystem or allocation failures remain possible and preserve the last
committed host image. Mutations with unknown outcomes are never replayed.

## Recovery, buffers and upgrading

The browser receives `connection` events with `{ "connected": true|false }`.
An open browser socket is independent of repo connector readiness. During an
outage the host replays its last complete image with `connected: false` and
keeps the browser subscribed; a complete replacement image and `true` arrive
on that same socket after reconnection. The page retains its display and
shows reconnecting. Browser revocation still closes access immediately.

The CLI waits for the first committed image without a total image deadline.
Connection establishment and the small version/admission handshake each have
10-second deadlines before publication begins. Actual startup failures are
reported immediately. Readiness is a watch value (`ConnectionStatus`), not a
queue of old acknowledgements that can overwrite a later disconnect.

The Rust connector and reference hosts use 64 KiB socket read buffers and
flush each transport message (`write_buffer_size = 0`). Their incoming
physical-message/frame bound is the existing 1 MiB pre-authentication hello
bound; admitted transfer bodies are checked separately at 64 KiB. The fixed
17-byte wincode header accompanies each block. No logical publication has
either of those bounds. WebSocket buffer settings apply to the physical
transport and cannot be substituted for a session-size budget.

The browser hop sends one SSE-shaped JSON text frame for each event. The
server library's default incoming bounds (16 MiB per frame, 64 MiB per
message) govern bytes **received from the browser**, not large events sent
by the host. Outbound browser frames have no configured size ceiling. The
pump admits a frame once, then retries its flush while still servicing pings
and revocation. Pending text uses shared `Utf8Bytes`, so a ping does not copy
a whole transcript again. A browser frame, the browser's decoded JSON and DOM
still scale with its content; this is not constant-memory rendering.

Install both matching binaries from the reviewed checkout:

```sh
cargo install --path crates/cli --locked
```

This installs `clank` and its sibling `clank-web-cloudflare`. Restart existing
protocol-1 web hosts/remote instances once so both running peers use protocol
2, then reload an old page to receive the connection-event handler. An
independent embedding host must also update `clank-web`, its structured
`BrowserFrame` adapter and connector upgrade configuration (`websocket_config`).
Version mismatches give an upgrade diagnostic rather than silently reverting
to capped JSON or retrying forever. Session content requires no deletion or
migration. Ordinary subsequent interruptions recover automatically.

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
