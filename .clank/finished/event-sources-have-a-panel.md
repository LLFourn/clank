# event-sources-have-a-panel
# Event sources have a panel in the TUI

## Why

An agent's event sources are command producers, timers and GitHub watches, configured in
its `wait_events`. Their events show in the timeline with their glyphs, but the sources
themselves are invisible in the TUI:
- whether a command source is running, and its pid;
- what a source is configured to do;
- a source's recent events on their own;
- a way to restart a stuck producer.

## What to build

- **A small region above the LOG** listing the configured event sources that are active, one
  row each. A row shows:
  - the same glyph its events carry in the timeline (the configured glyph, or the backend's
    default);
  - the source's name;
  - the agent it belongs to;
  - for a command source, whether its producer is running.

  Messages are left out: they are always on and need no config. Hidden when there are no
  sources.
- **Selectable, like the queue rows.** Enter opens a source's page. It shows:
  - the source's configuration, read-only: kind, command or schedule, reply and receipt
    commands, interrupt setting and hint, glyph;
  - for a command source, its producer's pid and state, from the monitor that hosts it;
  - its recent events: the timeline filtered to this source only, newest first, opening
    an event's page as the main timeline does;
  - **Restart**, for a command source, behind the usual confirm. It stops the hosted
    producer's process group and lets the host start it again, through the same core the
    monitor uses. It is not a reimplemented kill.
- **GitHub is included when configured.** A GitHub watch is shown with its glyph and its
  config (repo, what it watches, poll interval, prompt) and its recent events. It has no pid
  or restart, being polled rather than a process.
- **Read from what exists.** The config comes from the agents' configs. Producer state
  (pid, running, last exit) comes from the monitor's hosted-producer records. Events come
  from the same timeline projection as the LOG, filtered by source. No new polling: the panel
  refreshes on the status TUI's existing wakes.

## Acceptance

- With a command source, a timer and a GitHub watch configured, the region shows three rows
  with their timeline glyphs. Messages are not listed.
- A command source's page shows its config, its running producer's pid and its recent events,
  and only its own events.
- Restart, after confirm, ends the producer's process group; the host starts a new one (a new
  pid appears) without restarting the TUI.
- A GitHub watch's page shows its config and events, and offers no pid or restart.
- With no event sources, there is no region.

## As built

- **Identity.** `cli::source_status::ConfiguredSource` is a configured
  source as owner plus storage key (`source_key` for each kind), never its
  display name.
  - Its icon is the timeline's own `log::badge_of`.
  - Its events are the merged events with a member in that agent's log of
    that key, or an outgoing reply from one (`produced`, `events_of`). Two
    agents' same-named sources stay apart.
- **Lifecycle, from the supervisor itself.** `EmissionSink` can carry a
  reporter (`reporting`), which the monitor gives the sinks of its hosted
  sources.
  - The command-source loop reports `Lifecycle` at its real transitions:
    following another host's log, settling a deferred retirement, running
    with the command's pid, and backing off with exit code or launch
    error, failure count and deadline.
  - An agent's own `clank wait` has no reporter.
- **Published outside the repo.** The monitor keeps the last lifecycle per
  hosted source and publishes an `Account` to
  `runtime_path(repo, "sources.json")`:
  - the account holds each source's accepted-config fingerprint and
    lifecycle, stamped with the monitor's pid and process birth token;
  - it is published only when it changes, and each publish beats the
    observers;
  - a lifecycle change is not a repo write, so it wakes no wait, gate or
    monitor.

  `source_status::live` believes the account only while the published
  monitor is that very process. Otherwise it is `Unavailable`: "monitor
  not running", never a stale pid. Reading never starts a monitor. The
  account's state is part of the status input signature, so a beat with a
  new account rebuilds the snapshot.
- **A rejected edit is said.** The monitor keeps the previously accepted
  host. The page compares that fingerprint with the file's config and
  says the earlier config still runs.
- **Restart is a monitor request.**
  - It is `Who::Control { Restart { author, key, accepted } }`, `PROTO`
    12: one hello, one `Done` or `Refused` note.
  - The monitor's tasks are now keyed by storage key (`Hosted.tasks`,
    `EventSources::into_tasks`). `restart_source` revalidates the author,
    the source and the accepted fingerprint, aborts only that source's task
    (its drop retires the whole process group), and waits up to the settle
    budget. It then spawns just that source again.
  - The replacement cannot start a run before the old group is retired and
    its lease free, which the existing retirement and lease enforce.
  - Its log, dedup index, backend state and pending rows are untouched. A
    pending backoff dies with the old task. Siblings keep their processes.
- **TUI.**
  - A SOURCES region sits above the log (`PanelRow::Source`).
  - `Mode::SourcePage` shows the config, read-only, then Restart where it
    applies, then the source's events newest first, in a window that keeps
    the cursor's row drawn. Enter on an event opens its event page, whose
    Back returns to the source page (`event_beneath`).
  - The restart confirm carries the fingerprint it was shown to the
    monitor.
  - A refresh closes the page if its source is gone, and clamps the
    cursor otherwise.
- **Not built.** Timers and GitHub have no pid or restart. GitHub shows
  its config, and its events with "hosted" or "not hosted".

### After review

- **One task per storage key.** `resolve_wait_event_sources` refuses two
  command or GitHub sources resolving to one storage key, as it refused
  duplicate timers. The monitor therefore keeps its last accepted host when
  an edit introduces them. `EventSources::into_tasks` aborts a handle a
  duplicate would overwrite, rather than detach it.
- **Lifecycle belongs to a task instance.**
  - Each hosted task is a `HostedTask` with an instance number, and every
    report carries its instance.
  - `record_lifecycle` keeps only the current task's reports, and
    `forget_replaced` drops a replaced or removed task's state on every
    reconcile. A same-key replacement starts with nothing said, and the old
    task's late word is refused.
  - A supervisor that ends reports `Stopped` with why (an empty command, a
    lease it cannot take, a log it cannot open), so nothing reads
    "starting" or an old pid for ever.
- **Identities through refresh.**
  - The panel anchor knows drafts and sources (owner and key), so a refresh
    keeps the cursor on the source it was on.
  - The source page keeps its cursor on Restart or on its event by page key
    (`SourcePick`, `rebind_source_sel`), as events arrive and Restart comes
    and goes. It falls back to the same row only when that row is gone.
- **The source page scrolls as a whole.**
  - Under the pinned title and hint, configuration, state and rows scroll
    as one, and values wrap under their label.
  - Its window keeps the cursor's row drawn, raised by `lift` lines.
  - `Up` on the first row, `PageUp` and `Home` raise it, so a reader can
    scroll up through all the configuration in any pane height.
  - Any cursor move, `PageDown` or `End` sets it back to zero and brings
    the row into view (`source_moved`).
  - A refresh keeps both the lift and the row's identity.
