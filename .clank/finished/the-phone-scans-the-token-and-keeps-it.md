# the-phone-scans-the-token-and-keeps-it

Getting back into the remote on a phone is one scan, and then none.

> "The 'paste a token' page needs the ability to access the camera so
> it can scan the token on the token page in the tui which should also
> be available as a QR. scanning the token page should be enough to
> reauthenticate."
>
> "is there [any] way to save the token in local storage so that upon
> first opening the link with the scan to phone the next time you open
> the page on the phone's browser it auto logs you through"
>
> "the url is stable enough for our purposes. It's a cloudflare tunnel
> that stays open for a long time." — lloyd

## Why

The token is a 43-character string shown on the TUI's token page, and
the only way it gets into a phone is thumbs. The minted link (`p` on
the remote page) avoids that once, but it is one-time and five
minutes; when the session cookie is gone the phone is back to typing
a credential off a terminal.

Everything needed to fix this is already here: the `qrcode` crate
renders the link QR the overlay draws (`remote::qr_lines`), and
`/login` already takes the token as a POST behind a per-address rate
limit and a same-origin check. What is missing is the token as
something a camera can read, a camera on the page that reads it, and
a page that does not ask twice.

**The token in the browser's store is the point, not an oversight.**
The session cookie is `HttpOnly` and thirty days; the stored token is
script-readable and forever. That is the trade the ask names: over a
hostname that stays put, reopening the URL signs itself in rather
than asking for a paste, past the cookie's thirty days and past a
cleared cookie. The origin serves nothing but clank's own page and
the decoder vendored beside it, and rotation on the token page is the
revocation — it already ends every session the token opened. A
session can ask for the token, which is a session's thirty days
becoming forever; that is the ask, and it is why `/token` answers
only a request that already holds one.

## The build

**The token page shows a QR.** Under the token's text, `qr_lines` of
the token itself — the same half-block cells the link overlay draws,
from the `qrcode` crate already in the tree. It encodes the bare
token, not a URL: a QR that logs anyone in is a photograph away from
being the credential, and this one is only worth something to a page
that already has the box. A pane too narrow for the QR keeps the text
and drops the cells (the page scrolls; it must not wrap into noise).

**The sign-in page scans it.** A `scan` button beside the box opens
the rear camera (`getUserMedia`, `facingMode: environment`), draws
frames to a canvas and decodes with jsQR; a decoded token goes
through the same POST the paste does. The button is there only in a
secure context with a camera (the tunnel is https; loopback counts),
and a refused permission says so on the page rather than hanging on a
black rectangle. The camera stops on decode, on cancel and on hide.

**jsQR is vendored.** `BarcodeDetector` is Chrome-only — iOS Safari
has no decoder at all, and iOS is the phone this is for. jsQR 1.4.0
lands beside `login.html` with its licence and a notice saying where
it came from and what it hashes to (**Apache-2.0**, and 251 KiB — not
the MIT 43 KiB this plan first claimed; there is no minified build
published). It is served from the binary at `/jsqr.js` on the
unauthenticated side of the door, since the sign-in page needs it
before it has a session, compressed to 56 KiB for callers that take
it so and fetched only when someone presses `scan` — most people
paste, and the sign-in page is the one thing between them and in. It
is the only decoder path so there are not two.

**Being signed in is what hands over the token.** Not the box: the
scan-to-phone link is the way a phone usually gets in, and it is
redeemed server-side — a five-minute one-use ticket, spent for a
cookie, redirected to `/` — so a page that only remembered what its
own form posted would leave exactly the phone the ask is about with a
cookie and nothing else. Instead `/token` answers a request that
already holds a session with the token, and the remote page fetches
it on load when its store is empty. Every door — the minted link, the
box, the scanner, the desktop's `o` — ends at a page with a session,
so every door provisions the phone through one path.

**The page remembers it.** The fetched token is written to
`localStorage`. On load, a sign-in page with a stored token and no
session posts it and goes to `/`, so reopening the URL is not a form
at all. A stored token the door refuses is wiped and the box is shown
saying so, which is what a rotation looks like from the phone.

**Forgetting is signing this device out.** An empty store cannot say
both "never provisioned" and "asked to be forgotten": the page
provisions itself whenever it loads signed in with nothing stored, so
a store cleared under a live cookie fills again on the next reload
and the device cannot stay forgotten. So the page's `forget` clears
the store AND ends this session — `POST /forget`, same-origin, the
door closing the session the cookie names and the response expiring
that cookie — and lands on the sign-in page. A signed-out phone
cannot ask `/token`, so nothing re-provisions it; remembering resumes
on the next deliberate sign-in through any door. The TUI's revocation
stays rotation, which ends every device at once.

## Tests

- The token page draws the token's QR, and a narrow pane draws the
  token without it.
- `/jsqr.js` is served without a session, with a script content type
  and an immutable cache, compressed for a caller that takes it that
  way and whole for one that does not; the sign-in page asks for that
  path and scans with what it gets.
- The vendored blob is the published build: its digest is the one the
  notice beside it names, so an edit fails rather than passing.
- The sign-in page carries the scan control and the remember hooks
  (the asset and the page ship together or neither works).
- The handoff, from the door the phone actually uses: with no store at
  all, redeem a minted scan-to-phone link, ask `/token` with the
  cookie it set, drop that cookie, and the token that came back opens
  a new session. The same with no session at all is refused, so the
  token is not a thing the door gives away.
- A forgotten device stays forgotten: pair through the minted link,
  forget, and the session that pairing opened admits nothing — so a
  reopened page is the sign-in box, not a page that quietly asks for
  the token again. It takes signing in again to be remembered, and
  then it is.
- A stored token that the door refuses leaves no stored token, and a
  rate limit or a store failure leaves it alone. The door's half is
  tested: only a refusal of the CREDENTIAL carries the mark the page
  discards on. The page's own half is read and walked on a phone —
  this repo has no JS runtime, so there is nowhere for that test to
  run, and giving it one is `the-pages-logic-can-be-tested` rather
  than a toolchain decision smuggled in here.
- A session the store no longer keeps is handed no credential, checked
  and read under one hold of the lock, so a rotation in another TUI
  cannot land between the two and hand the new token to the session it
  just revoked.
- The existing door tests stand: the same token, posted again after a
  cleared cookie, opens a new session; a wrong one does not; the rate
  limit still bites.

Manual, once, on a phone: scan the TUI's QR into the page, close the
browser, reopen the URL, land signed in.

## Out of scope

- The ephemeral hostname. A quick tunnel's name changes each start and
  takes cookie and store with it; that is a stable-hostname question
  (`remote.tunnel.url`), not this plan's.
- Any second credential. The token stays the one way in.
