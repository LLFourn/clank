# a-finished-plan-ends-with-a-divider
# A finished plan ends with a divider in the log

## Why

In the TUI's LOG, a plan's run is marked by its banner (`── signal-group-mentions · 5m ──`)
under its first commit. Nothing marks where the run ENDS. Once a plan finishes, ad-hoc commits
(`~`), messages, timer events and reviews that land afterwards stack directly on top of the
finished run. They read as if they belong to it.

The user saw this on the `signal-group-mentions` run (newest at top):

```
3m ~ 39028c1 Move Rama group follow-up reminder to Sydn…
4m ✓✓ 💬 @assistent "Lloyd in Rama + LL + Bots …"
 …  (ad-hoc commits and events after the plan finished)
1h ~ bbbd177 Record delivered Rama call reminder
1h ⚑ e0f1e30 Send real @mentions in the bot's Signal gr…   ← the finish commit
1h ✓✓ ⏰ @assistent rama-call-2026-10-06 …
 …  (the plan's reviews and commits)
1h ✎ db0d7a1 intro
── signal-group-mentions · 5m ──
```

## What to build

- **An end divider.** In the LOG, directly above a finished run's finish row (`⚑`), draw a
  plain rule with no text. It separates the run from whatever came after.
- **Only when nothing starts right after.** When the next run's banner comes straight after
  the finish (the next plan's intro is the next row up), that banner already separates them,
  so draw no divider.
- **One source.** Derive it where the run banners are derived (`oneline_items_rows` in
  `cli/log.rs` and its TUI rendering), so `clank log` and the TUI agree. The divider is a
  line the viewport counts but not a selectable row: the cursor passes over it, while plan
  banners stay selectable.
- **The active plan has none**, since it has not ended.

## Acceptance

- A finished run followed by ad-hoc commits or events shows one textless rule above its `⚑`
  row. Everything above that rule is outside the run.
- A finished run followed directly by the next plan's intro shows no extra rule, just the
  next banner.
- An active run shows no end rule.
- Cursor movement and selection through the log are unchanged: the rule cannot be selected.
- `clank log` shows the same divider in the same places.

## As built

- **The row.** `OnelineRow::RunEnd` is a row of its own. `mark_run_ends`
  inserts it over the rows as finally interleaved:
  - it runs over the whole rows in `oneline_items_rows`;
  - `extend_oneline_rows` runs it again from the seam only, so a finish
    at the top of an older page is closed against the page above it.

  For each finish row (`⚑`, including a plan collapsed to its finalize
  commit), it looks up past that finish's own reviews, which stay inside
  the run. It inserts the rule unless the next row up is a banner (the
  next run's) or already a rule, or there is no row up. It is idempotent.
  The active run, having no finish, gets none.
- **Everywhere the log is drawn.**
  - `clank log` prints a dim fixed-width rule.
  - The TUI draws a full-width dim line with no age gutter.
  - The remote timeline's ledger gets a `run_end` row that the page
    draws as a thin line.
- **Never selected.** The rule stays a physical line, so viewport sizing
  counts it.
  - Every move lands the cursor at once, past a rule in the direction it
    moved, or the other way at an end: up, down, page and top/bottom all
    go through `LogView::moved` and `scroll::land`, with `is_rule` as the
    loop's predicate.
  - Keys handled together before a paint therefore select what the same
    keys do one at a time, and an Enter in the same batch never finds the
    cursor on a rule.
  - `settle` lands it again after a refresh changes the rows. Enter on a rule opens nothing. Reviews still bind to the
  commit below them, and plan banners still open their plan.
