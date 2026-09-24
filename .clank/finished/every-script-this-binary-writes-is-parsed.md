# every-script-this-binary-writes-is-parsed

The generated site's scripts are read by the same guard as the pages'.

> "A second surface ships scripts from this binary and is unguarded:
> `cli/html.rs` emits inline JS — `THEME_HEAD_SCRIPT` (html.rs:2508),
> the block pushed at :2497 and the template at :856 — into every
> generated page. A SyntaxError there kills those pages exactly as
> this one killed the remote." — ruthless, reviewing 9473655

## Why

`a-page-that-does-not-parse-is-not-a-page` closed the hole it was
opened by: the remote's two pages are parsed, with their scopes, so a
script that cannot run cannot ship. It named the remote's pages as its
surface, which was the surface that was on fire.

The other surface is `clank html`, which writes inline scripts into
every page of the generated site — the theme switch in the head, the
block beside it, the template's own. They ship from this binary too,
they run in the same browsers, and a duplicate declaration among them
would blank those pages the same way, with the same silence: the site
would build, the file would contain what a grep looks for, and nothing
would work.

One guard for one kind of fault, not one per surface.

## The build

The parse-and-scope check moves somewhere both surfaces can reach it,
and the site's scripts join the inventory the remote's are in: each
named, each read as what it is (the site's are classic scripts in a
`<script>` the generator writes), each failing the build when it does
not parse.

Where the generator composes a script from pieces, the check reads
what it COMPOSES rather than the pieces — a fragment that parses alone
and not together is the shape that shipped last time.

## Tests

- Every script `clank html` writes is parsed with its scopes; a
  redeclaration or a syntax error in any of them fails.
- A script the generator assembles is checked as assembled, not
  piecewise.
- The remote's pages keep their own coverage, through the same helper
  rather than a second copy of it.

## Out of scope

- What the site's scripts DO; this is the same narrow promise as
  before — that they can run at all.
- Scripts that are not JavaScript. The title overreaches: this binary
  also writes `init_facts.rs`'s `POST_REWRITE_BODY`, a `sh` hook, and
  nothing parses it. The guard is for inline JS in pages, which is
  where a fault is silent — a broken hook fails loudly at the shell.
