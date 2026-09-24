# a-refusal-is-not-green

The remote page's review marks take the colour of their verdict.

> "minor visual thing can you change the color of x from green to
> red." — lloyd, on a log where `✗ codex: distinguish a device that
> has n…` is drawn in the same green as the `✓` above it
>
> "the double ticks go behind the text for finished. Also they are
> yellow for some reason?" — lloyd, on `✓✓codex: The follow-up
> preserves the …`

## Why

Every other surface colours the mark by what it says: the TUI's log
rows go through `verdict_color` (green continue, cyan finished, red
request-changes, dim unmarked), and the static site has
`.mark-request-changes { color: var(--changes) }` against its own
`--changes` red. The remote page has one rule for all of them —
`page.html`: `.row.review .glyph { color: var(--hue); }` — so a
refusal is the accent colour, the same as approval.

The row already knows better: `LedgerRow` carries `verdict` as a word
beside the glyph (`status_tui/mod.rs`), and the page throws it away
for reviews. A colour that cannot be wrong about the verdict is one
taken from the verdict.

The same rule is why FINISHED runs into its author. Every glyph gets
`width: 1.5em` (`page.html`: `.row .glyph`), which fits the one-
character marks; FINISHED's is `✓✓`, two of them, so it overflows its
box and sits under the text that follows. A column sized for the
narrowest mark is a column that is wrong about the widest one.

## The build

The review row's glyph carries a class from the row's own verdict —
one class per verdict, named as the row names it — and the stylesheet
gives each its colour: request-changes red, continue the approving
green, finished its own, unmarked the muted grey. The page's palette
gains the colours it lacks, beside `--hue` as the site's sit beside its
accent -- ONE block, not two: `page.html` has a single `:root` and no
`prefers-color-scheme` at all, so the light/dark pair this said to
edit belongs to the generated site and not here. A row whose verdict is
missing keeps today's colour rather than losing the glyph.

`--hue` is assigned from the script per session state
(`setProperty('--hue', facts.hue …)`), which is also why FINISHED read
YELLOW: the mark was taking the session's colour, so both complaints
are this one rule.

The glyph column is sized for the widest mark there is rather than
the narrowest, so a two-character verdict sits in it like the others
and the text after it starts where it starts on every other row.

## Tests

- The row markup carries the verdict's class for each of the four
  verdicts, and a row without a verdict still draws its glyph.
- The stylesheet defines a colour for each class, each naming a
  variable the palette actually defines, and request-changes is not
  the approving colour.
- The four verdicts map to four DISTINCT classes. "every class has a
  rule" is satisfied by two verdicts sharing one, which IS the bug.
- The review row's markup interpolates the class, and the row
  populates it. Three links -- the mapping, the row, the markup --
  and testing any two leaves the third free to be wrong.
- The glyph column keeps its per-character allowance for the widest
  mark. That establishes the ARITHMETIC; whether the browser lays it
  out so is the font's business and not reachable from here
  (codex 90780fa).

## Out of scope

- The TUI and the static site, which already colour by verdict.
- The rest of the palette, and the glyphs themselves.
