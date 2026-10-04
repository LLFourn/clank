# Browser checks for `crates/web/assets/page.html`

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
node bridge.js
node colors.js
node questions.js
node messages.js
```

`check.js` covers the header, the picker and the draft; `bridge.js` covers multi-megabyte frames, full retained transcript display,
connection notices and recovery on the same socket. Its synthetic browser
check complements `large_harness_turns_reach_a_browser_and_survive_connector_outages`,
which drives the real harness parser, Feed, RepoSession, bridge and browser socket.
`look.js`
covers the rendered transcript — markdown, tool grouping, the measure
and the two grounds.

`colors.js` checks actual computed colours across repository state changes at
320/390/768/1280px: links, controls, focused composer, question cards and
historical verdicts stay stable while activity/gate dots change. It also checks
text/button contrast, readable gate labels and draft retention. Set
`CLANK_COLOR_PREVIEW=/tmp/clank-colors` to save screenshots per width.

`questions.js` checks native-only question cards, choice/free-text/private forms,
opaque native IDs, replay/focus/draft preservation, rejection and unknown sends,
cancellation before HTTP replies, duplicate submission suppression, session
changes and private draft storage. Native socket delivery/arbitration has separate
Rust fixtures; this browser test does not claim a live authenticated round trip.

`messages.js` checks the real page's event timeline against synthetic custody
rows: the mail badge, an external reply's sent marker and destination, escaped
text, and absence of an invented recipient acknowledgement. Rust custody tests
cover actual accepted-operation projection and same-repo inbox merging.

Every check is written so that reverting the fix it guards makes it
fail; they were built by reproducing three defects found in review
(a draft delivered to the wrong agent after an automatic move, auto
not re-resolving after a send, and a clock clipped with the sentence
beside it) and one found here (a browser refusing site data killed the
page on its first statement).

Three things learned from mutating these, all worth keeping in mind
when adding more. A scripted mutation must assert its anchor is
UNIQUE, not merely present — the same line appeared in `applyTurns`
and `applyTurn`, so a mutation aimed at one silently hit the other and
read as "not caught". And: a crash is a failed check (an assertion that cannot
RUN has still told you something, so the run reports it rather than
exiting quietly), and a transparent element is not a dark one — a
colour check that ignores alpha passes for the unpainted element it
exists to catch.

What `cargo test` covers instead — the ids the script asks for, the
fields it reads, the session substitution — lives in `cli::web::tests`
and `cli::status_tui::tests`.
