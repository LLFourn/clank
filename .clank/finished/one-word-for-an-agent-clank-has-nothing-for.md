# one-word-for-an-agent-clank-has-nothing-for
# An agent clank has nothing for reads "waiting for work", whether or not a wait is parked

## Why

`an-agent-with-no-work-says-so` split `Activity::Unobserved` into two labels:
- `waiting for work`: a wait is parked;
- `no work`: none is parked, and the fold found nothing owed.

The user, 2026-10-17: "what is the difference between waiting for work and 'no work'. If you have no work aren't you also waiting for work." From the user's side there is no difference. In both cases clank has nothing for the agent, and it delivers work as soon as some arrives: through the parked wait, or by a nudge. Which delivery path is in place is clank plumbing.

## What to build

- **One label.** `agent_state::brief` renders `Activity::Unobserved` as `waiting for work`, whether or not `Suppressor::Delivering` is present.
- **No repeat.** `line` still skips the `Delivering` suppressor for an `Unobserved` agent, so the words appear once.
- **Unchanged:**
  - the resume and nudge clauses stay omitted for `Unobserved`;
  - other suppressors still show (`waiting for work · auto off`);
  - every other activity renders as before, including unknown, stale and owing states.

## Acceptance

- `Unobserved` with and without `Delivering` both render exactly `waiting for work`, in `line` and in the TUI's `brief`.
- `waiting for work · auto off` for an `Unobserved` agent with auto off.
- Owing, unknown and stale cases are unchanged.
- A repo grep for `no work` as a label finds no stale reference.

## As built

- **`agent_state::brief`** renders `Activity::Unobserved` as `waiting for work`, with or without `Suppressor::Delivering`.
- **`line`** is unchanged:
  - it still skips the `Delivering` suppressor and the resume and nudge clauses for `Unobserved`;
  - other suppressors still show.
- **Tests:**
  - `agent_state`: a waiter, and no waiter, both render exactly `waiting for work` in `line` and `brief`; `waiting for work · auto off`; owing caveats kept;
  - the existing unknown and stale tests pass unchanged;
  - `status_tui::render`'s row test expects `waiting for work`.
- **Sweep:** the remaining `no work` in the source is `stop_hook`'s deadline re-arm message ("no work is pending"), which is not this label. Finished plans are left as history.
