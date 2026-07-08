# tui-plan-page-redesign

Design feedback on the plan-actions page (tui-plan-actions-page):

- The plan document should be READ on this page — shown beneath the
  actions — not behind a "read the plan" menu item.
- The `── danger ──` rule is noise; purge being red is enough.
- Action hints get cut off on narrow panes (single-line `label  hint`
  rows truncate), and the purge hint wastes its space describing the
  NEXT screen ("rewrite history; next screen chooses how").
- The purge chooser's explanations are unreadable for the same
  truncation reason.
- Menu items should become big button-like blocks with word-wrapped
  explanations inside them.

## Changes

All in `status_tui` (input.rs + render.rs + mod.rs wiring); pure
rendering/input — no new CLI behavior.

1. **Remove `PlanAction::ReadDoc`** from the action enum, `plan_actions`,
   hotkeys, and dispatch. ⏎ keeps activating the selected button.

2. **Button blocks** (shared for the plan page and the purge chooser):
   a block is a label line (`  f  force finish`) plus its explanation
   word-wrapped to the pane width on the following line(s), indented
   under the label; a blank line separates blocks. Selection reverses
   the WHOLE block, not one row (`row_line` per block line). Danger
   blocks (purge; both chooser options) render red — dim red
   unselected, red reverse selected. No explanation is ever truncated:
   wrap, don't clip.

3. **Plan page layout**: header rule (stem + state) → button blocks
   (no danger rule) → a separating rule → the plan DOCUMENT body,
   word-wrapped, filling the remaining rows. The body scrolls
   (PgUp/PgDn + mouse wheel, matching overlay scroll keys) while ↑↓
   move the button selection; esc backs out. Fetch the body the way
   the removed ReadDoc overlay did (active plans from
   `.clank/plans/<stem>.md`, finished from `finished/`), once, when
   the page opens; store it on `PlanPage`.

4. **Rewrite the purge texts** — say what it does, never what the next
   screen shows:
   - plan-page purge button: "remove this plan's reviews and
     feedback from history, or delete the plan entirely."
   - chooser "artifacts only": "strip this plan's .clank files —
     reviews, feedback, the plan document — out of history. The
     implementation commits stay."
   - chooser "drop EVERYTHING": "delete the plan AND its
     implementation commits from the branch."

5. **Purge chooser** uses the same button blocks (both red), full
   explanations wrapped, esc/back row unchanged.

## Tests (pure render/input, existing patterns)

- `plan_actions` no longer contains ReadDoc; hotkeys unchanged
  otherwise.
- Plan page render: no `── danger ──` anywhere; the plan body text
  appears beneath the buttons; a long explanation wraps (assert both
  halves of a known sentence appear on successive lines at a narrow
  width, nothing clipped).
- Selection reverses every line of the selected block (existing
  reverse-band assertions extended to the block's wrapped line).
- Purge chooser: full explanation text present at narrow width; the
  words "next screen" appear nowhere in the TUI.
- Body scroll: PgDn advances the body region while the selected
  button stays put.

## Out of scope

- The confirm screens (banner style stays).
- Any change to what the actions DO.
