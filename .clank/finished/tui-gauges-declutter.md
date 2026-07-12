# tui-gauges-declutter
# status TUI: agents first, branch on the log rule, drop echo gauges

An idle repo's gauge area reads (lloyd, 2026-07-12):

    git  master ee38b95
    done tui-short-pane-whole-scroll @ ee38b95

— the same commit twice, and both lines echo what the LOG below
already shows (the newest log row IS head; the finalize row IS the
last finished plan). Meanwhile the roster — the thing you actually
act on — sits under a title rule nobody needs ("everyone knows what
it is").

## New layout (panel renders — a repo WITH a roster)

    <bar>
    <agent rows>                      ← directly under the bar, NO
                                        "AGENTS" title rule; the
                                        selection band already shows
                                        focus, the detail page teaches
                                        the keys
    <load-bearing gauges only>        ← gate / fix / pr / dirty —
                                        state the log does NOT carry
    ── STASH / QUEUE ──               ← unchanged
    ── log · <branch> ──              ← the branch lives where the
                                        commits are; the sha goes
                                        (the top log row is head)
    <log rows>

- DROPPED in panel renders: the `git` gauge line (branch moves onto
  the log rule; head sha is redundant) and the `done` line (the
  finalize row in the log carries it).
- The agents section keeps its rows, selection band, spinner/verb,
  and the "+ add" row — only the title rule goes. The panel-focus
  hint ("↑↓ move · SPC play/pause · ⏎ details") moves to the agent
  DETAIL page (or is dropped — the band + muscle memory suffice;
  reviewer's call which).
- Panel-LESS renders (no roster): unchanged — they keep the `git` and
  `done` gauge lines, since there's no log rule to host the branch
  and no agents to lead with.

## Care points

- The pressure-lift/scrollable-header model just landed
  (tui-short-pane-whole-scroll): the agents rows moving to the TOP of
  the scrollable header changes which rows leave first under lift —
  agent rows now recede before the gauges. State this explicitly and
  update the walk fixtures; the budget/policy layer itself is
  untouched (it counts rows, not their meaning).
- Selection indices (`mode.selected()` mapping agents → add → stash →
  queue) must be unaffected — only the ORDER of header content
  changes, not the index space.
- The log rule keeps its elevation-on-scroll behavior; the branch
  text must not break the focused/elevated styling or the width math.
- Dirty stats stay (not in the log); `fix`/`gate`/`pr` stay.

## Acceptance

- Idle-repo panel render: no `git` line, no `done` line; the log rule
  carries the branch; agents rows sit directly under the bar with no
  title rule (fixture-pinned before/after).
- Focus behavior unchanged: band on the selected agent row when
  panel-focused, log rule styling when log-focused (existing tests
  updated, not weakened).
- Panel-less renders byte-identical to today.
- Pressure-lift walk fixtures updated for the new header order; the
  render≡budget property test still passes untouched.
- fmt/clippy at the 18/6 baseline; suites green.
