# log-timeline-github-events
# Interleave github events into the clank log and status --tui timelines

## Problem

Github events live only in the per-agent inbox (`clank events`); the
repo's narrative surfaces — `clank log` and the status TUI's timeline
— show commits and reviews but not the external events the team
reacted to. "A PR comment arrived, then master committed a fix" is
currently two tools and a mental join.

## Design

### Event time, best effort

The inbox record gains an OPTIONAL `event_at` (epoch seconds): the
event's github-side timestamp, captured at classification — the feed's
`created_at`, or the webhook payload's per-kind timestamp
(`comment.created_at`, `review.submitted_at`, push `head_commit
.timestamp`, …) when present. Additive optional field: `v` stays 1
(event-log-format-compat's rules). Interleaving uses `event_at`,
falling back to the record's `at` (ingest time) when github's own time
wasn't determinable — "in so far as you can figure out" is the
contract, never a blocker.

### Bounded display retention through compaction

Today compaction folds EVERY acked inbox record into a payload-free
identity observation — under this plan that would erase all handled
events from both timelines at the next threshold, making the
"event, then fix" narrative depend on when compaction ran (codex
3097f71). The store gains a presentation-retention bound: compaction
preserves the newest DISPLAY_CAP handled inbox records VERBATIM —
payload, identities, event time — together with their ack records
(dropping the ack while keeping the inbox would re-present handled
events as unhandled after reload); only handled records older than
the bound fold to identity observations as today. Unhandled records
remain lossless at any age, the feed/action identity horizons are
maintained independently and unchanged, and baseline/dedup
observations never enter the timeline.

### One merged repo view

A shared read layer (usable by both surfaces) merges EVERY agent's
event logs under `.clank/agents/*/events/`:
- Rows come from the existing read-only inspector path (foreign
  records skipped, corrupt logs reported and skipped — never
  quarantined from a viewer).
- The SAME github event can sit in several agents' inboxes (each
  watcher logs its own copy) and arrive by both transports. Merging is
  CONNECTED COMPONENTS over shared stable aliases (codex 8354e51),
  never a precedence key: each row contributes the aliases it has —
  `(repo, feed_id)` and/or `(repo, action_key)`, both repo-scoped
  (action keys are source-local and collide across repos) — and rows
  sharing ANY alias join one component. A poll row carrying both
  identities is the bridge that joins its relay twin (action key only)
  into the same entry.
- KEYLESS rows (no feed id, no action key) never merge — one row, one
  entry, mirroring the coordinator's deliberate emit-always treatment
  of missing identity. No manufactured buckets: distinct real events
  can share (repo, kind, number, time).
- Merged lifecycle: an entry is UNHANDLED while ANY seen-by copy is
  unhandled, handled only when ALL copies are acked — the TUI's
  open-work marker is deterministic under mixed ack state.
- Each merged entry: timestamp (earliest across copies, per above),
  repo, kind/detail, number/title/actor/url, seen-by agents,
  handled/unhandled.

### clank log

- Github events interleave into the chronological timeline by their
  timestamp, rendered distinctly (a `gh` badge/prefix; `--oneline`
  keeps them to one line mirroring the events CLI's describe()).
- `--no-github` filters them out entirely (flag name bikeshed
  welcome at review; default is included).
- `-j`: a typed `github_event` entry alongside the existing kinds,
  carrying the merged-entry fields.

### status --tui

- The TUI's timeline/log page interleaves the same merged entries with
  the same rendering, unhandled ones visually marked (they're the
  team's open external work).
- Reuses the shared read layer; no second merge implementation.

## Out of scope

- Acking from the log surfaces (the events CLI owns mutation).
- Backfilling `event_at` for records logged before this plan (they
  fall back to ingest time).
- html rendering (follow-up if the merged view proves useful).

## Acceptance

- Ingest captures `event_at` from feed events and from at least the
  comment/review/push webhook shapes; absent timestamps fall back to
  ingest time (tested per shape, injected fetcher/relay).
- `v` remains 1; a pre-`event_at` record still loads (additive-field
  rule holds).
- The merged view joins a poll-and-relay pair through the poll row's
  alias bridge AND the same event across two agents' logs into one
  entry (earliest time, both agents listed), tested over temp dirs;
  same action key in two DIFFERENT repos stays two entries.
- Keyless rows never merge, even when (repo, kind, number, time)
  coincide.
- Mixed ack state across agents: the merged entry stays unhandled
  until every copy is acked (tested both ways).
- `clank log` shows gh events in timestamp order among commits and
  reviews; `--no-github` removes exactly them; `-j` carries the typed
  entries; `--oneline` stays one line each.
- The TUI timeline shows the same entries with unhandled marked, via
  the shared layer (no duplicated merge logic).
- A corrupt or foreign-heavy log degrades the view (skip + one
  notice), never a crash or a quarantine from the read path.
- Compacting a mix of handled and unhandled events: the newest
  DISPLAY_CAP handled entries stay displayable WITH ack state intact
  (no re-presentation after reload), unhandled entries remain
  lossless, older handled entries age out exactly at the bound, and
  the identity horizons are unaffected.
