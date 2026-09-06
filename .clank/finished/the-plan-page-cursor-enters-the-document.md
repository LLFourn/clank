# the-plan-page-cursor-enters-the-document

> On a plan page in clank status --tui there's two issues: no need for
> a line gap between options; and when you scroll down to the document
> section so you can scroll the document, the final option `esc` stays
> highlighted. — lloyd

## Two symptoms, one false model

The plan page (`render_plan_detail`, `plan_detail_nav`) says the
cursor "never enters the document": ↓ on the last button scrolls the
document while the cursor STAYS on that button. So the moment the
operator starts reading the document, `esc back` sits in reverse
video — a highlighted control nobody is on. Enter at that point fires
`back`, which is the trap the highlight is honestly advertising.

The panel↔log model this page claims to mirror does the opposite: ↓
past the last panel row moves FOCUS into the log, and nothing in the
panel is highlighted while the log has it. The fix is to be that
model, not to hide the highlight while keeping the cursor on the
button.

The gap between buttons is unrelated and just spacing: `button_block`
already draws a label line plus dim explanation lines, which
separates blocks on its own; the blank line after each is a third of
the vertical budget on a short pane.

## The model

The document is a focus position, past the last button:
`sel == actions.len()`.

- ↓ on the last button → the document has focus. No scroll yet — the
  same step the panel takes into the log.
- ↓ with the document focused → scroll 1. ↑ → unscroll 1; at the top,
  ↑ → the last button.
- Enter with the document focused → nothing. Esc → back, as anywhere
  on the page. Hotkeys and PgUp/PgDn/Space are unchanged and work from
  any position.
- A refresh REBINDS the cursor by identity, never by index (codex on
  40e5d34). `plan_actions` changes shape when a plan finishes —
  `[OpenHtml, Stash, ForceFinish, Purge, Back]` becomes
  `[OpenHtml, Squash?, Purge, Back]` — so a clamped index that was on
  `ForceFinish` would land on `Purge`, and the next Enter would run a
  different destructive action than the operator last saw. The rebind
  is the agent page's `rebind_detail_sel` shape, applied to the action
  list computed before the refetch and the one after:
  - document focus stays document focus, whatever the new length;
  - a selected action that is still listed keeps its identity, at its
    new index;
  - a selected action that vanished falls back to DOCUMENT focus — the
    one position with no Enter target, so a stale keypress does
    nothing rather than something else.

## What it looks like

- No button carries the selection band while the document has focus.
- The document rule shows its key hint only while focused —
  `── DOCUMENT · ↑↓ scroll ────` — which is what the main screen's
  rules already do with `focused`; today the page passes `false` and
  the hint it names is never drawn. Lift-on-scroll is unchanged and
  keys on scroll, not focus: elevation says "content is under the
  bar", not "you are here".
- The bottom hint row says `↑↓ scroll · esc back` while the document
  has focus, and `↑↓ move · ⏎ select · esc back` on the buttons.
- Buttons are contiguous: label line, explanation line(s), next label
  line. The blank after the page's title rule stays.

## Tests

- `plan_detail_nav`: ↓ on the last button → `Sel(len)`; ↓ at `len` →
  `Scroll(1)`; ↑ at `len` with scroll → `Scroll(-1)`; ↑ at `len` at the
  top → `Sel(last)`; Enter at `len` → `None`; ↑ from a middle button
  with a scrolled document still moves the cursor.
- Render: with the document focused, no line on the page carries the
  reverse band, and the document rule carries the hint; with a button
  selected, the rule does not. The existing "document lines are never
  highlighted" test moves to the document-focus position it was
  approximating.
- Render: between two consecutive button blocks there is no blank line.
- The refresh rebind, active→finished with each action selected:
  `ForceFinish` and `Stash` (removed) → document focus; `Purge` and
  `OpenHtml` and `Back` (retained) → the same action at its new index;
  document focus → document focus across both shrink and growth. And
  through the mode rebind in the loop, so the rebind is what the
  refresh actually calls.

Each assertion is mutation-checked with production-only edits.

## Out of scope

- Any change to what the buttons do, or to purge/confirm/input pages.
