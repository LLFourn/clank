# the-idle-bar-is-not-the-selection-band

In `clank status --tui`, an idle repo's header bar is visually
identical to a selected panel row. Both render as plain reverse
video, so the eye reads the header as "the cursor is up there".

## Why (the model, not the pixel)

`derive::state_color` maps each `AttentionState` to the frame's one
hue, and `render::bar` emits `\x1b[1;7;{color}m` — bold, reverse, in
that hue. Under reverse video the hue becomes the bar's BACKGROUND,
which is what makes each state legible at a glance.

Every state contributes a real color SGR — red `31`, orange
`38;5;208`, green `32`, cyan `36`, yellow `33` — except one:

    AttentionState::Idle => "2",   // dim   (derive.rs:134)

`2` is an ATTRIBUTE, not a color. It contributes no background, so
the idle bar collapses to the terminal's plain reverse — which is
exactly `text::emit_selected`'s selection band (`\x1b[7m`,
text.rs:169). The collision is not a coincidence of two chosen
colors; it is the one table entry that isn't a color at all falling
through to the default.

So the fix is to make idle a hue like every other entry, NOT to
special-case the bar or to move the selection band.

## Change

`AttentionState::Idle` gets a real hue: **`38;5;63`** (#5f5fff).

### Why that exact value (codex on 9eded7b)

Under reverse video the hue becomes the BACKGROUND and the terminal's
default background becomes the TEXT. So the bar must be legible with
BLACK text (dark terminal) and with WHITE text (light terminal) — the
same colour, both ways. Contrast ratio against each:

| SGR        | colour  | vs black | vs white | worst |
|------------|---------|----------|----------|-------|
| `34`       | #0000ee | 2.23:1   | 9.40:1   | 2.23  |
| `94`       | theme   | —        | —        | —     |
| `38;5;62`  | #5f5fd7 | 4.10:1   | 5.12:1   | 4.10  |
| `38;5;63`  | #5f5fff | 4.57:1   | 4.60:1   | 4.57  |

Plain `34` fails: 2.23:1 is black-on-dark-blue, unreadable. Relying on
SGR 1 to brighten it is not portable — some emulators treat bold as
weight only, so the bar's legibility would depend on the terminal.

A single colour can clear 4.5:1 BOTH ways only inside a narrow band,
L ∈ [0.175, 0.183] — below it black text fails, above it white text
does. `38;5;63` sits at L = 0.178, near the centre, and is the best
blue in the 256-palette by worst-case contrast.

It is a 256-colour literal for the same reason orange already is: a
fixed value keeps the contrast claim TRUE. A theme-mapped ANSI code
(including bright `94`) leaves the actual colour to the terminal, and
with it any number stated here.

### Make the defect unrepresentable (codex on dbaa3e0)

Changing the one entry fixes today's bug and leaves tomorrow's
possible, because the table's element type still admits the mistake:
`state_color` returns `&'static str`, so "an attribute where a colour
belongs" is a well-typed value. String-level tests cannot close that
— `\x1b[1;7;2m` really is textually distinct from `\x1b[7m`, and `"2"`
really is distinct from `"31"`, so the obvious assertions pass on the
exact collision this plan exists to remove.

So the hue becomes a type that can only hold a colour:

```rust
/// The frame's one hue. A COLOUR, never an attribute: `bar` paints it
/// as a reverse-video BACKGROUND, and an attribute (dim, bold) paints
/// no background at all — which is how idle came to render as the
/// selection band.
pub(super) enum Hue {
    Red,
    Green,
    Yellow,
    Cyan,
    /// A 256-palette index, for hues no 16-colour code is close
    /// enough for: orange (208) and the idle blue (63).
    Indexed(u8),
}
```

`sgr()` serialises it (`Red => "31"`, `Indexed(i) => "38;5;{i}"`).
The named variants are a closed set, and `38;5;n` is a colour for
every `n`, so there is no `Hue` that paints nothing. `AttentionState::
Idle => Hue::Indexed(63)` — and `Hue::Dim` cannot be written.

Scope: the type stops at the table. `state_color` returns `Hue`; its
two call sites serialise once (`state_color(snap).sgr()`) and thread
the `&str` onward exactly as today. `emit`, `row_line` and `bar`
interpolate an SGR body and have no stake in which SGR it is — giving
them the type would widen the diff without adding an invariant.

- Do NOT reach for a grey background: `Style::Highlight` already owns
- Do NOT reach for a grey background: `Style::Highlight` already owns
  dark grey (`\x1b[1;48;5;238m`) for the log's plan umbrellas, and a
  grey bar would trade one collision for another.
- `state_color` also feeds `Style::Accent` spans through `emit`, so
  check an idle frame's accent spans still read (they are rare under
  idle — a pending block is `Blocked`, not `Idle`).

## Tests

The one-off assertion ("idle is blue now") is worth little; the
INVARIANT is what keeps this from coming back:

The "is it a colour" invariant is carried by `Hue`, not by a test —
that is the point of the type. What remains testable:

- Every branch's `sgr()` is a foreground COLOUR SGR: `30`-`37`,
  `90`-`97`, or a well-formed `38;5;n`. Never an attribute (`1`, `2`,
  `3`, `4`, `7`). This is belt-and-braces over the type, and it is
  what a reviewer can read as the rule.
- Across all SIX rendered branches — blocked, correction, idle,
  master, promote, reviewer — the bar's leading escape differs from
  `emit_selected`'s.
- Every branch's hue is distinct from every other's — the bar's whole
  job is telling states apart.
- Idle is pinned to `Hue::Indexed(63)` specifically, not merely to
  "some colour that isn't the selection band": the contrast derivation
  above is what makes the value correct, and a looser test would stay
  green while an edit swapped in an illegible colour.
- Idle specifically: the rendered bar and a rendered selected panel
  row do not share a prefix.

Existing assertions on the old string return (`derive.rs` orange,
`render.rs` green/cyan) move to `Hue` values.

## What does NOT change

- The selection band (`emit_selected`) and every row that uses it.
- The other five hues, the lamp emoji, `bar_text`, the zellij tab
  indicator.
- `Style::Highlight`, `Style::Dim`, and the region rules — dim stays
  the right style for quiet TEXT; this plan only stops dim standing
  in for a bar COLOR.

## Acceptance

- An idle `status --tui` header is distinguishable at a glance from a
  selected row.
- No `AttentionState` renders a bar that matches the selection band.
- fmt/clippy/suites green at baseline.
