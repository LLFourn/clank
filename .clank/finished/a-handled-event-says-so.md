# a-handled-event-says-so

## Goal

A github or command event in the TUI timeline must say, at a glance and in a fixed column, whether it still wants someone. Today only the unhandled state says anything, so "I have acknowledged this" and "this row does not mention it" look identical.

## Problem

`log_row_spans` (`status_tui/render.rs:1098`) renders an event row as the backend badge plus the shared describe line, and then:

```rust
if *unhandled {
    spans.push(colored("33", " ⚠".to_string()));
}
```

Three things are wrong with that, and they are one thing: **acknowledgement is encoded as an absence.**

1. **An acknowledged event is rendered as nothing.** There is no mark for the handled state, so a reader scanning the log cannot distinguish "handled" from "this row has no opinion". The only state with a glyph is the one you have not dealt with, which means the common case — a timeline of events you have already worked through — is a wall of rows that all look unmarked, indistinguishable from rows that were never anyone's to answer.

2. **The mark is trailing, so it cannot be scanned.** It is appended after a variable-length describe line, so down a column of events the marks land at ragged x-positions and cannot be read as a column at all. It is also the first thing lost to width truncation on a narrow pane — the one signal that says "this still wants you" is the one that disappears first.

3. **`⚠` overstates.** An unacknowledged event is work waiting, not a fault. This repo already settled the principle in `a-refusal-is-not-green` — a mark takes the colour of its meaning — and warning-yellow for "you have not looked at this yet" is that same category error pointed the other way.

The timeline has a glyph vocabulary — review marks in green `✓`, cyan `✓✓`, red `✗`, commit markers `⚑ ⚒ ✎ ~` — but it does **not** have one shared mark column, and this plan must not claim to inherit one. The real layout:

- Every log row is prefixed with an age gutter (`render.rs:448`), so nothing a row builds starts at screen column 0. `age_gutter` is the age right-padded to the region's width plus a space (`render.rs:1244`), and a header or notice keeps the left margin instead.
- A commit row's marker icon leads its own spans, at column 0 of the row body (`render.rs:1149`).
- A review mark starts at column **2** of the row body, after two spaces, and `MARK_FIELD` pads it so the AUTHOR column lands consistently across verdicts (`render.rs:1196-1212`) — it aligns what follows the mark, not the marks of other row kinds.
- An event row leads with its backend badge, and the acknowledgement mark is appended at the end.

So there are three different positions already, and the one thing they share is the age gutter in front of them.

## Design direction

**Every event row carries a mark, in a fixed column of its own.** The column is event-only and deliberately so: the question "does this still want me?" is one an operator scans down the EVENTS, and a mark that is fixed relative to other event rows answers it. Claiming a column shared with commit and review rows would mean moving those, which is a different change with different risk.

Place it at column 0 of the event row's body — immediately after the age gutter, before the backend badge — which is where a commit row already puts its marker icon. That makes those two agree without touching review marks at column 2, and it puts the state to the LEFT of everything variable-length, so width truncation eats the describe line rather than the state.

**Absence is given a real meaning instead of an ambiguous one.** Three states, two glyphs:

| state | mark | reading |
| --- | --- | --- |
| wants someone | `?` | still unanswered — the only row with colour |
| handled | `✓` | dealt with; recedes |
| pre-watch history | *blank* | never yours to answer |

Pre-watch (`baseline`) rows already render fully dim with a `(pre-watch)` suffix: they are context from before the watch existed and were never anybody's to acknowledge. Leaving their mark field empty makes the blank column mean "not yours", which is information, rather than "handled or unmentioned, cannot tell".

**Weight goes to the unanswered ROW, not to its mark alone.** Colour belongs to the rows that still want someone: an unanswered row keeps its state mark coloured *and* its backend badge, while a handled row carries no colour at all — mark and badge both recede. A timeline is mostly history, and the few rows that still want you should be the ones that interrupt scanning.

The badge keeps its colour on an unanswered row rather than surrendering it so the mark can be the single coloured thing. The badge is identity — which source produced this — and it is most worth reading on exactly the rows a person is about to act on; taking its colour away there would cost a signal that predates this change in order to satisfy a rule about counting colours. What matters is the CONTRAST between the two states, and colour-versus-none delivers that more plainly than one hue versus another.

A handled row stays distinguishable from a pre-watch row because its describe line is still plain. Pre-watch is dim *everywhere*, including its text, and carries no mark at all.

**The row stays binary; the page owns the detail.** `unhandled` is `members.iter().any(|m| !m.acked)` (`status_tui/mod.rs:5111`), so an event with several agent copies reads as unanswered while any copy is. Keep that: the row's question is "does this still want someone?", which has two answers. The per-copy breakdown already exists on the event page, which lists each copy with `handled` / `UNHANDLED`. Do not invent a third glyph for partial acknowledgement — say in the code why, so the next reader does not add one.

Both backends get this. A `cmd` event is acknowledged the same way a `gh` event is, and the same mark must mean the same thing on both.

## Work

- Give the event row a leading mark in a fixed-width field at column 0 of its body, and remove the trailing `⚠`. Do not keep both: two marks for one fact is how the next one drifts.
- Mark the handled state as well as the unhandled one, so no state is rendered as an absence except the pre-watch case that genuinely has no answer to give.
- Keep colour proportional to meaning: an unanswered row carries it (mark and badge), a handled row carries none, and the mark never uses the warning colour reserved for faults — work you have not looked at yet is not a fault.
- Assert the STYLES, not only the text. A rendered row's colours do not survive `visible`, so a test that reads text alone cannot tell a receding row from a shouting one.
- Keep pre-watch rows unmarked and fully dim, and assert that a handled row and a pre-watch row are distinguishable — dimming the handled mark is what makes them close, so this is the pair most likely to collapse.
- Define the narrowest pane this contract holds at, and assert the RENDERED row at it — the row as the region builds it, age gutter included, not `log_row_spans` alone. `render_at` clamps to `cols.max(1)` (`render.rs:134`), so there is no width at which every row kind stays legible and a promise of "always visible" is unkeepable without a stated floor. Below that floor the pane is degraded for every row kind and this plan promises nothing.
- State the priority rule at that floor: within the event row's own spans the state is leftmost, so the describe line is clipped before it. The age gutter still precedes it, and this plan does not change that — if the gutter alone can consume the pane, that is the existing behaviour for every row and a separate question.
- Check the event page against the row so the two surfaces say the same thing in the same words. The page has `handled` / `UNHANDLED` per copy and a header hint; if the row now says `?`, the page should not call the same state something a reader has to translate.
- Keep the glyphs in the existing vocabulary rather than starting a parallel one, and make sure the event mark cannot be read as a review mark — `✓` already means "approved" on a commit row, so if reuse is ambiguous in a mixed timeline, pick a different handled glyph and say why in the code. The two now sit in different columns, which helps but does not settle it on its own.

## Acceptance

- Every event row in the timeline shows its acknowledgement state in a fixed leading column: unanswered and handled each have their own mark, and only pre-watch rows have none.
- The marks align vertically down a timeline of events with describe lines of differing lengths, in the rendered rows including their age gutter.
- At the stated minimum width the mark is still present and still distinguishable in the RENDERED row; truncation removes describe text before it. The minimum is named in the test rather than implied.
- Commit and review rows render exactly as they do today; this plan adds an event-only column and moves nothing else.
- An unanswered row is visually dominant over a handled one — asserted on span styles, not on text — and a handled row is still distinguishable from a pre-watch row.
- Acknowledging an event through the TUI flips its row from the unanswered mark to the handled mark without a reload.
- An event with several agent copies reads as unanswered while any copy is unacknowledged, and as handled once the last one is acknowledged; the event page continues to show the per-copy breakdown.
- `gh` and `cmd` events use the same marks with the same meanings.
- No row shows both a leading mark and the old trailing `⚠`.

## Scope

Presentation of an existing fact. No change to what acknowledgement means, to the ack fanout, to the event log, or to which events appear in the timeline. Out of scope: the event page's own layout beyond making its wording agree with the row, and any change to commit-row or review marks.
