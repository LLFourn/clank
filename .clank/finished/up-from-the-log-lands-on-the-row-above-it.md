# up-from-the-log-lands-on-the-row-above-it
# Up from the top of the log lands on the panel row directly above it

## Why

In the status TUI, pressing Up at the top of the LOG jumps over every section between the
agents and the log (remote, stash, queue, drafts, sources) and lands on the agents. The
user: "I have no idea why it wants to do that but it shouldn't."

The cause is arithmetic: `scroll::log_up_target(cursor, offset, agents_len)` returns
`Some(agents_len)` as the panel cursor, i.e. the row after the last agent. That was the row
adjacent to the log when the panel was agents plus "+ add". Every region added below since
then sits between that index and the log. `panel_row_list` says it is THE source of panel
positions because computing one arithmetically breaks as soon as rows are added, and this is
a remaining arithmetic caller.

## What to build

- Up from the log's top lands on the LAST row of `panel_row_list`, the one drawn directly
  above the log, whatever regions exist. The crossing's guards stay as they are: cursor on
  the first entry, viewport at its top, and a panel to cross to.
- Down from that last row still enters the log, so Up then Down is a round trip.
- Grep for any other place that names a panel row by arithmetic over `agents.len()` and
  route it through `panel_row_list` too, reporting each hit and whether it was wrong.

## Acceptance

- With queue, drafts and sources present, Up from the log's top selects the last source row.
  With only agents, it selects the row the panel draws last. The test asserts the row KIND
  and identity (`PanelRow::Source(last)`), not a number that the old arithmetic could also
  produce for some fixture.
- Up then Down returns to the log's first entry.
- Mutation-checked: putting back `agents_len` as the target fails a test.

## Out of scope

The panel's region order and the log's own navigation.

## As built

- `scroll::log_up_target` returned the panel cursor as `agents_len`. It is now
  `crosses_into_panel`, the guard alone (cursor on the first entry, viewport at its top, a
  roster to cross to). The target comes from `log_up_crossing` in the loop, which selects
  the last row of `panel_row_list` at the current width, including drawable wait rows and
  grouped source rows.
- The audit found one more arithmetic panel position: `apply_detail_action` returned
  `AgentPanel { sel: idx }`, the agent's index, when its label no longer parsed. It now
  returns `agent_row`, the row the caller located, as Back already did. The other
  `AgentPanel { sel: … }` sites take `row_position`/`add_row_index` results, or row 0, which
  is always the first agent's.
- Tests cover a wait row, the queue, drafts and grouped sources (Up lands on the last
  source row and Down from it enters the log), the bare panel (Remote, not + add), the
  mid-log, burst and no-roster guards, and the detail fallback. Each mutant is caught,
  including the old `agents_len` target.
