# the-way-in-is-a-token

The remote's door is a token you can read off the TUI and paste.

> let's drop all requirement for yubikey for now. Literally just an
> api token appears in your clank tui that you can paste into the
> website. ... is there any way we could save the credential so it
> can be reused across sessions on the same computer. Like user
> level config. I think the access tokens should be configured on a
> user level. — lloyd

## Why

A passkey is bound to its relying party, which is a hostname. That
binding is the only reason the remote needs a stable public domain —
and a stable domain is the only reason it needs an ngrok account or
a Cloudflare zone. A token is host-agnostic: the TUI shows it, you
paste it, you are in. Ephemeral hostnames stop being a problem, and
`webauthn-rs` and its OpenSSL build dependency leave with the
ceremony.

## The build

**The token is the user's, not the session's.** A random 256-bit
token, minted on first use and kept at
`~/.clank/config.json#/remote/token` — user level, so every repo's
remote on this machine takes the same one and it survives restarts
and reboots. It is shown on the remote page in the TUI and nowhere
else; rotating it there mints a new one and ends every session it
opened.

**The way in is a paste box.** `/login` offers a field. A POST
carrying the token, compared in constant time, opens the same
thirty-day session part 2 already keeps: hashed at
`~/.clank/remote-sessions.json`, listed and revocable from the row,
ending live streams on revocation. The existing per-address rate
limit guards the box, and `/say` keeps its `Origin` check. Paste
once per device; the cookie carries from then on, and when it
expires the same token still works.

**The links stay, as a convenience rather than the mechanism.** `o`
on the row still opens the browser through a one-time link, so the
desktop never pastes; `p` still shows a QR of the URL carrying a
one-time grant, so the phone usually does not either. The paste box
is what makes both optional instead of load-bearing — a phone that
cannot scan can still be told six words.

**What goes.** `webauthn-rs`, `webauthn-authenticator-rs`, the four
ceremony routes, `register.html`, the passkey rows on the remote
page, `remote.passkeys`, and the relying-party resolution. The
cookie's `Secure` flag follows the request's own host rather than a
configured relying party, and the README loses its OpenSSL note.

## Tests

- The right token opens a session; a wrong one does not; past the
  rate limit neither does.
- Rotating the token ends the sessions it opened and their open
  `/events` streams.
- The token is user level: a second repo's remote admits the same
  one, and a fresh process reads the same one back.
- A one-time link still admits once and not twice; every protected
  route still refuses without a session.
- Verification and the session are ONE hold of the store, so a
  rotation cannot land between them; a rotation is one transition,
  writing the sessions before the credential so a half-written
  change refuses too much rather than admitting too much.
- A connector that adds no forwarded header still gets a `Secure`
  cookie when the request arrived at the configured public https
  host; loopback stays plain. The public URL is parsed ONCE and both
  the `Origin` allowlist and that host come from it, normalized as a
  browser normalizes — a default port dropped, the host lowercased —
  so a config spelling `https://Host.Example:443` still admits the
  page's own posts.
- A real-length token is readable whole in a narrow pane.
- Mutations: the token compared by length or prefix rather than
  whole — caught; rotation leaving its sessions open — caught; the
  paste box exempt from the rate limit — caught; verify and open
  split across two holds — caught; `Secure` trusting only the
  forwarded header — caught; the token rendered as one truncating
  row — caught; the `Origin` allowlist keeping the raw URL — caught;
  the cookie host keeping a default port — caught.

## Out of scope

Passkeys and the YubiKey. The relying party goes with them; if a
stable hostname ever makes them worth having again they return as
an addition, never as the requirement.

## Acceptance

- [x] the remote page shows a token; pasting it from a phone gets
      in (the paste box and its session are covered end to end in
      the server round trip; a phone in hand is the manual check)
- [x] the same token works from another repo's remote on this
      machine, and after a restart
- [x] rotating it ends existing sessions
- [x] no webauthn dependency remains, and the build needs no
      OpenSSL (only `openssl-probe`, a cert-store locator, is left
      in the lock, and it is not reachable from clank)
- [x] tests as above, mutation-checked
