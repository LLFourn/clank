# a-tunnel-is-a-switch-too

Part 3 of `remote-access-is-the-tuis-to-grant` (§3): reaching the
remote from a phone. Depends on parts 1 and 2.

> With the tunnel thing I'd rather build support into the Rust
> directly, for Cloudflare or whatever other tunnel services we can
> support. We can support an "execute this binary" one too, but I
> want native support. — lloyd

## What is native, measured on crates.io

- **ngrok**: the official agent SDK, `ngrok` 0.19 — pure Rust, no
  binary; an auth token, a static domain per endpoint (one is free
  per account), TLS at ngrok's edge. A fixed origin, which the
  passkey needs.
- **Cloudflare**: `cloudflared` is Go, and its edge protocol — QUIC
  carrying Cap'n Proto RPC — is documented only by that source.
  `cloudflare-quick-tunnel` 0.3 is a pure-Rust client for it, but
  for QUICK tunnels only: no account, and a new `*.trycloudflare.com`
  hostname every start — and Cloudflare documents that quick
  tunnels do not carry Server-Sent Events: the edge buffers the
  response, and `/events` is how the page is live (codex on
  edcac42). Not a provider for this remote, natively or otherwise.
  Named tunnels (credentials, a fixed hostname) carry SSE, and the
  crate explicitly does not do them; the connection protocol is the
  same, so registering with a named tunnel's credentials is a
  contribution to that crate or a fork — a spike, not a promise.
  Until then a named Cloudflare tunnel runs through `cloudflared`
  under the `command` provider.
- **Tailscale**: the pure-Rust implementations on crates.io are
  work in progress. Not yet.

## The design

**A `Tunnel` is a provider.** One trait — `start(port) ->
Public { url }`, `stop()` — with two providers, chosen and
configured at user level under `remote.tunnel`, each with a FIXED
public URL: the origin the passkeys, the cookie's `Secure` host
and the `/say` `Origin` check are bound to, as part 2 binds them.

- `ngrok` — native, through the SDK: `authtoken` (or the
  `NGROK_AUTHTOKEN` environment), `domain`. The first-class native
  provider: a fixed origin, TLS, nothing to install.
- `command` — execute a binary: `run` (argv, `{port}` substituted),
  `url`. cloudflared with a named tunnel, `tailscale serve`, `ssh
  -R` — anything that forwards a fixed hostname to the port and
  passes a streaming response through.

**Up means reachable, proven end to end — and live.** Whatever the
provider, the server answers `GET /instance` with a nonce minted at
this remote's start, and the row reads `starting…` until the TUI
fetches `<url>/instance` through the public URL and reads its own
nonce back, then opens `<url>/instance/stream` — an event stream
that says the nonce and stays open — and receives that first event
through the tunnel: routing AND the streaming transport the page
lives on, proven before the URL is on the row and `o` opens it.
Another nonce: another instance behind the hostname, said on the
row. No answer, or no event, within the start's grace: the
provider's reason (the SDK's error; the child's stderr), or "the
tunnel does not carry event streams". Both probe routes answer
without a session and carry nothing of the page.

**One tunnel, one holder.** A provider's fixed identity — the ngrok
domain, the command's `url` — is claimed before it is started: a
user-level lease (`~/.clank/remote/<identity>.lock`, flocked,
holder recorded, the status lease's mechanism), released when the
tunnel is stopped. A TUI that finds it held says whose it is and
starts nothing. Cloudflare runs a second connector to a named
tunnel as a replica rather than refusing it, which is why the lease
is not optional for the command provider either.

**The door follows the tunnel.** Part 2 reads the relying party's
origin and the phone link's base from `remote.url`; with a
provider configured, that URL is the provider's fixed one — the
ngrok domain, the command's `url` — and `remote.url` is not set
separately. The cookie's `Secure` and the `/say` `Origin` check
follow the same host, as part 2 already does.

**Bounded, whole, one endpoint, one snapshot.** The whole start —
the provider's connect, the probe, the cleanup of what was started
— runs under one grace and one abort: a provider that stalls fails
at the grace, and quitting the TUI abandons it at once rather than
waiting the grace out; a stop is bounded too, past which the handle
is dropped (an SDK session closed, a child killed). The command
provider runs in its own process group and stop ends the group BY
NAME — TERM, then KILL if any member is left, whatever the leader
did — so a wrapper's forwarding child goes with it even when it
ignores TERM, and the stderr reader is aborted and joined at its
bound rather than left behind. The native provider runs on a
runtime of its OWN: the SDK detaches a task per connection and
spawns unlisten work on drop that keeps a session clone, so a stop
that aborted only the accept task would leave them alive. A stop
asks the graceful close for a bound and then drops that runtime,
which cancels every task the connector left. Cancellation is owned
through construction, not only after: the thread races the build
against the signal, and the connector handle is held across
readiness, so a start abandoned mid-connect — the grace elapsed,
the TUI quitting — tears the runtime down on the dropped future's
drop before the lease is released, never after a handle is merely
dropped (codex on f248156, 22eed48, c776ad1). The lease is keyed by the endpoint the URL
names — host and port, lowercased, path and trailing slash aside —
so ngrok's `clank.example.com` and a command's
`https://clank.example.com/` are one lease. And the public origin is
resolved once per start and handed to the relying party, the
cookie's host and the links alike: a domain changed while the
remote is on is the next start's, and a phone link is minted from
the running tunnel's proven URL (codex on 6f60efa).

**Owned by the remote.** The tunnel handle — an SDK session or a
child — lives in the `Remote` of part 1 and ends with it: switched
off, or the TUI closed, the public URL is gone within the stop.

## Tests

- The provider trait against a fake: readiness from the nonce
  through a fake public endpoint (right nonce → on; wrong nonce →
  another instance named; no answer within the grace → failed with
  the provider's reason; a nonce that answers but a stream that
  does not → failed, saying the tunnel does not carry streams).
- The lease: held → refused naming the holder; released on stop.
- The command provider: the child ends with the remote (a shell in
  the binary's place, as the remote's own tests do).
- The ngrok provider against the SDK's offline surface (session
  builder, endpoint config) — the live path is a manual check with
  a token, named in the plan's verification, never a test.
- A provider that never finishes connecting: failed at the grace;
  abandoned at once by the exit path. A wrapper that does not exec:
  its forwarding child gone with the stop; one whose child ignores
  TERM: ended by the group's KILL. The SDK's close stalling, with a
  connection task and a nested worker already spawned: all of them
  gone with the runtime, within the bounds. A start abandoned
  mid-connect, a nested worker already spawned: the runtime gone
  before the caller proceeds. Endpoint
  aliases and the two providers naming one host: one lease.
- Mutations: readiness taken from a live handle — caught; the
  stream probe skipped — caught; the lease not taken — caught; the
  connect unbounded — caught; the abort ignored — caught; only the
  spawned process signalled — caught; the lease keyed by the raw
  URL — caught; KILL only when the leader outlived TERM — caught;
  the graceful close not bounded (the runtime never dropped) —
  caught; the runtime not waited for on stop — caught; the build not raced
  against cancellation (an abandoned connect never torn down) —
  caught; the abandoned runtime detached rather than joined —
  caught.

## Out of scope

- Cloudflare quick tunnels: no SSE at the edge, so no live page.
- Named Cloudflare tunnels natively (the spike, once measured);
  until then, `cloudflared` under `command`.
- A hub for several repos on one hostname; TLS of clank's own.

## Acceptance

- [ ] `remote.tunnel` configured as `ngrok` brings the remote up on
      the fixed domain with nothing installed (the live path: a
      token and a domain in hand; the SDK's builder and the token's
      sources are what the tests hold); as `command` on whatever
      the binary forwards — [x] in the tests, a shell in the
      binary's place
- [x] the public URL shows only once the TUI has reached itself
      through it, nonce and event stream both; a held identity is
      named, not shared
- [x] the tunnel ends with the remote
- [x] tests as above, mutation-checked
