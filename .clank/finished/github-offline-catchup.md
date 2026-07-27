# github-offline-catchup
# Github event inbox: write-ahead seen log with explicit ack

## Problem

`poll_loop`'s state — the bounded `SeenSet` cursor, the ETag, and the
`X-Poll-Interval` floor — is process-local (`github_events.rs`, locals of
`poll_loop`). Every `clank wait` arm starts cold: the first fetch is a
baseline that emits nothing, so any github event that fires while no
wait is armed is swallowed forever. Worse, an event that IS emitted is
gone the moment it's delivered: if the wait dies (or the agent crashes)
between delivery and reaction, nothing re-presents it.

Wait downtime and delivery loss are routine, not exotic:
- the window between a wait delivering work and the agent re-arming;
- background waits being externally killed (observed: harness reaps
  them on a ~30-minute cycle) and re-armed;
- the machine or session being offline outright.

## Model

A per-agent **write-ahead event log** with an explicit
handled/unhandled lifecycle — an inbox, not just a dedup cursor:

1. **Log first.** Every github event clank ingests — from BOTH
   transports (poll feed and realtime relay) — is appended to the log
   BEFORE it is presented to the agent. A crash between append and emit
   leaves the event logged-unhandled, so it is re-presented on the next
   arm: at-least-once delivery, never silent loss.
2. **Present unhandled.** `clank wait` presents the log's unhandled
   events: at arm time, any unhandled backlog returns IMMEDIATELY as
   wake items (before live polling even starts); live arrivals
   append-then-emit. An agent with unhandled events is always woken.
3. **Explicit ack.** Events stay unhandled until acked via the CLI. The
   skill teaches the loop: react, then ack — an unacked event wakes you
   again by design.

## Design

### Storage & locking

- Per (agent, source): `.clank/agents/<label>/events/github-<owner>-<repo>.jsonl`
  plus a small `state.json` (`{etag, poll_interval_floor, saved_at}`,
  atomic rewrite per successful tick; failed ticks touch nothing).
- Logs are **per agent** — the agent's own wait process is the sole
  ingest writer, which is the concurrency story. The ack CLI is a
  second appender: the log is event-sourced JSONL — records are
  appended lines, `unhandled` = inbox record with no ack record.
- Lock protocol (codex 899f915): ALL writers participate in one
  sidecar-lock discipline — every append (ingest and ack alike) holds
  the lock SHARED; compaction/rotation holds it EXCLUSIVE, so a
  compactor can never snapshot-and-replace the file while a hot append
  is in flight and drop that line. Single-line `O_APPEND` writes under
  the shared lock; a concurrent append-vs-compaction race test is part
  of acceptance.
- Record forms — with the invariant that each newly observed feed
  event gets EXACTLY ONE atomic primary record, written in one append
  (codex df3f1a7: a two-append observation+inbox transaction lets a
  crash between the appends rebuild the cursor with a feed id whose
  event was never presented — permanent silent suppression):
  - **inbox** `{seq, logged_at, transport, feed_id?, action_key?,
    item: {…}}` — the primary record when the event is PRESENTABLE.
    `item` is the COMPLETE wake payload (`WaitItem::GithubEvent`:
    kind, detail, repo, number, title, url, actor…) so the backlog
    re-presents byte-faithfully. `feed_id` only for poll events; relay
    events carry `action_key` only (their payloads have no feed id —
    codex 82a221a); keyless relay events carry neither identity and
    are always presented.
  - **observation** `{obs: feed_id, logged_at, baseline?}` — the
    primary record when the observed event is NOT presentable:
    filtered kind, own action, unclassifiable, action-key-deduped, or
    part of the first-run baseline (below). Cursor-rebuild only.
  - **ack** `{ack: seq, at}` — secondary; refers to an inbox seq.
  - The feed-id cursor rebuilds from the feed ids of BOTH primary
    forms, so one append is always the complete durable decision for
    its event.
- Rotation/compaction: when the acked/observation prefix exceeds a
  fixed cap, compact keeping the newest SEEN_CAP identities (the feed
  itself retains only ~300 events, so older identities are dead weight
  for dedup) and ALL unhandled inbox records regardless of age.

### Durable horizons (codex 82a221a)

Two INDEPENDENT structures, both rebuilt from the log at arm and
maintained live:
- **Feed-id cursor** (poll only): seeds the existing `SeenSet` from
  observation AND inbox records' feed ids. The reconcile keeps the
  existing fully-seen-PAGE boundary state machine unchanged —
  pagination does NOT stop at the first individually seen event, so a
  delayed unseen event behind a seen one still emits; only the
  SeenSet's initial contents change.
- **Action-key horizon** (both transports): seeds the `Coordinator`, so
  a relay event that emitted before a restart cannot double-emit even
  if the poll never observes it — the relay ingest appended its action
  key durably at emit time.

### Wake & baseline

- Arm: load log → return unhandled backlog immediately as wake items →
  start live polling/relay with horizons seeded from the log.
- First-ever run (no log): baseline exactly as today, and every
  baseline feed event is appended as an OBSERVATION record
  (`baseline: true`) — atomically one append per event, no inbox
  entry, no ack machinery (codex df3f1a7). History gives the cursor
  durable contents without ever being presentable.
- Failure domains are separate (codex 899f915): `state.json` is cache
  metadata — if it's corrupt, reset ONLY the ETag/floor (warn once)
  and keep the WAL, its cursor, and its unhandled events intact; the
  at-least-once guarantee never rides on cache metadata. Only a
  corrupt WAL itself degrades to a fresh baseline (one warning).
  Never fail the wait over state problems.
- Overrun on reconcile (existing `overrun` flag): deliver what the feed
  still has, one warning naming the possible gap; synthetic gap items
  out of scope.

### CLI

`clank events` group (agent inferred from the session binding, like
`feedback`):
- `clank events list [--all]` — unhandled by default; `--all` includes
  acked; `-j` JSON. Inspection = reading the log.
- `clank events ack <seq>...` — mark handled (appends ack records).
- `clank events show <seq>` — full record.

### Skills

- `clank-master` skill: mention the inbox and the react-then-ack loop.
- A dedicated github-handling skill (installed by `clank setup`
  alongside the existing ones) owning the full GH flow: what the wake
  items mean, the inspect/ack loop, catch-up semantics, realtime
  caveats. The master skill links to it rather than duplicating.

### Out of scope

- Cross-agent shared logs; retention beyond rotation; synthetic gap
  wake items; acking on behalf of another agent.
- Duplicate concurrent waits for one agent can interleave ingest;
  last-writer-wins is accepted here — the agent-start singleton lock
  (separate proposal) is the real fix.

## Acceptance

- WAL ordering: every emitted wake item has its log record appended
  before emission (injected fetcher/relay; no network, no binaries).
- Relay-only event → process restart → poll never observes it → no
  double emit (action-key horizon rebuilt from the log).
- An event fired between two wait runs is presented by the second run.
- Unhandled backlog at arm returns immediately AND byte-faithfully
  (the re-presented `WaitItem` equals the originally emitted one);
  after `clank events ack` it stops waking; unacked events re-present
  on every arm.
- A filtered/own-action/unclassifiable feed event writes an
  observation record and never re-emits after restart (cursor rebuilt
  from both primary forms).
- Crash-point coverage: for every prefix of the append sequence around
  a presentable event (before its append / after append, before emit /
  after emit), the event ends either unhandled in the WAL or absent
  from the durable cursor — i.e. always recoverable, never
  cursor-present with no inbox record (codex df3f1a7).
- Append-vs-compaction race: concurrent appends during a compaction
  pass lose no records (threaded in-process test over a temp dir).
- Corrupt `state.json` with a valid WAL: ETag/floor reset only —
  unhandled events still present, no re-baseline, no duplicate emits.
- Fully-seen-page reconcile semantics preserved: a delayed unseen event
  behind an individually seen one still emits (existing poll tests keep
  passing with the seeded cursor).
- Baseline run logs one observation record per feed event and wakes no
  one.
- `clank events list/ack/show` work while a wait is live (append-only +
  flock-guarded compaction); corrupt artifacts degrade to baseline with
  one warning.
- Skill assets updated: master mention + dedicated GH skill installed
  by `clank setup`.
