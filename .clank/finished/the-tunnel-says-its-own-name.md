# the-tunnel-says-its-own-name

Remote access with no account and no domain — and no remote at all
without one.

> we need it to work without static tunnel domains so you don't need
> to sign up for accounts. ... we should not have the feature work at
> all unless they've set up a "remote" -- this thing where it binds
> to localhost is pretty useless. — lloyd

Rescoped after `the-way-in-is-a-token`: the credential no longer
binds to a hostname, so the auth half of this plan is gone and an
ephemeral URL costs the operator nothing.

## Why

Both providers today demand a fixed public URL in the config, which
means an ngrok account or a Cloudflare domain. That is a signup
between the operator and their own agents. And with no tunnel
configured the switch still serves the page on loopback, which
reaches nobody — the remote exists to be reached from elsewhere.

## The build

**The URL is an output; the CLAIM may still be an input.** An
endpoint is one of two kinds, and conflating them would undo the
exclusion the last plan built (codex on 2a199ca).

- *Claimed* — the provider already owns the hostname and names it
  before it starts: ngrok's `domain`, a command with a configured
  `url`. These keep today's order exactly: lease the canonical
  endpoint BEFORE the connector runs. A second connector against an
  owned endpoint is not refused by the service — a named Cloudflare
  tunnel joins as a replica and the edge begins routing to the wrong
  instance — and a teardown after the refusal cannot undo that
  interval.
- *Allocated* — the hostname does not exist until the tunnel starts
  and is handed one: a quick tunnel, an ssh service's random
  subdomain. These cannot be leased in advance because nobody knows
  the name yet; they lease the hostname the start reported, which is
  safe precisely because a freshly allocated name has no other
  holder.

So `Provider` gains `reservation() -> Option<String>` — the
canonical endpoint when it is known up front — beside a
`start(port)` that now returns the public URL with its handle. The
order is: lease the reservation if there is one → start → learn the
URL → lease it if there was no reservation → probe → up, with the
same teardown-before-release on every failure path
(a-tunnel-is-a-switch-too). A reservation that the started tunnel
contradicts — it reported some other host — is an error, not a
silent adoption. The `Origin` allowlist and the cookie's
`Secure` host are resolved from the URL the running tunnel
reported, still ONE snapshot for the life of the instance.
`TunnelSection::Command`'s `url` becomes optional: with it the
command is claimed, without it the command is allocated and reads
its hostname from the child's first lines against a configured
pattern.

**A provider that needs no account: the quick tunnel.** Native,
through the pure-Rust `cloudflare-quick-tunnel` — an ephemeral
`*.trycloudflare.com`, no account, nothing installed. It was cut
from the last plan because it cannot carry a stream; with the long
poll below it can carry the page, which is what the measurement
settles. The ssh services (`ssh -R 80:localhost:{port}
nokey@localhost.run` and its kind) need no native provider of their
own: they are a `command` whose URL is discovered, which this plan
adds anyway.

**A live channel the edge will carry: the WebSocket.** MEASURED
through a real quick tunnel, against local servers serving each
shape:

| shape | through the tunnel |
| --- | --- |
| finite, immediate (`/instance`) | 200 in 0.16s |
| finite after a 2s wait (a long poll) | 200 in 2.57s |
| open stream (SSE) | nothing in 12s |
| WebSocket upgrade | 101 in 1.92s, first frame at once |

The edge buffers a response BODY until it completes, which is why
SSE delivers nothing and a long poll gets through. A WebSocket is
not a body at all — it is an upgrade, and the edge forwards its
frames as they come. So the live channel becomes a WebSocket: it
survives the edge that motivated this, and unlike the long poll it
is ONE transport for every provider, with no cursor bookkeeping and
no second code path beside SSE to keep honest. `/events` gives way
to a socket carrying the same frames the feed already publishes, in
the same order, with the same retained-state-then-live handoff; a
revoked session closes it as it ends a stream today. Readiness
probes the transport ACTUALLY in use, so "it routes" is never
mistaken for "it streams", and the honest failure stays when the
socket does not survive.

**Auth needs nothing from this plan.** The credential is a token
now, bound to no hostname (the-way-in-is-a-token), so an ephemeral
URL costs the operator nothing: the same pasted token, the same
one-time links, the same sessions. What the start still owes the
door is the public URL itself — the `Origin` allowlist and the
cookie's `Secure` host are derived from it once per start — so a
URL learned at start rather than read from config must reach them
by the same path, and the row's link must be the URL the tunnel
reported.

**No remote without a remote.** With no `remote.tunnel` configured
the loopback fallback is gone: the row reads `not configured`, the
switch refuses with the one line of config to add, and nothing
binds. `clank tui` still runs; only the remote is absent.

## Tests

- A fake provider that reports its URL at start: the lease, the
  cookie host and the `Origin` allowlist all follow the reported
  URL, not config; a start that fails after reporting still tears down before
  the lease is released.
- A discovered-URL command provider: the URL is read from the
  child's output; a child that never prints one fails at the grace
  with its stderr as the reason.
- The socket: the retained state arrives before the live frames, in
  the feed's order; a revoked session closes it; a lagged browser is
  resynced as it is on the stream today.
- The live channel over a transport the fake tunnel buffers: the
  start fails saying so; over one it carries: up.
- An instance on an allocated URL admits the same token and mints
  links against the URL the tunnel reported.
- Unconfigured: the switch refuses, serves nothing, and binds no
  port.
- A claimed endpoint already held: the second TUI starts NO
  connector at all — the refusal precedes the start — and the first
  keeps serving; an allocated endpoint leases the name it was given.
- Mutations: the URL taken from config rather than the start —
  caught; a claimed endpoint leased only after its connector starts
  — caught; an allocated endpoint never leased — caught; the
  loopback fallback still reachable when unconfigured — caught.

## Out of scope

- A long-poll transport: the WebSocket is measured to survive the
  edge that motivated a second transport, and costs no cursor
  bookkeeping, so it is not needed.
- A hub for several repos on one hostname.

## Acceptance

- [ ] with no account and no domain, `remote.tunnel` set to the
      no-account provider brings the page up on a public URL the
      tunnel chose, reachable from a phone, entered by a QR link
- [x] the live channel works through it, proven by the probe, not
      assumed
- [x] a claimed-hostname provider still works exactly as it does
      today
- [x] a claimed endpoint held by another TUI refuses before its
      connector starts; an allocated one leases what it was given
- [x] with nothing configured the remote refuses and binds no port
- [x] tests as above, mutation-checked
