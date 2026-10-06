# a-source-is-its-producer
# The sources panel shows one row per source, the roster's agents on it, and who runs each

## Why

In `~/src/life` the TUI's SOURCES panel shows four `voice` rows, three `signal` rows and five
timers:

```
⏰ btc-eth-daily-review  @assistent  hosted
⏰ daily-inbox-review  @assistent  hosted
⏰ personal-tax-spit-bas-2026-10-07  @assistent  hosted
⏰ rama-follow-up-2026-10-13  @assistent  hosted
💬 signal  @assistent  running · pid 47170 · 4h
⏰ virus-meeting-reminder-2026-10-08  @assistent  hosted
🎙voice  @assistent  running · pid 47171 · 4h
💬 signal  @claude  following another host's log
🎙voice  @claude  following another host's log
💬 signal  @codex  following another host's log
🎙voice  @codex  following another host's log
🎙voice  @grok  not hosted
```

An earlier draft of this plan read the repeated rows as one shared producer drawn once per
subscriber, and proposed grouping them. That premise was false (codex on 3180791). Each
author's sources live in that author's own `.clank/agents/<author>/events/`, and the lease is
per directory and key (`IngestLease::try_acquire`, `run_command_source_with`). Identical keys
in two authors are two sources. Checked live in life: the monitor runs assistent's voice and
signal, while `claude`'s parked `clank wait` (pid 46202) and `codex`'s (pid 43748) each hold
their own author's leases and run their OWN voice and signal processes. That makes three
receivers of each. The per-agent states are true. Merging them into one producer would
have hidden exactly the misconfiguration the user needed to see.

What IS wrong with the panel:

1. **Timers are shown.** A timer is a reminder an agent set for itself, built in, with no
   process, pid or restart. The panel is for the external event producers configured for
   the repo. Their events stay in the log.
2. **Agents off the roster contribute rows.** `grok` is not in life's roster, but its
   `agents/grok/config.json` remains, so its `voice` shows as "not hosted".
   `source_status::configured` reads every agent config on disk (`load_all_agent_configs`).
3. **"following another host's log" hides what is running.** It is what the monitor reports
   when the source's lease is held elsewhere, and it says nothing about where. In life that
   "elsewhere" was a stale parked wait running a duplicate receiver. The row should show
   the process that is actually producing.

## What to build

- **No timers.** `configured` lists command and gh sources only. A timer's events remain in
  the log and keep their glyph.
- **Roster agents only.** Sources come from the roster's agents. A config left on disk by an
  agent that is no longer on the roster contributes nothing.
- **One row per source, its agents listed on it.** After the user's direction: the panel
  has one row per source name, e.g. `🎙 voice @assistent,@claude,@codex`, with the one
  agent's state in full, or with several, how many are in each state (`1 running · 2 held
  elsewhere`). This is presentation only. Each agent's source keeps its own log, lease,
  producer and state. Enter on a row one agent configures opens its page. On a row several
  configure, it opens a list of those agents, each with its own state, and Enter there opens
  that agent's page, whose Back returns to the list. Nothing is merged: events, restart and
  config stay per (agent, key).
- **A followed source names its producer.** The producer record beside the log
  (`<key>.producer`: the run's process group and its birth token) says which process group
  is producing. When the monitor reports `Following`, the panel and the source page read
  that record and show the producer's pid. That pid is shown only when the birth token
  proves the group is still the one recorded: `held elsewhere · producer pid 53272`.
  Otherwise the row says the producer is unknown. It never shows a pid from a record whose process is gone or
  reused. Restart stays what it is: the monitor restarts only a task it hosts. A followed
  source's page says the lease is held elsewhere, and offers no restart.

## Acceptance

- A timer source configured on a roster agent produces no row, and its events still render
  in the log.
- An agent config on disk for a label not on the roster produces no row.
- Three agents configuring `voice` share one row naming all three, whose counts add up to
  three. Their list shows each one's own state, so a duplicate running producer is visible
  there, not hidden.
- A followed source whose producer record is live shows that pid. A record whose group is
  gone, or whose birth token no longer matches the live process, shows the owner as
  unknown.
- The page of a followed source offers no restart.
- Mutation-checked: shown timers, on-disk agents, a dropped producer pid, trusting a
  stale or legacy record, ungrouped rows, and the list's Back and cursor each fail a test.

## Out of scope

Leasing, following and the monitor's hosting. What a timer does. life's configuration: its
duplicate `voice`/`signal` sources on `claude` and `codex` are the user's to remove.

## As built

- `source_status::configured` lists command and gh sources of the roster's agents only
  (`agent_store::roster_labels`, now shared with the monitor's `roster_authors`), sorted by
  name and then agent.
- `Live::Monitor` carries, beside the monitor's account, the producers proven for the
  sources it only follows. `command_source::verified_producer` reads the source's own
  `<key>.producer` in its author's events directory, read-only. It accepts only an anchored
  record whose recorded leader is still the same process (`anchored_leader` reports `Same`).
  A legacy, stale, unreadable or missing record proves nothing. It is computed on the
  refresh path (the snapshot build and the sources fingerprint), never per frame.
- Timer events still render in the log with their glyph. The timeline joins glyphs from each
  agent's config itself (`github/timeline.rs`) and never reads `configured`, so the panel's
  filter cannot reach it.
- A followed source reads `held elsewhere · producer pid N`, or `held elsewhere · producer
  unknown`. `restartable` is false for it.
- Panel rows are `input::GroupRef` (backend, name). `Mode::SourceMembers` lists a row's
  agents, its cursor following its agent through a refresh. `open_source_row` and
  `source_back` are the transitions.
