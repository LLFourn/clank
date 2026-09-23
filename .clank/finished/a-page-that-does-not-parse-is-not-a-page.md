# a-page-that-does-not-parse-is-not-a-page

The pages in the binary are parsed before they ship.

> "I see this error in the console: `Uncaught SyntaxError: Identifier
> 'remember' has already been declared (at (index):391:9)`" — lloyd,
> after two sessions of a remote that signed in and then showed
> nothing

## Why

`the-phone-scans-the-token-and-keeps-it` added `async function
remember()` to `page.html` to fetch and keep the credential. The file
already had `const remember = (key, value) => …`, three hundred lines
down, for the view preferences. Two declarations of one name in one
scope is a `SyntaxError`, and a `SyntaxError` is not a broken feature:
the whole inline script never runs. No socket, no frames, no roster —
a page that signs you in and then says `No agents in this session yet`
forever, which is also what it says before it has heard anything.

It shipped because nothing reads the page as a program. The tests
assert that the page CONTAINS `'/token'`, `'/forget'`, `getUserMedia`
— all true of a page that cannot run at all. A check that a dead page
passes is not a check, and it was mine: I wrote those greps and called
them "the asset and the page ship together".

The reviewers could not have caught it either: they read the diff, and
the collision is with a line far outside it. A name is unique across a
file, and only something reading the whole file can say so.

## The build

**The collision goes.** The credential's function takes a name of its
own; the view-preference helper keeps the one it had, being older and
in use.

**The pages are parsed in a test.** Every script the binary ships —
`page.html`'s, `login.html`'s, and any module beside them — is handed
to a JavaScript parser, and a file that does not parse fails the
build. Parsing is all this needs: no DOM, no fetch, no engine
pretending to be a browser, and no network. The lightest pure-Rust
parser that accepts what these pages already use is the one to take;
it is a dev-dependency, so nothing the binary ships grows.

**The greps stop claiming more than they check.** They pin the page
to the routes it calls, which is worth keeping, and they say that in
their names rather than standing in for "the page works".

## Tests

- Both pages parse, and their scripts' declarations are unique in
  their scope — the exact fault, caught where it was made.
- A page with a duplicate declaration fails that test: the check is
  shown to fail on the bug it is for, not only to pass on the fix.
- The existing route-and-page greps still hold.

## Out of scope

- What the pages DO once they parse: that is
  `the-pages-logic-can-be-tested`, which this does not replace — it
  stops the page being dead on arrival, which is a smaller and more
  urgent thing.
- The other rendering faults already queued (`a-refusal-is-not-green`,
  `a-page-that-has-not-heard-is-not-empty`). The empty-state one is
  still real: it is what made a dead script look like an empty repo.
