# one-menu-not-five
# One menu, not five

## Why

> "I want a single design with consistent ui for all these action menus.
> I hate the way the remote page works now. Not clear what's selectable
> and what's not."

> "I really hate the way the remote tui tries to embed all these
> shortcuts in the title horizontally."

## What is actually there

There IS a menu primitive. `button_block(key, label, desc, danger,
selected, cols)` draws a hotkey, a label, and a wrapped explanation
beneath, with the selection band across the whole block. Three pages use
it: the plan page, the event page, the purge choice.

Two pages hand-roll their rows instead:

- **`render_agent_detail`** builds single lines through
  `detail_row_spans`: a caret gutter, no hotkey, no explanation, and
  toggle segments for auto/review.
- **`render_remote_page`** builds different spans per action variant,
  each with its own icon (`↗`, `▦`, `⧉`, `‹`), and — the real problem —
  **interleaves non-selectable content into the action list**. The token
  row pushes a section heading above itself and wrapped credential lines
  below; session rows push their own heading; the back row pushes a
  blank. Selectable and unselectable lines share one sequence with
  nothing but the band to tell them apart, and the band is only visible
  on the row you are already on.

And the two title rules disagree about what a title is for:

| page   | left           | right                                            |
|--------|----------------|--------------------------------------------------|
| plan   | `plan · stem`  | `active`                                         |
| remote | `remote`       | `⏎ select · ␣ switch · o open · p phone · ⌫ revoke · esc back` |

The plan page puts each key **on the thing it acts on** and spends the
title's right side on STATE. The remote page puts six keys in the title,
as far from their targets as the layout allows, and says nothing about
state.

## The model

**A page has a title, a menu, and content. They are three different
things, and the menu is a list of one type.**

A menu row is one of exactly two kinds, and both get the same block
shape and the same selection band:

- **Action** — hotkey, label, explanation. `Enter` runs it, or opens a
  page.
- **Checkbox** — hotkey, label, on/off. `Space` ticks it in place.

There is no third kind, and the rule that keeps it that way is about how
much interaction a thing needs, not what type it is:

> **If a row runs an action, or ticks a checkbox, it stays on the page.
> If it takes text, or the thing behind it has several ways of being
> interacted with, that thing gets a page of its own.**

This kills the cramped-title problem at the root rather than tidying it.
A row that manipulates something IN PLACE has to teach its keys
somewhere, and the only somewhere is the title or the footer — which is
exactly how the remote page ended up with `⏎ select · ␣ switch · o open ·
p phone · ⌫ revoke · esc back` jammed into its title. An action needs no
legend and a checkbox needs one key. Anything richer has a page with room
to say what it does.

So neither a row nor a title carries a key legend:

**A title carries identity and state.** Keys live on their rows. The one
pinned line at the bottom carries navigation only — `↑↓ move · ⏎ select ·
esc back` — because navigation is the only thing not attached to a row.

Content is never a menu row. It is drawn below the menu, in its own
region, under its own rule.

## What becomes a page, and what does not

| today | interaction | verdict |
|-------|-------------|---------|
| plan priority | a number nudged by `←→` ±10 and `PgUp/PgDn` ±100, its keys explained in the footer | **its own page**, where it can be typed instead of nudged |
| the remote token | read it, paste it, rotate it — and today it wraps its own characters into the action list | **its own page**: the credential, what it admits, and rotate |
| open sessions | a list of objects, each revocable, whose heading is pushed by the first of them | **its own page**: the list, each row revocable |
| review tier, on the agent page | three checkboxes, `commit` graying out `plan`/`final` | **stays** — ticking a checkbox is a page-local act, exclusivity or not |
| review tier, while ADDING a reviewer | one tier cycled with `←→` before `Enter`, so the choice is made on a row belonging to a different question | **its own step**: `Enter` chooses who, then the same checkboxes say what they review |
| agent auto | on/off | **stays** — a checkbox |
| the remote switch | on/off, with `starting…`/`stopping…` between | **stays** — a checkbox |

The row that opens each page **shows the current value**, so the overview
survives the move: `priority 500`, `token`, `4 sessions open`.

`RemoteAction::Back` goes. `esc` is already the way out and already in
the hint; a row that duplicates a key is a row that can be selected by
accident.

## Deliverables

1. **`MenuRow`** — `Action { key, label, value, desc, danger }` and
   `Checkbox { key, label, on, desc }` — with one `render_menu(rows, sel,
   cols)` drawing both through `button_block`'s shape. The single source
   of menu layout.
2. **Three pages**: priority, token, sessions. Each a title, a menu or a
   list, and content.
3. **The remote page becomes a menu.** Switch is a checkbox; open, phone,
   token and sessions are actions. The credential's characters and the
   session metadata move to their pages. Its title loses the six-key
   legend and gains state.
4. **The agent page uses the same menu.** Auto and the three tier rows
   become `Checkbox`es, keeping the exclusivity they have; remove stays
   an `Action` with `danger`. Its tool/invocation/session block becomes
   content under its own rule, which is what it already is.
5. **The plan page keeps its shape** — it is where the design comes from
   — but moves onto `MenuRow`, and its priority row becomes an action
   opening the priority page.
6. **Adding a reviewer asks in two steps.** The picker chooses WHO, and
   `Enter` opens a step that ticks what they review, using the SAME
   checkbox component and the SAME exclusivity as the agent page. The
   `←→`-cycled `tier` leaves `Mode::AddPicker` entirely.

   This is the fourth place the same defect appears: a choice among more
   than two, expressed as a hidden multi-key control on a row that is
   really asking a different question. The picker's row means "this
   agent"; the arrows meant something else entirely, and nothing on the
   row said so.

## The document, fullscreen

Scrolling past the last menu row currently focuses the plan document
while leaving the menu on screen above it, so the document gets whatever
rows the menu did not want.

- Entering the document (`↓` past the last row) gives it **the whole
  pane**: its own rule at the top, the body, the hint at the bottom.
- `↑` at the document's first line **returns to the menu**, cursor on
  the last row, rather than being swallowed.
- Everywhere else `↑↓` scroll the document as they do now.

This is a state, not a scroll offset: `PlanDetail` gains a focus of
`Menu { sel }` or `Document { scroll }`, so "the menu is not drawn" and
"up leaves" are the same fact rather than two derived ones.
`document_focus`, which today encodes the boundary as `sel ==
actions.len()`, goes.

## Tests

- One menu renderer: an `Action` and a `Checkbox` from the same list
  agree on gutter width, indent and band.
- **No menu row and no title rule carries a key legend**, asserted across
  every page, so a new page cannot quietly reintroduce one. This is the
  regression that produced the complaint.
- A remote page with a token and four sessions puts **every** selectable
  row in the menu and **no** content line in it: walking `sel` from 0 to
  the end visits exactly the rows a key acts on, and that count equals
  the number of rows.
- Each of the three new pages changes the value it owns, and the row that
  opens it shows the new value afterwards.
- The tier checkboxes keep their exclusivity in place: ticking `commit`
  clears `plan`/`final`, without leaving the agent page.
- Adding a reviewer takes two steps: the picker's `Enter` does not add
  anyone, and the roster changes only after the review step confirms.
- The add step and the agent page agree — the same ticks produce the
  same roster tier, asserted against each other rather than against a
  literal, so they cannot drift.
- `Mode::AddPicker` no longer carries a tier, and `←→` in the picker does
  nothing.
- Priority still moves by ±10 and ±100, on its page, and can be typed.
- Entering the document hides the menu; the document fills the pane to
  one line short of the hint.
- `↑` on the document's first line returns to the menu with the cursor
  on the LAST row; `↑` anywhere else scrolls by one.
- The plan page's action behaviour is otherwise unchanged: same keys,
  same order, same destructive confirmations.
- Mutation-check each.

## Out of scope

- The log and the status bar. This is about action menus.
- New actions, or changing what any existing action does.
