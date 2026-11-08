# interrupted-agent-is-loud
# An interrupted agent is impossible to miss in the TUI

## Why

LLFourn's TUI showed `claude  commit  interrupted 72090s` in the same dim grey as `idle` or
`waiting for work`. claude was a commit reviewer, and it had been interrupted for 20 hours while
commit 4923454 sat unreviewed. Nothing woke it, for two reasons:

- Claude Code runs no Stop hook on an interrupted turn, so no wait was parked.
- Recovery opens no episode for an interrupted turn (`recovery.rs`, `_ => {}`). It treats the turn
  as a person taking the pane.

Recovery is deliberately hands-off there. That makes the TUI the only place a person learns the
agent has stopped, so the row has to shout.

LLFourn: "make it super legible in the TUI when an agent has been interrupted. uppercase orange
text maybe?"

## Build

1. **The row shouts.** In the agent rows of `status_tui/render.rs` (the `not_working` branch that
   draws `s.brief(…)`), an `Activity::Interrupted` agent is drawn as `INTERRUPTED <age>`: uppercase,
   orange (bold if the palette needs it to stand out), not dim. Every other activity keeps its
   current style.
2. **It says what's stuck.** When the interrupted agent owes work, for example a review the gate is
   waiting on or a wake item, the row adds what it owes, for example `· owes review of 4923454`. Take
   it from the data the TUI already has. Don't add a new read path for it.
3. **The header agrees.** If the TUI's header line for the selected agent (`👀 CLAUDE reviewing`)
   would otherwise describe it as doing its role while it is interrupted, the header says
   `INTERRUPTED` in the same style.
4. **`clank status` text output** marks an interrupted agent the same way, without colour when it
   isn't a terminal. `status -j` is unchanged: it already carries the activity.

Use the TUI's existing palette and theme handling. Pick an orange that reads on both light and dark
terminals.

## Out of scope

Nudging an interrupted agent automatically after a grace period. That's a separate decision about
recovery policy, and this plan only makes the state visible.

## Tests

- An interrupted agent's row renders `INTERRUPTED`, uppercase, in the orange style, not dim. The
  other activities' rows are unchanged.
- An interrupted agent that owes a review shows what it owes, and one that owes nothing shows
  nothing extra.
- The header for a selected interrupted agent says `INTERRUPTED`.
- `clank status` text marks it.

## Done when

An interrupted agent stands out at a glance in the TUI and in `clank status`.

## As landed

- **The row.** An interrupted agent's row reads `INTERRUPTED <age>`, using `brief` (so the agent page and the monitor line are in capitals too). It's drawn in bold orange `38;5;166` (#d75f00). The head-correction orange 208 was too faint on white. `View::interrupted()` ignores stale states.
- **What it owes.** `owes(snap, label)` (`status_tui/derive.rs`) gives the routed work: review, revision, plan work, tag fix, or promotion via `promotable()`. `promotable()` is now shared with the header's promote arm. It also gives a count of the agent's own unacked event copies, across messages, commands, timers and github. Baseline history is born acked, so it's never counted.
- **The header.** It says `INTERRUPTED`, drawn off the lamp in the orange, when every actor it names is interrupted. With several reviewers where only some are interrupted, it keeps its role verb.
- **`clank status`.** Text output carries the capitals and the colour, painted only on a terminal with `NO_COLOR` unset. It doesn't carry the owes clause, because that comes from the TUI's routing. `status -j` and recovery are unchanged.
- **Known disagreement.** When HEAD's tag is broken and plans are queued, the row says `tag fix` (the wake order) while the header still says promote. That predates this plan.
