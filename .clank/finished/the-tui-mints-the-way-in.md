# the-tui-mints-the-way-in

Part 2 of `remote-access-is-the-tuis-to-grant` (§4): who gets in.
Depends on part 1 (the remote is the TUI's).

## Why

Nothing authenticates the page, and `/say` types into agents. The
door is a passkey — the YubiKey as the phone's — and the TUI, which
whoever holds the terminal holds, mints the links that open it.

## The build

**Sessions.** A random 256-bit id in an `HttpOnly; Secure;
SameSite=Strict` cookie; the record at user level
(`~/.clank/remote-sessions.json`: id hash, passkey name, created,
last seen), 30 days. Every route but `/login` and the registration
page requires one — the page, `/events`, `/say`, `/html/…`; `/say`
also checks `Origin`. Each `/events` connection holds its session
id and its deadline; revocation or expiry closes those connections
— expiry by the stream's own timer, revocation by a watch the door
bumps when the live set changes. The authority is the user's, not
one TUI's: every TUI reads and writes the same two files under one
lock (`~/.clank/remote.lock`), keeps no copy, and re-reads them
every two seconds while its remote is on, so a revocation in one
TUI is refused by the next request in another and ends its
streams. A mutation that cannot be written is an error to whoever
asked, never a success in memory. Login attempts rate-limited per
source; constant-time comparisons; nothing secret logged. `Secure`
is set when the request's host is the configured tunnel's https
host, not on plain loopback.

**One-time links.** The TUI mints tokens (random, 5 minutes,
consumed on use) for two doors: `o` on the `remote` row opens the
URL with a login token that sets a session (the desktop needs no
ceremony); Enter on the row opens the remote page, where `p` mints
a registration link, shown as text and as a QR code of half-block
cells in an overlay, for the phone. (`k` was the plan's key; it is
vim's up everywhere in the TUI, so the page's is `p`.)

**Passkeys.** `webauthn-rs` as the relying party; RP ID and origin
from the tunnel's configured `url` when there is one, else
`localhost`. Registration from the minted link stores the
credential at `~/.clank/config.json#/remote/passkeys` (id, COSE
key, sign count, name, added). `/login` runs the assertion against
the stored passkeys; success sets the session. The remote page
lists passkeys and sessions and Backspace revokes either — by the
identity the row was drawn with (credential id, session hash), so
a list that changed under the page revokes what was shown or
nothing. A stream's authority governs its sends as well as its
waits: a forwarder blocked on a browser that stopped reading still
ends on revocation, deadline or cancel, and what it had queued is
not delivered.
`webauthn-rs` links the system OpenSSL; the README says so under
Install.

## Tests

- Sessions: an unauthenticated request to every protected route is
  refused; a session cookie admits; a revoked session's open
  `/events` stream ends; `/say` without a matching `Origin` is
  refused; a login attempt past the rate limit is refused.
- Links: a minted token admits once and never twice, and not after
  5 minutes.
- Passkeys: registration and assertion round-trip against
  `webauthn-rs`'s software authenticator in tests; a credential from
  another RP is refused.
- Mutations: the stream not closed on revocation — caught; a token
  admitting twice — caught; `/say` without the `Origin` check —
  caught.

## Out of scope

The tunnel itself (part 3); passkeys on other people's devices.

## Acceptance

- [x] nothing but `/login` and the registration page answers
      without a session
- [ ] the YubiKey registers from a TUI-minted link and logs in on
      the phone (a device in hand: the software authenticator
      round-trips in the tests); `o` logs the desktop in without a
      ceremony
- [x] sessions and passkeys are listed and revocable from the row,
      and a revocation ends the stream
- [x] tests as above, mutation-checked
