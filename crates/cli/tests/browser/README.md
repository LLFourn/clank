# Browser checks for `web/page.html`

The page is JavaScript, and `cargo test` cannot run it: a renamed field
or a clipped element is not an error there, just a blank space. These
checks drive the real `page.html` in a real browser against synthetic
status frames, and they are run BY HAND — wiring them into `cargo test`
would put node and a chromium download between the repo and its tests.

```sh
cd crates/cli/tests/browser
npm install playwright
npx playwright install chromium     # or set CLANK_CHROMIUM to a build you have
node check.js
```

Every check is written so that reverting the fix it guards makes it
fail; they were built by reproducing three defects found in review
(a draft delivered to the wrong agent after an automatic move, auto
not re-resolving after a send, and a clock clipped with the sentence
beside it) and one found here (a browser refusing site data killed the
page on its first statement).

What `cargo test` covers instead — the ids the script asks for, the
fields it reads, the session substitution — lives in `cli::web::tests`
and `cli::status_tui::tests`.
