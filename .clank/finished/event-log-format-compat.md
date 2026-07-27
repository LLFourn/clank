# event-log-format-compat
# Make the event-WAL format evolvable: tolerate unknown record kinds

## Problem

The github event WAL (github-offline-catchup) is additive-field safe
(serde ignores unknown fields; new fields can default) but NOT
new-record-kind safe: `github_event_log::Record` parses strictly, so a
line whose `t` the running binary doesn't know — a newer binary's new
record kind read after a downgrade, or a mixed-version machine where
two clank builds share one agent dir — is indistinguishable from
corruption. Today that quarantines the whole log and re-baselines,
throwing away a valid cursor and (from this binary's view) the
unhandled backlog. There is also no version marker to hang future
semantic migrations on, and inbox records couple to `WaitItem`'s exact
serde shape: a required-field change there would invalidate every
existing log.

## Design

### Three-way line classification (replaces the current two-way)

Parsing a line now distinguishes:
1. **Known record** — a JSON object with a recognized `t` (and
   `v <= CURRENT`, below) that parses fully → used as today.
2. **Foreign record** — a well-formed JSON OBJECT that is not a known
   record: unknown `t`, missing `t`, `v > CURRENT`, or a known `t`
   whose fields no longer parse. Foreign records are SKIPPED for
   semantics but PRESERVED byte-verbatim: they pass through load
   untouched and compaction re-emits their original lines, so a newer
   binary's records survive an older binary's custody. One warning
   per load names the count. They contribute nothing to horizons or
   the backlog (inherent: this binary can't read them).
3. **Garbage** — not a JSON object at all. Exactly today's rules: a
   torn FINAL unterminated line drops alone; interior garbage
   quarantines to `.corrupt`.

Foreign-line retention is bounded like identities: compaction keeps
the newest LOG_IDENTITY_CAP foreign lines and warns when it drops
older ones — unbounded growth must not ride in on tolerance.

### The version-stable envelope

Three top-level keys are FRAMING, owned by every version forever:
`t`, `v`, and `seq`. A reader may skip a foreign record's semantics,
but it must still honor the envelope — concretely, the seq allocator's
high-water mark advances past any well-formed foreign object carrying
a numeric top-level `seq`, and compaction keeps that reservation while
the line is retained (codex 7e6e585: otherwise a downgraded binary can
re-allocate a newer binary's seq, and one ack then addresses two
different events). Future kinds that need per-record identity MUST use
top-level `seq`; keys other than the three framing keys are private to
their `t`.

### Version marker

- `v` is a REQUIRED field on every record; writers stamp `v: 1`.
  No serde default: no log records exist in the wild yet (the WAL
  shipped within this release cycle), so the format breaks cleanly
  now instead of carrying an absent-means-1 shim forever. A record
  without `v` is simply foreign.
- `v` bumps ONLY for semantic changes to an existing `t` — additive
  optional fields never bump it. A record with `v > CURRENT` is
  foreign (skip-preserve), never corruption.
- A log whose records are ALL foreign is NOT a warm start: it
  baselines (preserving the foreign lines) — a warm empty cursor
  would replay the feed's history as wakes.

### Payload drift is the same mechanism

A future `WaitItem` shape change makes an inbox record's fields fail
to parse — which the three-way rule already classifies as foreign:
skip-preserve, warned, never quarantine, never a crashed wait. No
separate raw-payload machinery is needed; the record's identities sit
out of the horizons exactly like any other foreign record's.

### Format-evolution rules (module doc, enforced by convention)

- New semantics → a NEW `t` (never repurpose an existing kind).
- New data on an existing kind → optional field with a serde default.
- `v` bump only when an existing kind's MEANING changes.
- The atomic-primary-record invariant (one append = the complete
  durable decision per event) binds every future kind.

## Out of scope

- Any change to current record semantics or the lock protocol.
- Cross-version migration tooling (v1 is the only version; the marker
  just reserves the seam).
- On-disk migration: none — no logs exist in the wild to migrate.

## Acceptance

- A line with an unknown `t` (and one with `v: 99`) loads as foreign:
  horizons and backlog unaffected, warning counted once, and the line
  survives BOTH a load-append cycle and a compaction byte-verbatim.
- Seq-namespace safety (codex 7e6e585): a foreign record with a high
  numeric `seq` (both a `v: 99` inbox and an unknown kind) forces the
  next old-format append ABOVE it, across reload and compaction — no
  seq reuse, no ack addressing two records.
- A known `t` with unparseable fields is foreign (skip-preserve), not
  corruption.
- Interior non-JSON garbage still quarantines; a torn unterminated
  final line still drops alone (existing tests keep passing).
- Foreign-line retention is capped at compaction with a drop warning.
- A known `t` whose fields drifted (including a changed `item` shape)
  is foreign, not corruption — wait stays up, other records present.
- New records serialize with `v: 1`; a record lacking `v` is foreign.
- An all-foreign log loads cold (baseline), foreign lines preserved.
- Round-trip: a log interleaving native, foreign, and acked records
  through load → append → compact → load preserves foreign lines and
  native semantics.
