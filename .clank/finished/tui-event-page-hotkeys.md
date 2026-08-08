# tui-event-page-hotkeys

The github event page advertises single-letter action keys that do
nothing. Make the keys it displays actually work.

## The bug (verified)

`event_action_row` (status_tui/render.rs:1304) draws `o  open in
browser` and `a  ack`, but `event_detail_nav`
(status_tui/input.rs:767) matches only Up/Down/PageUp/PageDown/
Space/Enter/Escape/Quit. Nothing routes a letter, so both advertised
keys are inert and the only way to act is Enter on the selected row.

The plan page already solves exactly this: `plan_hotkey`
(input.rs:227) maps every key it advertises — `Key::Html` for `o`,
`Char(b's')` stash, `Char(b'f')` force-finish, `Char(b'c')` squash,
`Char(b'b')` block, `Char(b'p')` purge — and gates each on the
action being currently available. The event page needs the same
seam; this is a missing mirror, not a new mechanism.

## The `a` complication — and the trap in the obvious fix

`a` never reaches a page as a letter: the parser folds
`b'\t' | b'a'` into `Key::Focus` (input.rs:1043). So the advertised
`a  ack` cannot be delivered by adding a `Char(b'a')` arm, and an
implementation that adds one will silently still not work.

**Do NOT resolve this by routing `Key::Focus` to Ack** (codex on
6028cb0). That variant is physical Tab as much as it is `a`, so it
would bind acknowledging every unhandled copy — destructive and
irreversible from the page — to the established focus-navigation
key. An inert key is a papercut; Tab silently acking an inbox is a
data-loss bug. This plan must not trade one for the other.

The parse is where the defect actually lives: two physically
distinct keys are collapsed into one variant, discarding the
information a page needs to tell them apart. Every mode that wants
the alias can re-add it; no mode can recover what the parser threw
away.

**Preferred shape — split the parse, preserve the alias at the use
sites.** Give Tab and `a` distinct variants, then map BOTH to focus
at each site that documents the alias today, so nothing existing
changes behaviour:

- input.rs:558 `DetailNav::Back`
- input.rs:588 `DocNav::Back`
- input.rs:904 `PanelAction::LeaveFocus`
- mod.rs:1700 (detail overlay back) and mod.rs:1964 (focus toggle)

The event page then routes ONLY the physical `a`, and Tab keeps
meaning focus everywhere including there.

**Fallback** — relabel the ack row to a letter that already reaches
the page. Acceptable, but it leaves the lossy parse in place for the
next page that wants `a`.

Whichever is chosen, the label and the behaviour must agree — that
agreement is the whole point of this plan.

## Scope

- An `event_hotkey(key, actions)` mirroring `plan_hotkey`: returns
  the action only when it is currently offered, so a no-URL event
  ignores `o` and a fully-handled event ignores ack.
- Route it in the event-page input arm alongside the existing Enter
  activation. Enter on a selected row keeps working.
- Hotkeys must NOT move the selection.
- Esc/q keep returning to the log (the page never quits the TUI).

## Acceptance

- A test that every key `event_action_row` displays maps to the
  action it names, driven from the SAME source as the render so a
  future action cannot be added with an inert key. A hand-written
  list of keys would re-admit exactly the drift this fixes.
- A RAW-INPUT regression, from input bytes through the parser into
  the page, proving physical `a` activates the displayed action and
  Tab NEVER acks. Asserting on `Key` values alone cannot catch this:
  the whole hazard is that the two bytes become the same value, so a
  test that starts from `Key` starts after the bug.
- Tab still means focus everywhere it does today (the sites listed
  above), verified rather than assumed.
- `o` on an event with no URL, and ack on a fully-handled event, are
  harmless no-ops rather than panics or misfires.
- A hotkey does not change the selected row.
- Existing Enter activation, ack fanout, retargeting and Esc/q
  behaviour unchanged.
