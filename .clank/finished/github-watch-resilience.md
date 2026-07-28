# github-watch-resilience
# Github watching that survives wedged connections; baseline history in the timeline

## Problem (diagnosed live)

fsctl watches two repos; `LLFourn/frostsnap` never completed a single
tick (no WAL, no state) while its sibling ticked happily from the
same process. Experiments: the feed is one page (73 events), parses
clean, and a fresh probe wait baselines it in ~1s. The cause is in
clank: `shared_session()` builds the reqwest client with NO timeouts
at all — a request wedged by a network transition or laptop sleep
(routine for overnight-parked waits) hangs that source's task
FOREVER: no error, no retry, while a sibling source keeps ticking.
The rare "error sending request" lines are the lucky cases where the
OS actively reset the connection. `acquire_github_token`'s
`gh auth token` subprocess has the same unbounded-await hazard.

Separately (same surface): baseline records are identity-only
observations, so events from before the first arm can never appear in
`clank log` / the TUI timeline even though they have timestamps —
the payload was deliberately dropped for single-append atomicity.

## Design

### Bounded transport — the deadline lives at the ownership boundary

- The DEADLINE is application-level, owned where the injection seam
  is (codex b1b1d91): `GithubSession` wraps every send — any
  `HttpSend`, not just the production reqwest one — in
  `tokio::time::timeout` with an INJECTABLE duration (default 30s),
  and the token-acquire path wraps its injectable `AcquireFn` the
  same way (default 15s). A hung injected fetcher or acquirer in a
  paused-clock test proves the loop-level property directly; a
  reqwest-only limit could not (a test HttpSend bypasses the client
  entirely, leaving poll_loop awaiting forever).
- Timing out the acquire DROPS the subprocess future; the existing
  `kill_on_drop` child config is what reaps the `gh auth token`
  process — stated as an invariant and pinned (the child must not
  outlive the timeout).
- The production client ADDITIONALLY gets `connect_timeout(10s)` and
  a total request `timeout(30s)` as transport-level belt. Ticks are
  60s+ so the bounds can never overlap the next tick. A wedged
  connection becomes a tick error either way, and poll_loop's
  existing retry-next-tick path recovers.

### Baseline history with payloads (single-append preserved)

- Baseline events append INBOX records carrying the full payload plus
  `baseline: true` — still exactly ONE atomic append per event
  (the codex df3f1a7 invariant): the flag makes the record BORN
  HANDLED, so fold never backlogs it, the wake path never re-presents
  it, and no separate ack record is needed. Additive field; v stays 1.
- read_rows exposes `baseline` on EventRow (rendered as handled;
  `events list --all` shows them acked) and the timeline layer
  carries it on MergedEvent. MERGED aggregation (codex b1b1d91): an
  entry is `baseline` only when EVERY copy in the component is a
  baseline record — one agent's cold arm may baseline an event
  another agent received live, and a live copy (especially an
  UNHANDLED one) must keep the entry rendered normal/open, never
  dimmed as pre-watch history. Non-presentable observed ids at
  baseline (filtered kinds, unclassifiable) stay identity
  observations as today.
- Compaction: baseline inbox records are handled records — they age
  through the existing DISPLAY_CAP retention; horizons unchanged
  (inbox records already carry identities).

## Out of scope

- Retrying within a tick (the next tick IS the retry).
- Backfilling payloads for existing baseline observation records.
- Realtime/relay changes.

## Acceptance

- A hanging injected HttpSend: the tick fails within the injectable
  bound (paused clock) and the NEXT tick proceeds — the deadline is
  proven at the session/loop layer, not the transport.
- A hanging injected AcquireFn fails within its bound and the source
  retries on cadence (existing fail-closed path); the dropped acquire
  future reaps its kill_on_drop child (pinned).
- The real client config: a request against an in-process tarpit
  listener errors within the transport bound (no hang).
- First arm on a cold source writes payload-bearing `baseline: true`
  inbox records: nothing emits, `load()` reports them handled (empty
  unhandled), the second arm is warm, and the crash-prefix invariant
  holds (single append per event, no prefix leaves a baseline record
  presentable-unhandled).
- `clank log` shows pre-watch events dimmed at their event time;
  `events list` stays empty (they're handled); `list --all` shows
  them acked.
- Mixed-copy aggregation: a component holding one agent's baseline
  copy and another's LIVE copy is not `baseline`; with the live copy
  unhandled the entry stays open (never dimmed) — pinned both ways.
- Existing identity-observation baselines (already-written logs) load
  exactly as before.
