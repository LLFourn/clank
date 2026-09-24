# a-row-truncates-the-same-way-selected-or-not

A row that is too narrow says so the same way whether or not the
cursor is on it.

> Found while probing the remote row for a different bug, and
> deliberately left alone there: pinning it inside
> `the-cloud-is-always-the-cloud` would have claimed a fix that plan
> did not make.

## Why

`row_line` has two paths and they truncate differently:

- `emit` (unselected) truncates SPAN BY SPAN. When a span exactly
  fills the remaining width, `truncate_to` returns it unchanged, no
  ellipsis is added, and the next span sees `remaining == 0` and
  breaks. The row ends with text silently missing.
- `emit_selected` concatenates every span first and truncates the
  whole line, so it adds the ellipsis.

So moving the cursor onto a row can change what the row SAYS, not just
how it is coloured. Measured on plain spans `["ab", "cdef"]` at two
columns: unselected `ab`, selected `a…`. The remote row differs at nine
columns the same way — `☁️ remote` against `☁️ remot…`.

The unselected form is the misleading one. `ab` looks complete; `a…`
admits it is not. A reader cannot tell a row that fits from a row that
was cut.

## The build

One truncation rule for both paths. The concatenate-then-truncate of
`emit_selected` is the one that tells the truth, but `emit` cannot
simply adopt it: it needs the per-span boundaries to apply each span's
own style, so the cut has to be computed across the whole line and then
distributed back over the spans.

Whatever shape that takes, the ellipsis must appear whenever anything
was dropped — including when the drop is a whole later span rather than
part of the one being written.

## Tests

- For a row of several spans, at EVERY width from 0 to its full width,
  the visible text is identical selected and unselected.
- A row whose spans exactly fill the width and has a further span
  after them ends in `…`, in both states.
- A row that genuinely fits has no ellipsis, in both states.
- The existing style/hue behaviour of each SPAN is unchanged: this is
  about where the cut lands, not about colour. The MARKER is not a
  span's, and changes: it is the row's, so it is drawn outside every
  span's escapes and in the default colour rather than inheriting the
  styling of whichever span it happened to end (ruthless ad736e2).

## Out of scope

- The selected row's padding and inversion. Only the TEXT has to
  agree; `emit_selected` still pads to the full width and `emit` still
  does not.
- Variation-selector pairing, which `truncate_to` already keeps whole
  (the-cloud-is-always-the-cloud).
