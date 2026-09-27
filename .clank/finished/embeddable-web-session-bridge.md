# embeddable-web-session-bridge

## Goal

Extract a reusable Rust crate that lets another application attach to a Clank repo session and host the existing web UI on another computer. That application owns public connectivity and authentication. A process beside the repo, initially `clank tui`, opens an outbound connection, publishes session state and streams, and executes incoming Clank actions locally. Preserve the current browser functionality and make the existing built-in remote consume this architecture in normal use. The production split is two crates: `clank-web` for the protocol, both connection implementations, session server, and web interface; `clank-web-cloudflare` for the runnable default server, its Cloudflare tunnel, and today's browser authentication policy.

**Mandatory architecture: the web server runs in a separate process, including on the local machine.** The two entry paths are (1) start a local server process, which owns the Cloudflare tunnel, and attach the TUI to it, or (2) attach the TUI to an existing server. Both use the same session protocol and dispatch path. There is no separate in-process "remote" implementation or local fast path. The operator explicitly accepts the overhead to ensure the boundary is exercised in everyday use.

"Attach" here means attach a consumer to a repo session. It does not bind an agent label or change who may establish an agent's harness-session binding.

## Mandatory operator review

The operator explicitly requested a planning stage followed by a block before implementation. The design below is the planning deliverable. The operator has now reviewed it, requested the two-crate split and authentication placement below, and explicitly authorized unblocking and continuing. If intro review requires a materially different architecture, update the design and give the operator another review opportunity before implementation.

## Current boundaries and coupling

The code has useful internal boundaries, but it is not presently an embeddable server:

- `crates/cli/src/cli/web/page.html` holds the UI. `page_for` injects session data, composer code, and the default authentication code. The page assumes root-mounted routes, `/token`, `/whoami`, `/login`, and the default sign-out flow.
- `web/tunnel.rs` already defines provider and handle interfaces. Tunnel construction is separate, but `web::Instance::start` still starts the HTTP listener, producers, default `Door`, site builder, and tunnel as one owned lifecycle. The TUI remote controller also requires a configured tunnel.
- `web/door.rs` holds token/cookie admission and revocation. `route`, the WebSocket `Authority`, origin handling, and the page's credential code directly depend on it. It is a concrete policy rather than a replaceable host boundary.
- `web::Panes`, `Sayer`, `Stopper`, and `Reopener` provide some injection points. `Panes::subscribe` returns a CLI/Zellij child type, and the other web actions still call CLI and filesystem primitives directly.
- `web/feed.rs` retains status, pane tables, full terminal viewports, and bounded transcript windows. It already handles snapshot-before-live ordering and resynchronization. Its types still refer to CLI types such as `PaneMeta`.
- Transcript discovery and JSONL tailing read the local harness files. Status facts are derived in `status_tui::web_facts`. `Pages`, uploads, queue/stash/roster/block actions, and on-demand commit rendering all assume local repo/home access.

Moving the listener alone would therefore leave most functionality dependent on the original machine's filesystem and TUI.

## Proposed architecture

```text
browser
   | host-owned browser authentication and HTTP/WebSocket service
server process: clank-web-cloudflare OR an existing host on another computer
   | embeds clank-web: protocol server, page/assets, session view, typed backend interface
   | attaches an authorized connector to one explicit session handle
   | ordered bidirectional connection (initiated from the repo machine)
local Clank session backend in clank tui
   | repo actions, status derivation, transcript adapters, generated pages
   | Zellij subscriptions, agent input/interrupt, uploads and pane reopening
repo + harness files + agent panes
```

For the local entry path, the TUI launches the server child, waits for a structured readiness result, then connects to its loopback endpoint. The child owns its listener, browser authentication, Cloudflare tunnel, and browser connections. The TUI supplies repo data and performs repo/agent actions through the connection. For an existing server, the TUI uses the configured endpoint and connector credentials. After connection establishment, the two paths are identical.

Add `crates/web` (`clank-web`) with a public session/backend API, shared wire types, client and server protocol implementations, retained view state, the bridge implementation, and the embeddable page/HTTP service. Keep these as distinct modules with a one-way dependency from HTTP presentation to the session API. Add `crates/web-cloudflare` (`clank-web-cloudflare`) as the runnable default host that embeds `clank-web`, owns the HTTP listener, Cloudflare tunnel, default login/token/device policy, and server process lifecycle. These are two separate crates from the start; split a protocol-only third crate only if the dependency audit shows a concrete need. `clank-web` must not depend on `clank`, `clank-web-cloudflare`, or private CLI/TUI modules. It may use suitable types from `clank-core` without moving filesystem or harness operations into that core.

The CLI provides a `RepoSession` adapter implementing the connector side of the public interface. It owns all repo and harness access. Every production web server uses a session binding backed by the protocol connection, including the server child on the same machine. The UI and HTTP handlers have one set of behavior for both deployments. Locality must not grant the server direct access to `RepoSession`, the repo filesystem, harness files, or Zellij. Unit tests may use protocol fixtures; an in-process adapter must not become an alternate shipping path.

The public API must include a session handle, an atomic initial snapshot plus ordered live updates, asynchronous typed requests/results, attachment and generated-asset transfer, and an explicit closed/disconnected state. A host can keep several handles in its own registry; every operation is scoped to one handle, with no ambient "current repo".

## Placement and full functionality

| Surface | Repo process responsibility | Host/crate responsibility |
| --- | --- | --- |
| Repo status, timeline, team facts, Chat availability | Derive from current repo and binding state | Retain and publish snapshots/updates to the page |
| Terminal | Discover panes, subscribe, send complete viewport updates and closure/geometry changes | Fan out bounded retained state; preserve existing terminal rendering |
| Chat | Discover and tail Claude/Codex/Grok JSONL, parse turns, preserve session/generation/reset identity | Retain/render normalized turn snapshots and live updates |
| Say and Stop | Revalidate the target; invoke the correct harness input/interrupt operation | Carry typed requests/results and retain existing pending-message UX |
| Uploads | Validate and store bytes locally, return the resulting local attachment reference/path | Transfer bounded bytes; never substitute a path on the host machine |
| Queue and stash | Existing add/open/save/promote/unqueue/priority and open/restore primitives | Expose their current controls and structured refusals |
| Team/session controls | Existing roster, library candidates, auto-mode, clear-session, and pane-reopen primitives; reopen stays in the TUI reconciler | Expose the current controls and preserve stale-session validation |
| Blocks | Existing shared answer validation/write and normal unblocked wake | Show pending questions, send answer and expected question, show refusals |
| Plan/commit/stash pages and assets | Generate/read pages and render missing historical commit pages locally | Fetch authorized named resources, preserve relative links and content types; cache only with a defined revision |
| Browser login/logout and public transport | Initiate attachment; present authorized server-management results in the TUI | Server owns its policy; default Clank server retains today's Door and Cloudflare tunnel behavior |

JSONL remains a local input to the backend. The connection carries the normalized transcript events the UI already consumes, so a host needs no harness installation or access to `~/.claude`, `~/.codex`, or `~/.grok`. Raw JSONL forwarding is not required for current feature parity.

## Connection and lifecycle contract

Use a versioned protocol with a documented async stream/sink interface for bounded messages. The reference connector uses WebSocket, with tagged JSON control messages and bounded binary chunks for bulk transfer. Use authenticated loopback WebSocket for the locally started server and WebSocket over TLS for a server on another computer; framing, handshake, requests, streams, and error handling are identical. The local connector need not traverse the Cloudflare tunnel: that tunnel exposes the server to browsers. The host can supply a different authenticated/encrypted message transport without changing the backend or UI. Include a working connector adapter and a minimal host example so this is a usable deployment, not only traits. Keep the protocol independent of a particular tunnel vendor or browser cookie format.

The handshake identifies the intended host session/repo handle, protocol version, and supported capabilities. Authentication of that connector and authorization to attach it to a handle belong to the host adapter. An unknown version or mismatched handle fails before commands can run. A second connector for the same handle is explicitly rejected unless the host has deliberately replaced the old binding; superseded connections cannot continue dispatching requests.

On connect/reconnect, install a complete authoritative snapshot before forwarding live updates. Reuse the feed's existing ordering/reset behavior. Prefer full resynchronization on reconnect over a durable replay log or new disk counters. A dropped connection marks the session disconnected, invalidates in-flight requests appropriately, and stops accepting new mutations until a fresh connection and snapshot are ready.

Requests carry correlation IDs and the target identity/precondition needed by the operation, including the expected agent session for input and the existing revision checks for editable resources. Validate authority and target again at local dispatch, where the current pane/roster/files are known. Do not send input to a replacement session selected from stale remote state.

Mutating requests are never automatically replayed after a lost connection. Distinguish refused-before-dispatch, completed, and outcome-unknown-after-dispatch. Explain the last state and refresh authoritative state; do not promise exactly-once effects across a process crash. Acknowledging that a request reached the bridge must not be shown as proof that the agent recorded a chat message.

Keep bounded queues and payload limits. Coalesce replaceable terminal/status state or require a resnapshot when a consumer falls behind. Define transcript reset behavior instead of dropping arbitrary deltas. Chunk or stream uploads/assets with cancellation and limits so a stalled transfer cannot grow memory without bound or prevent control operations indefinitely.

Each side owns and joins its tasks/subscriptions. Detaching the TUI stops new dispatch and releases local producers. Host browser-session revocation stops that browser's stream and new actions through a host-supplied cancellation/authorization contract. Disconnecting one browser does not end the shared repo connector; stopping the repo connector affects every browser attached to that session handle.

Specify child ownership independently of attachment. By default, a local server started for this TUI is its owned child: stopping that local service or exiting the TUI detaches, requests server shutdown, waits for tunnel/browser cleanup, and reaps the child with bounded escalation if needed. Attaching to an existing server does not transfer ownership; detaching or exiting must not shut that server down. A local server started independently is also an existing server for this purpose. Failed startup/attachment cleans up a child launched by this attempt. Server/tunnel failures report connection state to the TUI while its repo work and agent panes continue.

Use a private startup/control channel to deliver the local child's readiness, endpoint, and connector credentials rather than exposing secrets in command arguments. Starting the server does not imply that a repo is attached; show readiness and attachment state separately. Reconnect uses the same logic for both entry paths. No server crash or disconnected bridge may fall back to direct in-process dispatch.

## Embedding and authentication contract

Expose an HTTP service/router and assets that an application can mount under a chosen path; the application owns the listener, TLS, routing, and authentication. Browser and connector admission are separate decisions. Carry host authorization/cancellation into both HTTP requests and long-lived browser streams, preserving revocation behavior.

Move the current `Door` authentication implementation, default login/token/QR/device management, tunnel readiness probes, and tunnel-specific connection repair into `clank-web-cloudflare`. That crate supplies its policy to `clank-web`'s host authentication and connector-admission interfaces; other embedding applications supply their own policies. The generic session page and HTTP service stay in `clank-web`, with explicit base-path and authentication/navigation configuration. The default login page belongs to `clank-web-cloudflare`. Host mode must not read/write Clank credentials or call the default token/login endpoints. Test non-root mounting, including WebSocket URLs, uploads, iframe links, and relative generated assets.

Existing TUI server-management controls (opening a browser link, reading the public URL, token rotation, listing/revoking browser sessions) must use an authorized server-control interface rather than sharing a `Door` object or reading the server's files. Distinguish server administration capability from attaching one repo: attachment alone must not authorize revoking another host user's browser sessions. A custom host advertises equivalent supported controls or its own management destination. These controls are optional host policy; the repo session protocol and all repo actions are shared.

Preserve bounded request bodies, origin/CSRF enforcement appropriate to the host, safe generated-resource resolution, and per-session isolation. Authentication replacement does not remove these application boundaries. Specify which responsibilities the host must provide and make omitted admission fail closed rather than silently creating a public control endpoint.

## Implementation milestones after operator approval

1. Establish both crate boundaries: extract the reusable session DTOs, client/server protocol implementations, connector/backend interface, retained state, and web service/assets into `clank-web`; place the default runnable host, authentication, and tunnel in `clank-web-cloudflare`. Remove private CLI types from the public API. Define the protocol and process-ownership contracts with the extraction so the public server API cannot acquire direct repo access.
2. Implement the outbound session bridge and host-side session binding, including snapshot/live ordering, typed action dispatch, disconnect/error states, resource transfer, limits, and cancellation. Route every UI operation through this protocol.
3. Replace the existing in-process remote with a separate `clank-web-cloudflare` server process owning default authentication and the Cloudflare tunnel. Make the existing start workflow launch it and attach the TUI through the same `clank-web` connector used for an existing server. Implement readiness, ownership-aware shutdown, server-management controls, and failure reporting. Remove the old direct server/backend path from production.
4. Provide a small separate host application/example with its own admission mechanism and listener, and expose attachment to an existing endpoint in `clank tui`. Present the model as start a local server and attach, or attach to an existing server; do not introduce parallel "remote" and "attach" architectures. Document embedding and transport/authentication integration.
5. Run feature-parity and failure tests across two actual processes with different filesystem roots, then exercise the normal local-server workflow with the Cloudflare tunnel. Complete API documentation, migration of existing tests, and install the tested Clank build for everyday use.

Use cohesive implementation commits and avoid cosmetic extraction commits that leave two competing implementations. Any milestone split should produce a working, reviewable boundary.

## Acceptance

- Another crate can depend on the public crate and host the current UI without depending on CLI internals, running Zellij locally, possessing the repo, or loading harness credentials/files.
- Starting the existing remote workflow launches a distinct server PID that owns the Cloudflare tunnel. The TUI attaches over the wire protocol; server-side code has no direct repo backend. This is the default production path and is exercised in the local smoke test.
- Attaching to an existing server uses the same handshake, data frames, actions, and reconnect logic as attaching to the locally launched child. Run the same feature-parity suite against both entry paths. Tests must demonstrate that breaking the connection disables actions rather than silently reaching a local adapter.
- A repo-side process initiates the connection to the host. A browser attached to the host sees initial status, existing terminals and Chat history immediately, then live updates. Reconnects do not mix old/new agent sessions, duplicate terminal output, or lose the snapshot/live boundary.
- A route/action inventory demonstrates parity for every surface in the table, including uploads, generated pages, queue/stash/team controls, blocks, and pane reopening. Unsupported capabilities have an explicit reason, rather than silently disappearing.
- Two-process tests exercise input/interrupt, transcript rollover/rebinding, slow readers, disconnect during mutations/uploads, cancellation, and recovery. They observe no automatic duplicate mutation on reconnect and expose unknown outcomes honestly.
- The host example uses its own browser and connector admission. Unauthenticated/cross-session requests fail; revocation closes the affected browser stream. It requires neither Clank's login cookie nor a configured Clank tunnel.
- Non-root mounting works. Generated pages and attachments cross the connection rather than relying on a shared path between machines.
- The local workflow retains browser login, tunnel exposure, revocation, and all UI behavior across the process boundary. Server administration is capability-scoped. Stopping an owned server reaps its tunnel and connections; detaching from an existing server leaves it and its other attachments running. A failed child start leaves no server/tunnel orphan, and a server crash leaves the TUI and agents usable.

## Questions for operator review

The separate-process local server, common attachment protocol, two-crate split, and placement of current authentication in `clank-web-cloudflare` are explicit operator requirements. The operator has reviewed and approved these. Intro reviewers should check the resulting `clank-web` API, local JSONL parsing with normalized events, owned-child versus existing-server lifetime, capability-scoped server management, and reconnect policy of fresh snapshots without automatic replay of mutations. The plan supports multiple session handles in a host but does not add a hosted account/team product or persist repo mirrors on that server.
