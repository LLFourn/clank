# agent-state-ages-read-as-time
# An agent's state age reads as a time, not a count of seconds

## Why

LLFourn's TUI showed `claude  commit  interrupted 72090s`. That's 20 hours, and nobody reads 72090s
as that.

`brief` (`crates/cli/src/cli/agent_state.rs:437`) formats the activity's age, and its `nudge in` time,
with `config::show_duration`. That formatter is exact on purpose, for config values: whole hours
print as `Nh`, whole minutes as `Nm`, and everything else in raw seconds. So almost every real age
comes out in seconds. The `silent` clause in the same function already uses `age::duration`.

## Build

- `brief` formats the activity age, and the `nudge in` time, with the same human formatter as
  `silent` (`crate::age::duration`). `config::show_duration` stays for config display.
- Grep for every other use of `config::show_duration` that formats an elapsed time or a time left,
  rather than a configured value, and switch those too. List what you changed and what you kept.

## Tests

- `brief` for an agent interrupted 72090s ago reads `interrupted 20h` (or whatever `age::duration`
  gives at that size), not `72090s`.
- A `nudge in` time that isn't a whole minute reads as a time.
- Config display of a configured duration is unchanged.

## Done when

The TUI and `clank status` show agent state ages as times.

## As landed

`brief` uses `age::duration` for the activity age and the `nudge in` countdown. A source search
found no other elapsed-time or countdown use of `config::show_duration`; `config.rs:721` keeps the
exact form for config display. One test covers the reported 72090s → `20h`, a 150s countdown →
`2m`, a future `since` saturating to `0s`, and the config formatter staying exact. Each call site was
mutation-checked against it.
