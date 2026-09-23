# the-credential-is-not-a-cdns-to-take

The page that keeps a credential runs only code this binary shipped.

> "`the-phone-scans-the-token-and-keeps-it` put a long-lived
> credential in `localStorage` on page.html. That page loads
> `xterm.min.js` (and its CSS) from cdn.jsdelivr.net with NO
> `integrity`/`crossorigin`, and the server sends no
> Content-Security-Policy. Any script on that origin can read
> `localStorage['clank.token']`" — ruthless, reviewing 9473655

## Why

The remote page now keeps the user's token in `localStorage` so a
returning phone signs itself in. That was the ask, and the trade was
stated: a script-readable credential in exchange for not typing one.

What was not stated is who gets to run a script there. `page.html`
loads xterm from `cdn.jsdelivr.net` with no `integrity` hash, and the
response carries no Content-Security-Policy, so whatever that host
returns is executed with full access to the origin — including the
token, and including `/say`, which types into agents. A hostile or
compromised response takes the credential, and rotation is the only
remedy, after the fact.

jsQR was vendored, digest-pinned and served from the binary for
exactly this reason, in the same plan. xterm is the same risk with
none of the same care — the difference was not a decision, it was an
oversight, and the credential landing in that origin is what turned it
from unfortunate into urgent.

## The build

**The terminal's code ships with the binary.** xterm is vendored as
jsQR was: the published build, byte for byte, with its licence, its
version and its digest in a notice beside it, served from a route of
this binary's own. Its stylesheet comes with it — a stylesheet on the
page's origin is not neutral either.

**The page says what it will run.** A `Content-Security-Policy` on
the authenticated page and the sign-in page: scripts and styles from
this origin only, no inline handlers beyond what the pages already
carry, nothing from anywhere else. The header is the enforcement; the
vendoring is what makes it possible to enforce.

**Nothing else reaches out.** A test holds the pages against it: a
page that names a host other than its own fails, so the next
convenient CDN link cannot be added without meeting this again.

## Tests

- The pages reference no external origin: not a script, not a
  stylesheet, not a font.
- Both pages are served with a policy that names only this origin, and
  the policy is on the authenticated page and the sign-in page alike.
- The vendored terminal is the published build: its digest is the one
  its notice names, as the decoder's is.

## Out of scope

- The credential's design. Keeping it is what was asked for; this is
  about who else can read it.
- The generated static site under `clank html`, which holds no
  credential and is its own surface.
