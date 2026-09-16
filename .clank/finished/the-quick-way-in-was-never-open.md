# The quick way in was never open

## What was wrong

`{"provider": "quick"}` — the accountless, binary-free Cloudflare tunnel,
spoken natively in Rust — never came up. The README called it "Start
here" and the unconfigured row told every new user to paste exactly that
line, so the first thing a new user met was the one provider that always
failed.

Measured, this is what the edge hands the connector for a websocket:

```
GET /instance/stream HTTP/1.1
Sec-Websocket-Key: +vKNVs8DZtnNj+E9P07hzA==
Sec-Websocket-Version: 13
Connection: keep-alive
```

No `Upgrade: websocket`. No `Connection: Upgrade`.

This is not Cloudflare being strange. The tunnel is QUIC, and neither
QUIC nor HTTP/2 has an `Upgrade` mechanism — HTTP/2 forbids
connection-specific header fields, `Connection` and `Upgrade` among them,
and replaces the idea with Extended CONNECT. Those two cannot cross that
wire, so websocket-ness travels out of band as the stream's `conn_type`.
The capture is that rule being applied rather than a coincidence: the
fields that vanished are exactly the ones HTTP/2 bans as hop-by-hop, and
the two that survived — `Sec-Websocket-Key`, `Sec-Websocket-Version` —
are ordinary end-to-end fields it permits.

A connector is a protocol translator, and coming back down to HTTP/1.1
means SYNTHESISING what the outer protocol forbade.
`cloudflare-quick-tunnel` implements the HTTP half of that translation
and not the websocket half: it forwards only edge-supplied headers, and
then — seeing no `Connection` — writes `Connection: keep-alive` over the
field it should have rebuilt.

## Why we do not fork the crate

Because we do not have to. The crate's `analyse_response` sets
`is_upgrade` from `status == 101` ALONE, so an origin that answers 101
puts it into its bidi pump whatever the request looked like. Measured
directly against a raw origin that answers 101 off the
`Sec-Websocket-Key`: handshake `101 Switching Protocols` through the
tunnel, and a text frame delivered. **The connector can carry a socket.
It just cannot ask for one.**

So the repair belongs on our side, where a handshake that is complete in
every respect but two lines is put back together.

## Why it has to be on the wire, and why keep-alive has to go

Two findings, both by measurement, both the reason the obvious version
does not work:

1. **Rewriting the headers in the router does nothing.** hyper decides
   whether a connection is upgradeable while PARSING the head. Headers
   added afterwards produce a 101 that never hands over the socket, which
   the peer sees as `Connection reset without closing handshake` — the
   exact symptom. The repair must happen in the bytes, before hyper.
2. **The handshake is usually not the first request on its connection.**
   Since the request looks like plain HTTP, the crate's pool reuses the
   socket from the preceding `GET /instance`, and a repair that reads
   only the first head never sees it. With `keep_alive(false)` each
   request arrives alone and the repair always applies.

The cost of (2) is one loopback TCP connect per request, which is tens of
microseconds and only ever on the connector-to-origin hop. The
alternative — a framing-aware rewriter that tracks request boundaries
across a keep-alive connection — means a partial HTTP parser in front of
hyper that could corrupt a `POST /say` body containing `\r\n\r\n`. Not
worth it.

## Result

Three consecutive live runs of the NATIVE provider, end to end, probe and
websocket included:

| run | time to up |
|-----|------------|
| 1   | 10.9s      |
| 2   | 10.2s      |
| 3   | 10.0s      |

No fork, no vendored copy, no `cloudflared` binary.

## Deliverables

1. **`restored_head`** — a pure function over the request head: when a
   head carries `Sec-Websocket-Key` and `Sec-Websocket-Version` but no
   `Upgrade`, drop whatever `Connection` the connector invented and put
   `Connection: Upgrade` / `Upgrade: websocket` back. `None` when there
   is nothing to do, so the common path copies nothing.
2. **`repaired`** — reads a connection's first head, bounded, applies the
   repair, and hands back a stream that replays it.
3. **`keep_alive(false)`** on the server, with the reason recorded.
4. Keep `{"provider": "quick"}`, the README bullet, and the unconfigured
   help exactly as they are. They were right all along; the provider was
   what was broken.

## Tests

- `restored_head` on the REAL captured head (recorded verbatim in the
  test) yields a head with both fields, and no `keep-alive` left.
- A head that already has `Upgrade` is left alone — `None`, not a copy.
- A head with the key but no version is left alone.
- A head with neither is left alone.
- A plain `GET /` is left alone.
- The request line and every other header survive the rewrite in order.
- A body containing `\r\n\r\n` is never reached, since only the head is
  read.
- The live native-tunnel test stays, `#[ignore]`d.
- Mutation-check each.

## Out of scope

- HTTP/2 or HTTP/3 to the origin. Both need Extended CONNECT (RFC 8441,
  RFC 9220) for websockets, which is strictly more work than putting two
  header lines back, and the connector chooses the origin protocol
  anyway. The leg we control is loopback, where QUIC's advantages —
  loss recovery, migration — are worth nothing.
- Sending the same fix upstream. Worth doing, and independent: this plan
  stops depending on it.
