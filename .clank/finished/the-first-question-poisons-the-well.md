# the-first-question-poisons-the-well
# The first question poisons the well

## The problem, measured

A cloudflare quick tunnel never comes up on this machine. The user sees:

```
not reachable through the tunnel: error sending request for url
(https://stuffed-allocated-energy-mentor.trycloudflare.com/instance): client error
(Connect): dns error: failed to lookup address information: nodename nor servname
provided, or not known
```

while `cloudflared`'s own log says the connector registered fine, and then, 30
seconds later, `Initiating graceful shutdown due to signal terminated` — our
grace expiring and tearing down a tunnel that was working.

Measurements on this machine, against fresh `*.trycloudflare.com` hostnames:

| what was asked                                          | when   | answer         |
|---------------------------------------------------------|--------|----------------|
| `dig @9.9.9.9` the new host, polled from t+4s            | t+8s   | 104.16.231.132 |
| `getaddrinfo` the same host, first call, same instant    | t+8s   | NXDOMAIN       |
| `getaddrinfo` a new host, first call delayed to t+34s    | t+34s  | resolved       |
| `getaddrinfo` a new host, first call at t+4s, every 10s  | t+175s | still NXDOMAIN |

And the reason the fourth row never recovers:

```
trycloudflare.com. 1800 IN SOA kevin.ns.cloudflare.com. ... 604800 1800
                                                                  ^^^^
```

The zone's SOA minimum is **1800 seconds**. That is the negative-cache TTL. One
question asked before the record exists caches NXDOMAIN for thirty minutes, for
every process on the machine, and no amount of retrying reads the network again.

## The model that is wrong

`probe()` asks one question — "does this tunnel carry traffic to me?" — and
expresses it as "retry `GET /instance` until it answers or the grace runs out".

That folds two different waits, with two different failure modes, into one retry
loop:

- **Does this name exist on the public internet yet?** It does not, for the
  first several seconds of a freshly allocated tunnel. This is a wait.
- **Does the edge route that name to this process?** This is the actual probe.

The loop is not merely imprecise about which one failed. It is actively harmful
for the first: its very first attempt goes through `getaddrinfo`, which poisons
the OS negative cache for 1800s, and every later iteration then reads that cache
instead of the network. **The retry loop cannot win a race it has already lost by
entering it.** Raising `TUNNEL_GRACE` cannot fix this; a 30-minute grace would.

Note the third and fourth rows together: the poison is the whole difference. The
name was fine both times.

Note also row two. Even once the record is live at the upstream resolver, the OS
resolver can still answer NXDOMAIN at that same instant — so "wait until it
resolves, then ask normally" is not a fix either. It only narrows the window in
which we poison the well.

## The second measurement: whose answer was missing

Asking three resolvers about one fresh host at the same moments:

| t     | authoritative (`kevin.ns.cloudflare.com`) | `1.1.1.1`      | `9.9.9.9` (this machine's) |
|-------|-------------------------------------------|----------------|----------------------------|
| t+4s  | —                                         | —              | —                          |
| t+14s | 104.16.230.132                            | 104.16.231.132 | —                          |

and then, pinned to an edge address at t+15s, the tunnel answered
`GET /instance` with our JSON and `GET /instance/stream` with
`HTTP/1.1 101 Switching Protocols`.

**The tunnel was working the whole time.** Nothing about the edge, the connector,
or our server was wrong. The record existed at its origin within ten seconds. The
only component that did not have it was the recursive resolver this machine is
configured to use — and on other runs Quad9 still did not have the name after
156 seconds, while the authoritative server had it at t+14s.

So a resolver of our own is necessary but not sufficient: a resolver built from
system config asks Quad9, and inherits exactly this lag.

## The model that is right

**The probe resolves the host itself, once, with its own resolver, and pins that
address for every connection it makes.** `getaddrinfo` is never called for a
tunnel hostname at all, so there is no OS cache to poison and none to read.

This also makes the two waits distinct, which the error messages have wanted all
along: "the name never appeared in DNS" and "the edge would not route to us" are
different sentences, and only one of them means the tunnel is broken.

## Deliverables

1. **`hickory-resolver = "0.24"`** as an explicit dependency of `crates/cli`. It
   is already in `Cargo.lock` beneath `cloudflare-quick-tunnel`; name the version
   we rely on rather than inheriting it from a transitive edge.

2. **The invariant, named once and testable.**

   > **A name that did not exist a moment ago is never sent to a caching
   > resolver — for any record type.**

   RFC 2308 section 5 caches a negative answer by name and class, *not* by
   type, so an `NS` query for the fresh leaf burns it for `A` exactly as an
   `A` query would. The first draft of this plan started its zone walk at the
   leaf and called that safe. It is not, and it would have reintroduced the
   bug in the code meant to fix it — including for the browser, which is the
   actual product.

   Express the two paths as two different operations so the invariant is a
   thing code can be tested against rather than a thing a comment claims:

   - `recursive(name, type)` — through the machine's configured resolver.
     Caches, and its negative answers outlive the tunnel. Only ever asked
     about **ancestor** names and **nameserver** names.
   - `direct(servers, name, type)` — straight at given authoritative servers.
     Authoritative servers answer from the zone, not from a cache, so there is
     no negative cache between us and the origin. This is the only path the
     ephemeral leaf is ever asked on.

3. **`async fn address(host: &str, port: u16, deadline) -> Result<SocketAddr, String>`**
   in `crates/cli/src/cli/web/tunnel.rs`.

   A name created seconds ago is only guaranteed to exist at its origin;
   recursive resolvers catch up on their own schedule, and as measured above
   that schedule can exceed any grace we would be willing to wait. For an
   ephemeral tunnel hostname the origin is the only honest thing to ask.

   - A host that parses as an `IpAddr` returns immediately, with no resolver
     and no network. This is what every existing test uses
     (`http://127.0.0.1:<port>`), and it keeps them offline.
   - **Find the zone starting at the leaf's parent**, never the leaf:
     `abc.trycloudflare.com` → ask `recursive("trycloudflare.com", NS)`, and on
     no answer strip another label and ask again. Every name asked here is a
     proper ancestor of the leaf, so it pre-dates this start by definition and
     caching it is both safe and desirable.
   - Resolve those nameserver names with `recursive`. Also stable names.
   - Poll `direct(those servers, leaf, A)` until it answers or the deadline.
   - A **referral** (authority-section `NS`, no answer) means the leaf is
     itself a delegation point: follow it with `direct` against the referred
     servers, bounded to a small number of hops. Still never `recursive`.
   - A **CNAME** answer is followed by asking `recursive` about the *target*,
     which is a different name and, being shared infrastructure rather than a
     per-start mint, is not the name this invariant protects. Note the bound
     explicitly rather than pretending the case cannot arise.

4. **No fallback to the recursive path for an ephemeral name.** The first draft
   fell back to a system-config resolver "if discovery fails", which restores
   precisely the path this plan rules out. It is also the worse trade: failing
   to bring up one tunnel costs the user a retry, whereas burning the name
   costs them thirty minutes *and breaks the browser they were about to use*.
   On discovery failure, report `NoName` and stop.

5. **Scope the authoritative path to names that are actually ephemeral.**
   `Tunnel::up` already distinguishes a **claimed** endpoint (reserved before
   the connector runs — an ngrok domain, a configured `url`) from an
   **allocated** one (minted by this start). Only allocated names carry the
   hazard. A claimed name pre-dates the start, so the ordinary resolver is both
   correct and safe for it, and it keeps us out of CNAME chains and
   split-horizon setups we have no business second-guessing. Claimed endpoints
   keep today's behaviour.

   This is the honest bound on the guarantee: **for allocated names, nothing we
   do can poison the leaf.** For claimed names we make no such claim, and none
   is needed, because the name already resolves.

6. **`probe()` pins the address it resolved**, for both halves:
   - reqwest: `ClientBuilder::resolve(host, addr)`.
   - websocket: connect a `TcpStream` to `addr`, then
     `client_async_tls_with_config` with the `wss://` URI, so SNI and the `Host`
     header still come from the name.

7. **`NotUp::NoName(String)`** — a fourth variant, displayed as *"the tunnel's
   name never appeared in DNS: …"*. Today this failure arrives dressed as
   `NoAnswer`, which reads as "the tunnel is broken" when the tunnel is fine.

8. **`TUNNEL_GRACE` 30s → 60s.** Not the fix, but the measured authoritative
   wait was 8–14s before the probe even begins, and 30s left no room for a slow
   edge behind it.

## Tests

- **The leaf never reaches the recursive path.** A recorder implementing both
  operations logs every `(name, type)` asked of each. Drive a full `address()`
  against a fake zone and assert the leaf appears only in the `direct` log,
  for every type, including `NS`. This is the invariant's test; it is the one
  that would have caught the first draft.
- The zone walk starts at the parent and stops at the first ancestor whose
  `NS` answers.
- A referral for the leaf is followed with `direct`, not `recursive`.
- `address()` returns an `IpAddr` host with no resolver and no network.
- `address()` reports `NoName` when the deadline passes with nothing resolved,
  the message says DNS, and **no recursive query for the leaf was made on the
  way out** — the no-fallback rule.
- A claimed endpoint does not take the authoritative path at all.
- The existing probe tests still pass unchanged against `127.0.0.1`, proving
  the pinning path is what serves them.
- A probe against a host that resolves to a listener on a *different* address
  than the OS would choose proves the pin is honoured rather than decorative.
- Mutation-check each: start the walk at the leaf, restore the fallback, break
  the `IpAddr` fast path, break the pin. Each must fail a named test.

## Acceptance

`clank` brings up a cloudflare quick tunnel and the URL it prints loads on a
phone. A run that fails leaves the machine able to try again at once, rather
than unable to resolve that name for the next thirty minutes — and leaves the
browser able to resolve it too.

## Out of scope

- The `quick` provider's HTTP 426 (the native `cloudflare-quick-tunnel` crate
  does not re-add `Upgrade`/`Connection` to the edge-supplied request head, so
  our server sees a plain GET). Tracked separately; the `command` provider
  running the `cloudflared` binary is the path this plan unblocks.
- Copy-to-clipboard on error overlays. Separate plan.
