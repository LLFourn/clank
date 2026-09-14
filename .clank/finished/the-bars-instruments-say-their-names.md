# the-bars-instruments-say-their-names

> These "connected" icons in the status bar are ugly: one is a
> lightning emoji, one is a sideways lightning icon. What does
> either mean? No idea. They have a green background only, so they
> look weird on the idle background colour. Better icons, and fit
> the existing background or make that section of the bar gray or
> something distinct. — lloyd

## Two glyphs nobody can read

The bar's right end holds two indicators: zellij's reach (`⚡`
connected, `–` not in a session, `·` unknown, `✗` unreachable) and
the remote switch (`⌁` off/on/failed, `·` starting). Each is painted
in its own hue as a reverse-video cell inside the bar's band, so on
an idle bar two green cells sit in a blue field — the colour was
meant to be the glyph's, and reads as a patch — and neither glyph
says what it is: a lightning bolt is not zellij, a sideways one is
not a web server.

## The design

> I don't think having more text in the top bar would be good. An
> icon (not emoji) that can change colour is good. Just choose
> better icons. We don't need an icon to say we're connected to
> zellij — just a big red exclamation mark if we are not connected,
> with `! zellij`. — lloyd, on the first cut

**A band of its own.** The last cells of the bar are a dark-gray
band whatever the bar's state colour, so the instruments are a
cluster beside the lamp and their colours are foreground colours on
it — never a hue painted as a background patch, and readable when
the bar itself is green. One reservation, measured; too narrow for
it, the cluster yields and the lamp stays.

**zellij speaks only when something is wrong.** Connected, or not
yet known, shows nothing. Not in a session, or in one that does not
answer, shows `! zellij` in red — the one claim worth a word, since
it names what to do (`clank open` outside a session; zellij itself
inside one that is silent).

**The remote is one icon.** `☁` (U+2601, text presentation, one
cell — not an emoji), coloured by state: gray off, yellow while
starting, green on, red failed. The row under the agents carries
the same `☁`, so the band and the row say the same thing.

    …reviewing 3976e7f          foo ▏! zellij ☁▕
    …reviewing 3976e7f          foo ▏☁▕

`render::cluster(reach, remote)` composes the band's contents — text
and hue per item — in one place; `bar` lays them on the gray band.
`reach_glyph` and `remote_glyph` go.

## Tests

- The cluster per state pair: `! zellij` present for exactly the two
  disconnected states, red; `☁` always, in its state's hue; the band
  opens with the gray background and never the bar's colour; the bar
  exactly `cols` wide in every state; the lamp survives a pane too
  narrow for the band; `bar_body` strips the band.
- Mutations: the band painted in the bar's colour — caught; `!
  zellij` shown when connected — caught; the cloud's hue taken from
  the wrong state — caught.

## Out of scope

- Any other bar content.

## Acceptance

- [ ] the right end of the bar is a gray band: `! zellij` in red only
      when not connected, and a `☁` coloured by the remote's state
- [ ] no glyph's hue is painted as a background
- [ ] the remote row carries the same `☁`
- [ ] tests as above, mutation-checked
