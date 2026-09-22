# Vendored: jsQR

`jsQR.js` is a third-party QR decoder, kept here because the sign-in
page scans the token off the TUI's pane and iOS Safari has no decoder
of its own (`BarcodeDetector` is Chrome's). Nothing else in clank uses
it, and nothing here is modified — the file is the published build,
byte for byte, so its digest is checkable against the registry.

- Package: `jsqr`, version **1.4.0**
- Source: <https://unpkg.com/jsqr@1.4.0/dist/jsQR.js>
- Upstream: <https://github.com/cozmo/jsQR>
- sha256: `bc40c8a15196236b2314db0856f72ca0b49980cd5413b8c852a7349f5fee0859`
  (what the registry publishes, to check this against npm)
- blake3: `7e156db57fefffb4ea40e40c88fd63b8e5052eae37b7e47c6652160041a90818`
  (what the test below holds, in the hash clank already carries)
- Licence: **Apache-2.0**, in `jsQR.LICENSE` beside it

The binary serves it at `/jsqr.js`, on the unauthenticated side of the
door because the sign-in page needs it before it has a session, and
only when someone presses `scan` — it is 251 KiB, and a page that
fetched it on every load would spend that on everyone who pastes.

`web::tests::the_vendored_decoder_is_what_the_notice_says` holds the
digests above: a blob nobody can check is a blob nobody should ship, so
an edit here fails the build rather than passing quietly. To update,
fetch the new version, put its digests in this file and that test, and
read the diff.
