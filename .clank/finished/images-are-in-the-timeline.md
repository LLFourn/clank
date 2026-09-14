# images-are-in-the-timeline

> Does the jsonl show images? Could you render images output from
> user and agent in the timeline? — lloyd

## The transcript keeps them; the page drops them

Both harnesses keep pasted images in the transcript itself. Claude's
JSONL carries them as content blocks — `{"type":"image","source":
{"type":"base64","media_type":"image/png","data":…}}` beside the
text of a user message (a screenshot is ~40 KB of base64); codex's
rollout carries `{"type":"input_image","image_url":"data:image/png;
base64,…"}` the same way. Tool results can carry the same block
(an agent reading a picture). The adapters skip every one of them
today, so a turn that was "here's what I see" arrives as its words
alone.

## The design

**An image is a turn.** `Body::Image { media_type, data }` joins
`Text`, `Thinking` and `Tool`; the claude adapter makes one turn per
image block (`{uuid}:{i}`, as for text), the codex adapter one per
`input_image` whose `image_url` is a `data:<type>;base64,<data>`
URL. An image inside a tool result rides on the tool's output —
`Parsed::ToolOutput { id, output, images }` — and the tool turn
shows it under its text.

**Bounded.** An image over 2 MB decoded is not shipped: the turn is
`Body::Image` with no data and its size, and the page shows `image,
3.1 MB — not shown`. The window stays 200 turns.

**On the page.** An image turn is an `<img>` on a data URL, at most
the column's width and 60% of the viewport's height, decoded
lazily; tapping it opens the full picture in a new tab. A person's
image sits in the person's block as their text does; a tool's under
its output.

## Tests

- claude: a user message with text and an image → a text turn and an
  image turn, both the person's, in order; a tool result with an
  image → the output carries it; an image over the cap → no data,
  the size.
- codex: `input_image` with a data URL → an image turn; a URL that
  is not `data:` → nothing.
- The page reducer keeps an image turn as any other.
- Mutations: the image block skipped — caught; the cap ignored —
  caught; a non-data URL shipped — caught.

## Out of scope

- Images the agent itself produces (none of the harnesses records
  any); files the page could fetch by path.

## Acceptance

- [ ] a pasted image shows in the transcript, in place, for claude
      and codex
- [ ] a tool result's image shows under the tool's output
- [ ] an oversize image is named, not shipped
- [ ] tests as above, mutation-checked
