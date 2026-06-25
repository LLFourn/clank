# console-polish
# console polish — chrome legibility + focus-follow

Hands-on follow-ups to `console-cleanup`. Four targeted improvements to
the `clank open` console; keep the minimal philosophy.

## 1. Working-agent indicator: green outline, not a cramped dot

Today the working agent gets a green `●` jammed right against its tab
number (`●1:claude`) — too tight to read. Replace it with a clean green
**outline around the tab** so "who's working" reads at a glance without
crowding the label.

The chrome is a single row, so a literal four-sided rectangle needs
height. Two options to settle in the plan:

- **Stay 1 row (preferred, minimal):** wrap the working tab in green
  bracket/edge glyphs with breathing room — e.g. `▐ 1:claude ▌` or
  `▕ 1:claude ▏` rendered in green (SGR 32), distinct from the focused
  tab's reverse-video band. The two cues stack cleanly: a tab can be
  both focused (band) and working (green edges).
- **Grow the chrome to 2–3 rows** only if a true boxed rectangle is
  wanted — costs content rows, so do it only if the 1-row outline reads
  poorly in practice.

Default to the 1-row green outline; keep the focus cue (reverse-video)
and the working cue (green) visually separable.

## 2. Auto-follow the working agent

The main pane does NOT auto-switch when clank activates a different
agent — you have to switch by hand. It should follow: when an agent
**newly becomes the working one** (enters the work-state set the poller
already computes), switch the main pane to it.

- The `Ev::Working(set)` handler already runs on every work-state
  change. Diff the new set against the previous: if an agent that
  wasn't working now is, set `active` to it and clear `status_focused`
  (so the agent is actually shown). Pure helper, e.g.
  `newly_active(prev, next, roster_order) -> Option<usize>`, unit-tested.
- If several agents activate at once (e.g. master commits → both
  reviewers start), pick the first in roster order. Deterministic.
- Only agents auto-focus; the status pane never steals focus.
- Open question for review: should a manual switch temporarily suppress
  auto-follow? Default NO (always follow — that's what was asked); add a
  suppression only if it proves jumpy.

## 3. Visible chrome around the status pane

The status pane has no border, so its edge blends into the agent pane.
Add a **divider** between the two:

- Landscape: a vertical rule (`│`) column between main and status.
- Portrait: a horizontal rule (`─`) row between them.
- Reserve 1 col/row for the divider in `mux::layout` (shrink the panes
  accordingly); render it dim. Optionally a small `STATUS` label on the
  rule so the pane is named.

This is a layout + render change: the divider is its own thin region,
the per-region winsize math already in place just accounts for it.

## 4. `Alt-s` toggles status focus

`Alt-s` currently only *enters* the status pane (one-way). Make it a
**toggle**: press once to focus status, press again to return to the
active agent. Trivial: the `FocusStatus` action flips `status_focused`
instead of setting it `true`; returning focus lands back on the active
agent (cursor + input). Keep `Alt-digit` as the explicit "go to an
agent" (also returns from status, per the resolve_nav rule already in
place).

## Out of scope

Mouse support (still the deferred stretch from console-cleanup). No
multi-agent split, no scrollback — keep it minimal.

## Suggested order

1. `Alt-s` toggle (#4) — trivial, lands first.
2. Auto-follow (#2) — pure `newly_active` helper + the `Ev::Working`
   wiring, with tests.
3. Status divider (#3) — layout reserves the rule, render draws it.
4. Working-tab outline (#1) — chrome redesign, settle 1-row vs taller.
