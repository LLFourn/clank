# wal-single-ingest-writer
# Enforce the WAL's single-ingest-writer; tolerate the damage done

## Problem (field report, fsctl)

Two concurrent `clank wait` processes for one agent each ran the same
github source: both opened the WAL, both loaded `next_seq = 4`, and
both appended an inbox record for the same event — two rows, same
seq, same feed_id (plus duplicate obs records ~6s apart from the two
pollers). `clank events ack 4` then dead-ended: duplicate seqs within
ONE source make every id form — bare and qualified — match two rows,
so the resolver says "ambiguous" with no qualified form that works.
The workaround (hand-appending the seq-keyed ack record) worked
because fold's ack set is seq-keyed and clears every duplicate.

The offline-catchup plan documented concurrent-ingest as accepted
last-writer-wins; that assumption was wrong — interleaved ingest
corrupts seq uniqueness, which the ack addressing model depends on.

## Design

### Make the class unrepresentable: a per-source ingest lease

- A second sidecar, `<key>.ingest.lock`, taken EXCLUSIVE (flock,
  non-blocking) BEFORE ANY TRANSPORT EXISTS (codex 0fb262b): the
  lease covers the whole ingest runtime — polling AND the realtime
  relay (relay deliveries append primary records through the same
  gate, and today the relay starts before the poll loop). Only the
  lease holder creates the relay listener, spawns the forwarder, or
  drains deliveries; a non-holder starts neither transport. flock
  dies with the process, so a killed wait releases its lease with no
  cleanup protocol.
- A source that finds the lease held does NOT poll github at all: it
  warns once ("another wait is already watching <repo> for <agent>")
  and TAILS THE WAL instead — on its poll cadence it re-reads the log
  and presents unhandled entries it hasn't presented yet. Both waits
  then present the SAME durable events, every presented event is in
  the WAL (listable, ackable), and github is never double-polled. If
  the leased wait dies, the tailing wait's next cycle takes the freed
  lease and becomes the ingester.
- **Presentation never acknowledges** (stated invariant, pinned):
  `clank wait` — leased or tailing — NEVER writes ack records; an
  event is handled only when the agent explicitly runs `clank events
  ack`. Re-presentation on every arm until then remains the designed
  at-least-once behavior.
- The compaction/append data lock is unchanged — the lease is ABOUT
  ingest exclusivity, not I/O atomicity.

### Tolerate logs the bug already damaged — without erasing events

Same seq does NOT imply same event (codex 0fb262b): the allocator
race can hand one seq to two DIFFERENT feed events, and a blind
first-wins would silently erase a real payload while a seq-keyed ack
cleared both.

ONE canonical model, shared by every consumer (codex 77b5d97): a
row's LOGICAL IDENTITY is its ACTION KEY when present, else its
feed_id, else NONE — action-key-first because that is the
coordinator's cross-transport contract (codex 8d807b7): a poll copy
carries both identities while its relay twin carries only the same
action key, and the model must recognize those as one event. One
`ack selector matcher` implements "which rows does this ack cover":
a plain ack covers every row of its seq; a discriminated ack covers
exactly the rows whose logical identity equals its discriminator.
Identity-less rows use a stable hash over the COMPLETE logical event
content (item, event_at, transport) as their discriminator — which
makes rows whose complete content is IDENTICAL one indistinguishable
OCCURRENCE GROUP: they display once and one discriminated ack clears
the group (there is no durable bit left to tell them apart, so
per-occurrence addressing is not claimable — the never-merge rule is
narrowed to identity-less rows with DIFFERING content). fold,
read_rows, the events resolver, AND compaction all call this one
matcher — no consumer may reimplement it seq-keyed.

- Same-(source, seq) rows with EQUAL logical identity collapse; the
  reported field shape (same seq, same feed_id) collapses and acks
  exactly as the workaround did.
- Same-seq rows with different (or no) identities are damage made
  VISIBLE and individually addressable: both rows list, ids gain an
  ordinal (`seq#1`, `seq#2`, source-qualified as needed), and a
  bare-seq ack against distinct rows errors naming the ordinal forms.
- The ordinal form writes the discriminated ack (additive field, `v`
  stays 1); existing plain acks — including the manual workaround —
  read identically to before.
- COMPACTION honors per-row ack state through the matcher: each
  colliding row folds or is retained according to ITS matching acks,
  multiple discriminated acks at one seq survive the rewrite while
  their rows are retained, and an unmatched row remains unhandled —
  a rewrite can never silently drop a still-open event.

## Out of scope

- The general agent-process singleton (`agent-start` lock) — still
  parked as its own proposal; this plan closes the WAL-integrity hole
  it would have prevented, at the store where the invariant lives.
- Rewriting damaged logs (dedup-on-read suffices; compaction
  naturally collapses acked duplicates to identity observations).

## Acceptance

- Two EventLog handles on one source: the second poll-loop-style open
  fails to take the lease; the store-level test proves an exclusive
  lease holder blocks a second and a dropped holder frees it.
- Loop-level: two concurrent poll_loops on one source dir — one
  ingests, the other tails with one warning; both present the same
  logged event, the WAL gains no duplicate seqs, and NO ack records
  exist until `clank events ack` runs (paused-clock, injected
  fetchers).
- The tailing wait presents an unhandled entry the leased writer
  logged, and takes over ingest (lease acquired) after the leased
  wait dies — and only THEN starts its transports; realtime coverage
  pins that a non-holder neither creates nor drains a relay (a relay
  test double, not injected fetchers alone).
- A log seeded with the field shape (duplicate inbox seq + duplicate
  obs, same feed_id) loads with a single backlog entry, single
  display row, intact horizons; `events ack <seq>` resolves and
  clears it; cross-source seq collisions still demand the qualified
  form.
- A damaged log with TWO DISTINCT events at one seq: both list under
  ordinal ids, bare-seq ack errors naming them, a discriminated ack
  clears exactly one, and no payload is ever silently dropped.
- The same case ACROSS COMPACTION: ack one row (discriminated), force
  a compaction, reload — the acked row is folded/handled, its
  discriminated ack survived as long as needed, and the other row is
  STILL unhandled and presentable.
- Two identity-less rows at one seq with DIFFERING complete content
  never merge; their content-hash discriminators address them
  individually. Rows identical across the COMPLETE content (item AND
  event_at AND transport) form one group: displayed once, cleared by
  one discriminated ack — pinned on exactly that case.
- The damaged poll/relay pair: a poll row (feed_id + action key) and
  a relay row (same action key only) at colliding seqs are ONE
  logical event under action-key-first identity — collapsed, one
  ack.
- The ack CLI's append path still works while a leased wait is live.
