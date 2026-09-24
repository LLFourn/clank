# the-cloud-is-always-the-cloud

The remote's icon asks for the presentation it wants.

> "how could it be that one window of clank displays it one way and
> the other displays it a different way." … "yes you should queue a
> plan to make it always the nice looking clouds." — lloyd, with both
> in one screenshot: a colour cloud on one pane's remote row, a
> monochrome bump on another's

## Why

`REMOTE_ICON` is `U+2601`, bare. That codepoint has two
presentations — text (a thin monochrome outline from the text font)
and emoji (the colour font's cloud) — and with no variation selector
the renderer picks, per cell. It picks differently for a dim row and
for the selected row drawn inverted, so one frame shows both, from one
constant, in two panes of one terminal.

The code already states which it wants:

> `☁` is text-presentation, one cell, not an emoji

and the string never says so, which is why it is not true. The choice
is lloyd's, and it is the other one: the cloud.

## The build

The icon carries `U+FE0F` — VARIATION SELECTOR-16 — so every renderer
draws the emoji and none of them decides for itself. That is two cells
wide, which `char_width` already accounts for: it counts `U+FE0F` as
one so the selector pays for the second cell the glyph takes.

What reserved room for a one-cell icon now reserves it for two: the
remote row's pad to the mark column, so its name lines up with the
rows above it, and the bar cluster's own reserve, so the bar's text is
not squeezed by a cell nobody counted. The comment says what the
string now asks for.

## Tests

- The icon measures two cells, and the remote row's name starts at the
  same column as the agent rows' — at every state, since the width
  cannot depend on the colour.
- The bar's reserve counts the icon as drawn, so the text beside it
  keeps its width.
- The row is not clipped at the narrow widths the pane tests already
  use.

## Out of scope

- The other glyphs. `💤`, `🔨` and the verdict marks are unambiguous;
  this is about the one codepoint that has two faces. If another
  arrives, it makes the same request of the renderer.
