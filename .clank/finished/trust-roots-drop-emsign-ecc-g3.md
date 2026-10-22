# Trust roots drop emSign ECC Root CA - G3

## Why

The life assistant relayed a request to remediate the eMudhra CA
incident on this machine (RUSTSEC-2026-0382). The incident has two
supported fixes in the rustls ecosystem:

- `webpki-roots` 1.0.10 removes the root **emSign ECC Root CA - G3**
  (rustls/webpki-roots#133). That is the only anchor that changes:
  121 anchors become 120.
- `rustls-webpki` 0.103.16 makes the verifier reject the distrusted
  intermediate public keys anywhere in a path (rustls/webpki#546),
  including paths a native or custom trust store would otherwise allow.

clank has several TLS clients, and they do not share one root source.

| Path | Roots | Verifier | Fixed by |
|---|---|---|---|
| reqwest 0.12 `rustls-tls` (GitHub API, tunnel probes) in `crates/cli`, `crates/web`, `crates/web-cloudflare` | bundled `webpki-roots` 1.x | rustls 0.23.45 / rustls-webpki 0.103.15 | both patches |
| tokio-tungstenite `rustls-tls-webpki-roots` (wss connectors), same crates | `webpki-roots` 0.26.11, a shim re-exporting 1.x | rustls 0.23 / rustls-webpki 0.103 | both patches |
| cloudflare-quick-tunnel 0.3.1 QUIC client (`quic_dial.rs:123-155`, via quinn) | `rustls_native_certs::load_native_certs()` (the macOS system store) plus embedded Cloudflare roots | rustls 0.23.45 / rustls-webpki 0.103.15 | the verifier patch |
| cloudflare-quick-tunnel's DoT fallback (`edge.rs:159-172`, Hickory 0.24.4) | none at this feature resolution: neither Hickory `native-certs` nor `webpki-roots` is enabled, so its default DoT root store is empty | rustls 0.21.12 / rustls-webpki 0.101.7 | not applicable |

So the system store does reach clank, through the QUIC client. That
client's exposure is closed by the verifier patch, not by the root
bundle. The DoT fallback trusts no roots at all at this resolution, so
it cannot validate any chain, compromised or not. The old 0.101.7
verifier is therefore unreachable for trust here. Moving Hickory to a
newer rustls is not required by this incident.

The other eMudhra anchors that `webpki-roots` still ships (emSign Root
CA - C1, emSign Root CA - G1, emSign ECC Root CA - C3) are not named by
the incident and are out of scope.

## What changes

- `Cargo.lock`: `webpki-roots` 1.0.9 → 1.0.10 (the 0.26.11 shim
  follows), and `rustls-webpki` 0.103.15 → 0.103.16, both through
  `cargo update -p … --precise`. No feature changes, no custom
  verifier and no trust-all.
- A guard test in the clank crate reads `webpki_roots::TLS_SERVER_ROOTS`
  and asserts that no anchor's subject names an incident root: an
  emSign subject whose CN is a G3 root, or one containing `AAA`.
- A test that pins the resolution reads the workspace `Cargo.lock` and
  asserts that every resolved `rustls-webpki` 0.103.x is at least
  0.103.16 and every `webpki-roots` 1.x is at least 1.0.10. It also
  states the 0.101.x entry's classification, so a future change that
  gives the DoT path real roots is noticed.

## Not in scope

- The eMudhra roots the incident does not name.
- Migrating Hickory to rustls 0.23. Its DoT path has no trust anchors
  here.
- Installing. Installing hands every running monitor to the new binary
  at its next agent stop, which counts as activation, so it waits for
  Lloyd's explicit go-ahead. The activation step is `cargo install
  --path crates/cli --locked`; monitors then hand over at their next
  agent stop, with no manual restarts.

## Acceptance

- `cargo tree -i` shows every former 0.103.15 and 1.0.9 user on 0.103.16
  and 1.0.10, and neither old version remains in the lock.
- The guard and resolution tests pass. Each fails when its patch is
  reverted in the lock (mutation-checked).
- The lib, core and web tests covering the TLS clients still pass.
