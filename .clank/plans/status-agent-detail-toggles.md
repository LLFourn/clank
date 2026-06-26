# status-agent-detail-toggles

## Problem

The per-agent detail page (`status --tui`, Tab → an agent → it opens) is a
plain vertical menu: it shows a read-only info block (tool, **tier**,
**auto**, invocation, purpose) AND a separate action list
(`toggle auto (→ off)`, `switch tier → gate`, `promote to master`,
`remove from team`, `← back`) chosen with ↑↓ + Enter. Two problems:

- The value-bearing settings (auto-mode, tier) are shown TWICE — once as
  read-only info, once as a clunky "toggle auto (→ off)" menu verb. You
  can't see/change them in one place; you press Enter on a verb that names
  the OTHER value.
- "remove from team" reads like any other row — nothing signals it's
  destructive.

## Design (frontend-design skill)

Keep the existing "instrument panel" aesthetic — dense, monochrome body,
state shown by WEIGHT (bold = active) with a single danger hue (red) for
destruction. The page becomes: a read-only info block, then a column of
**directly-manipulable rows** the cursor moves over.

Selection cue: a left **caret `▸`** gutter, NOT the full-width reverse
band used elsewhere — because these rows carry inline state (a toggle's
segmented value, the destructive red) that a reverse band would flatten.
(Deliberate, page-local; noted in code.)

Rows:

- **auto-mode** (toggle): `▸ auto      ‹ on │ off ›` — the active option
  **bold**, the other `dim`, a `dim │` divider; when the row is selected,
  flank the segment with `dim ‹ ›` to signal it cycles. ←/→/␣ flip it in
  place (applies immediately, stays on the page).
- **tier** (toggle, reviewers only): `tier     commit │ gate`, same
  segmented control.
- **promote to master** (action): `⇧ promote to master`.
- **remove from team** (action, DESTRUCTIVE): `✗ remove from team` in
  **red** (`colored("31", …)`); the caret turns red when it's selected.
- **back**: `← back`.

auto/tier MOVE out of the read-only info block (no more duplication); the
info block keeps tool / invocation / purpose. Master agents keep the
reduced set (auto + back).

Hint line: `↑↓ move · ←→ ␣ change · ⏎ select · esc back`.

## Implementation

- **input.rs**
  - Add `Key::Right`; parse `\x1b[C` → `Key::Right` (mirror the existing
    `\x1b[D` → `Key::Left`).
  - `agent_detail_nav`: ↑↓ → `MoveCursor`; `Enter`/`Space` →
    `Activate(current)` (any row); `Left`/`Right` → `Activate(current)`
    ONLY when the current row is a toggle (`ToggleAuto`/`SwitchTier`),
    else `None` (←/→ must not fire `Remove`/`Promote`). A small
    `is_toggle(DetailAction)` helper. Toggles are 2-state, so a flip is
    direction-agnostic (left == right == space == flip), reusing the
    existing `apply_detail_action` flip — no apply change.
- **render.rs**
  - Rewrite `render_agent_detail`: drop tier/auto from the info block;
    render the interactive rows via a new `detail_row_spans(action, agent,
    selected)` (caret + segmented toggle / glyph+label / red remove);
    update the hint. Retire/repurpose `detail_action_label` (the segmented
    rows replace the verb strings).
  - A `toggle_segment(active_label, other_label, selected)` helper for the
    `‹ on │ off ›` control (active bold, other dim).
- `apply_detail_action` unchanged.

## Acceptance

- Tests (pure): `parse_keys(b"\x1b[C")` → `Key::Right` (was asserted
  empty — update that test to the kept behavior); `agent_detail_nav` —
  ←/→/␣ on a toggle row → `Activate(ToggleAuto/SwitchTier)`, ←/→ on an
  action row (`Remove`) → `None`, Enter on `Remove` → `Activate(Remove)`,
  ↑↓ move, Esc → Back. `render_agent_detail` — auto/tier render as
  segmented toggles with the active option emphasized and are NOT also in
  the info block; the remove row carries `✗` and red SGR (`\x1b[31m`); the
  selected row shows the `▸` caret (not a reverse band).
- `cargo test -p clank --lib` green; fmt + clippy clean. Build +
  `cargo install` (user-facing TUI change).

## Out of scope

- Color-coding the toggle values (e.g. green "on") — keep monochrome
  bold/dim for cohesion; revisit only if asked.
- Editing invocation/purpose from the TUI (still read-only).
- Changing the agent-panel (the list) selection style — caret is
  detail-page-local.
