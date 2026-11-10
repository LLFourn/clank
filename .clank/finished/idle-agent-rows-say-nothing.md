# idle-agent-rows-say-nothing
# An agent waiting for work says nothing in its TUI row

## Why

LLFourn: "these status are a bit too noisy … maybe waiting for work can just be omitted". In a typical TUI every agent row ends in a dim `waiting for work` (claude, artist and codex in his screenshot of 2026-11-10). That's the normal resting state, so repeating it on every row hides the rows that do say something: `working`, `INTERRUPTED`, a gate verb.

The words come from `agent_state::brief` (`Activity::Unobserved => "waiting for work"`), drawn by the `not_working` branch in `status_tui/render.rs` (around line 770).

## Build

- **TUI rows:** an agent row whose state is `Activity::Unobserved` (waiting for work) draws no state text. The name and role remain, and any suppressor or caveat clauses that `brief`/`line` add still show. If nothing remains, the row is just the name and role.
- **Other surfaces are unchanged:**
  - `clank status` text and the agent detail page keep "waiting for work", where an explicit word is useful;
  - `status -j` is unchanged;
  - every other activity (`working`, `idle <age>`, `INTERRUPTED`, `failed`, `asking`, `unwatched`) is drawn as today.

## Tests

- An agent row for an unobserved agent contains no "waiting for work", while `clank status` text for the same agent still does.
- A working, idle or interrupted agent's row is unchanged.
- A waiting agent with a caveat still shows the caveat.

## Done when

An idle roster renders as bare names and roles, and only agents doing something else carry a status.

## As landed

- `View::row` matches `View::activity()`. An `Unobserved` agent's row draws only its nudge clauses (`nudged`, `waiting for you`, …), or nothing at all.
- Every other activity draws `brief`.
- A stale view never reaches the row branch: `not_working` requires a current activity.
- `nudge_clauses` returns the clauses as a list, joined by `brief` and by `row`.
- `clank status`, the agent page and `status -j` still say "waiting for work".
