# a-pasted-image-is-an-attachment
# A pasted image is an attachment

## Why

> "pasting an image through the web interface doesn't work can you fix
> that."

It does not work because nothing was ever listening. The page has one
way in for a file — the `+` button's `<input type=file>` — and no
`paste` handler anywhere. An image on the clipboard is not text, so
pasting it into the box inserts nothing and no error is shown, which is
the worst of both.

Paste is how the person on the other end of this actually attaches
images: it is what they do with Claude Code all day, and a screenshot
is the reason to reach for a phone in the first place.

## The model

**However a file arrives, it becomes an attachment the same way.**

`+` already does the whole job: upload, take the server's path, put it
in the box, hold the draft while it is in flight, and keep Send shut
until it lands. That work belongs to the FILE, not to the button that
found it — but today it lives inside the file input's `change`
handler, so a second way in would have to duplicate every invariant
the last plan spent eight review rounds getting right.

So the handler becomes a function of one file, and both ways in call
it. A paste that carries no file is left alone entirely — pasting TEXT
into the box must keep working, so the event is only claimed when
something was actually taken from it.

## Deliverables

1. **`attach(file)`** — the upload path from the `+` handler, lifted
   out whole, with the pending/draft accounting it already has.
2. **A `paste` listener** that takes every file on the clipboard and
   attaches each in turn, so a paste of two images names two paths.
   Read `clipboardData.files`, falling back to `items` where that is
   what the browser fills in.
3. **`preventDefault` only when a file was taken.** A pasted URL, a
   pasted command, a pasted anything-that-is-text goes into the box as
   it always did.
4. **Listen on the document, not the box.** On a desktop the focus is
   often in the transcript when the screenshot is taken; the box is
   where the path lands either way.

## Tests

- A paste carrying one image uploads it and puts its path in the box,
  through the same route as `+` — the same pending hold, the same Send
  gate, so no invariant is re-implemented.
- A paste carrying two images names two paths, in order.
- A paste carrying only text changes nothing about the attachment path
  and leaves the text where the browser put it.
- A refused upload from a paste behaves as a refused upload from `+`.
- Mutation-check each: reverting the listener, the multi-file loop, or
  the conditional `preventDefault` must fail a named check.

## And the composer it lands in

> "can you make the text box styled like this. Centered bubble
> underneath the text. The text on the page still doesn't seem as wide
> as the claude app text. Roll that into this plan."

With a picture of the reference: one rounded bubble, the writing area
on top and the controls on a row beneath it, the whole thing centred
under the column it belongs to.

Ours is a single flex ROW — box, then buttons, all fighting for the
same line, which is why it had to be rescued from a 320px screen last
week. Two rows in one bubble gives the writing the full width at every
size and puts the controls where they cannot push it anywhere.

5. **The composer is a bubble**: rounded, bordered, centred, with the
   input across the top and `+` at the left of the row below it, Send
   (and Stop, when there is something to stop) at the right. The
   input loses its own border — the bubble is the border now.
6. **The column is wider.** 68ch of proportional text measured 605px,
   and it still reads narrow beside the reference. It goes to 46rem,
   and the composer takes the same width so the two line up.
7. **Both still fit a phone.** The checks that measure the document's
   scroll width and each control's right edge at 320/390/430 stay, and
   they cover the new shape.

## Out of scope

- Drag and drop. It is the same `attach(file)` call and worth doing,
  but it is not what was asked and has its own event surface.
- What the agent does with the path once it is sent. Unchanged.
- HEIC. A phone that hands over HEIC hands it over the same way here
  as through `+`; that question is already open and is not this plan's
  to answer.
