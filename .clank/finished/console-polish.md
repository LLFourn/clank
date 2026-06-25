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

## 2. "Follow active" mode (Alt-0 follow / Alt-digit pin)

Auto-follow is an explicit MODE, not always-on — so a manual switch
pins the pane and never gets yanked away mid-read:

- **`Alt-0`** → switch the main pane to whoever is currently the working
  agent AND turn follow-active mode **ON**. While ON, the pane
  auto-switches to whoever becomes the working agent.
- **`Alt-1`/`Alt-2`/…`Alt-9`** → switch to that specific agent AND turn
  follow-active mode **OFF** (a manual pin). The pane stays there even
  as work moves elsewhere (the working `●` still marks who's active).

State: a `follow: bool`. The `Ev::Working` handler updates the main
pane's agent ONLY when `follow` is on. The "current working agent" is
the primary one from the work-state set (first in roster order on a
tie — the deterministic `working_labels` already orders by roster).

- Pure helpers, unit-tested: pick the primary working agent from the
  set; and the `follow ? track-working : keep-pinned` decision.
- Alt-0 with no one working: keep the current pane, just turn follow on
  (it'll catch the next activation).
- This subsumes the old "always auto-follow" idea AND its suppression
  worry: follow is opt-in (Alt-0), and any Alt-digit pins (follow off).

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

## 4. `Alt-s` goes to status (one-way, orthogonal to follow)

`Alt-s` focuses the status pane — that's it, not a toggle (a toggle
conflates two orthogonal axes). Getting back to an agent is the agent
keys' job: `Alt-0` (follow current working agent) or `Alt-digit` (pin),
both of which already return focus from the status pane via the
existing `resolve_nav` rule. So the two axes stay independent:

- **focus** = which pane has input/cursor: an agent (main) vs status.
- **follow** = does the main pane track the working agent (§2).

`Alt-s` only moves focus; it leaves `follow` untouched.

## 5. Focus chrome: show whether main or status is selected

Right now only the cursor hints at which pane is focused. Make it
obvious in the chrome, reusing the §3 divider:

- When **status** is focused: render the status divider/`STATUS` label
  bright/accented (and the agent tab-strip band reads as not-current).
- When an **agent (main)** is focused: the active agent's tab band is
  the focus cue (already there); the status divider/label is dim.
- Also surface **follow mode**: when `follow` is on, mark it (e.g. the
  current tab shown as following, or a small `⏵follow` in the bar) so
  Alt-0 vs Alt-digit state is visible.

Keep focus (background/band + bright divider) and working (green edges,
§1) on separate visual channels so they never collide.

## Out of scope

Mouse support (still the deferred stretch from console-cleanup). No
multi-agent split, no scrollback — keep it minimal.

## Suggested order

1. Follow mode + keys (#2, #4) — `Alt-0` follow-on/jump, `Alt-1..9`
   pin/follow-off, `Alt-s` to status; pure helpers + tests. (These are
   the input-model changes; land them together.)
2. Status divider (#3) — layout reserves the rule, render draws it.
3. Focus chrome (#5) — light the focused pane's divider/band; surface
   follow mode. Builds on #3.
4. Working-tab outline (#1) — chrome redesign, settle 1-row vs taller;
   mind the width trap (a working tab is wider — reflow the strip).
