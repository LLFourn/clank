# Vendored

Everything the remote's pages load comes from this binary. Nothing here
is modified — each file is the published build, byte for byte, so its
digest is checkable against where it came from, and
`web::tests::the_vendored_assets_are_what_the_notice_says` holds those
digests: a blob nobody can check is a blob nobody should ship.

The reason is the credential. `page.html` keeps the remote's token in
`localStorage`, and any script on that origin can read it and can call
`/say`, which types into agents. A script fetched from somebody else's
host is somebody else's decision about what runs there
(`the-credential-is-not-a-cdns-to-take`).

To update one: fetch the new version, put its digest here and in that
test, and read the diff.

## jsQR — the sign-in page's QR decoder

The token is scanned off the TUI's pane, and iOS Safari has no decoder
of its own (`BarcodeDetector` is Chrome's).

- Package `jsqr` **1.4.0** — <https://unpkg.com/jsqr@1.4.0/dist/jsQR.js>
- Upstream <https://github.com/cozmo/jsQR>, licence **Apache-2.0**
  (`jsQR.LICENSE`)
- sha256 `bc40c8a15196236b2314db0856f72ca0b49980cd5413b8c852a7349f5fee0859`
- blake3 `7e156db57fefffb4ea40e40c88fd63b8e5052eae37b7e47c6652160041a90818`

Served at `/jsqr.js`, on the unauthenticated side of the door because
the page that scans has no session yet, and fetched only when someone
presses `scan` — it is 251 KiB, and most people paste.

## xterm.js — the terminal the page draws panes with

- Package `@xterm/xterm` **5.5.0** —
  <https://cdn.jsdelivr.net/npm/@xterm/xterm@5.5.0/lib/xterm.min.js>
  and `css/xterm.min.css`
- Upstream <https://github.com/xtermjs/xterm.js>, licence **MIT**
  (`xterm.LICENSE`)
- sha256 `4196e242ef1cf4c2adead8d97f4a772a69576076f70b095e004b4abbb049e7bf`
  (`xterm.js`)
- sha256 `f7f724aea2bb620a6482bfb8e4bdecfae1152b0c7facef55fbda61f3b6cfedb2`
  (`xterm.css`)

Served at `/xterm.js` and `/xterm.css`, behind the door: only a page
with a session draws a terminal.

## Martian Mono — the face agent names are set in

The page's `--plate`. The latin subset of weight 700, which is what it
sets: the rest of what Google's stylesheet offers is for text this page
does not have.

- From the Google Fonts build of **Martian Mono v6**, latin subset,
  weight 700 (`css2?family=Martian+Mono:wght@700`)
- Upstream <https://github.com/evilmartians/mono>, licence **OFL-1.1**
  (`plate.LICENSE`)
- sha256 `723569a183da9c0bb9fb829bd3a3fb3c6e044e0ad405a6feb6fce8a45480fb52`

Served at `/plate.woff2`, behind the door, and declared by the page's
own `@font-face` — the stylesheet that used to declare it was
Google's, fetched from Google, which is the arrangement this plan is
about.
