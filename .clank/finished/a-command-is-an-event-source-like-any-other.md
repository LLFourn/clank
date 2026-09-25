# a-command-is-an-event-source-like-any-other

Anything that can print JSONL can feed the same durable event log
GitHub does.

> "you configure the command and its arguments, it gets restarted with
> an exponential backoff if it terminates. It outputs jsonl … github
> and this get normalized into the same data format … github type
> events just get a carve out and get special treatment in clank tui
> etc." — lloyd

## Why

A command source WAS one process whose EXIT was the event
(`run_command_source`, `cli/wait.rs`): clank spawned the argv, read
stdout and stderr into a bounded 1 KiB ring, and when the root child
exited it sent one `CommandEvent`. Fire-and-forget — nothing durable,
no identity, no ack. If no one was waiting at the moment it fired, it
was gone.

GitHub events got the opposite deal: a write-ahead log under
`.clank/agents/<label>/events`, an explicit handled/unhandled
lifecycle, at-least-once presentation, and `clank events ack`.
Anything that was not GitHub was second class.

## Where the line falls

Settled by lloyd, and it is the spine of this plan:

**The BACKEND owns everything about its source.** How it gets events,
how it caches them, what counts as new, any baseline, and ALL
filtering. How it resumes, and how it avoids resending. Its only
obligation to clank: every line carries an `id` unique within that
source. It may emit the same event more than once.

**CLANK owns one thing: this agent's relationship to each event.**
Whether it is in the inbox, deduplicated by `id` EQUALITY, whether it
is handled, and waking the agent while anything is unhandled. Ids are
compared for equality and nothing else — never ordered, never used as
a resume point.

Everything below follows from that, including several things an
earlier draft of this plan built and this one deletes: clank does no
filtering, so no filter answer can change, so there is no decision
window and no checkpoint to hand back.

## The build

### Milestone 1 — the command source streams events

**Long-running, not one-shot.** The configured argv is spawned and
expected to STAY up, printing one JSON object per line on stdout.
Each line is one event; stderr is diagnostics.

    {"id": "...", "kind": "...", "content": "...", "at": 1730000000}

- `id` — REQUIRED and unique within the source. Clank tests it for
  equality and nothing else.
- `kind` — optional, defaulting to the source's NAME. It only labels
  the event now that clank does no filtering.
- `content` — optional, with a fallback display when absent.
- `at` — optional producer timestamp; ingest time otherwise.

A line missing `id` is refused and said so. A malformed line is
reported and the stream CONTINUES — one bad line must not cost every
later event.

**The inbox is the SAME log GitHub writes.** A command event is an
`EventLog` row under `.clank/agents/<label>/events`, so
`clank events list`, `show` and `ack` find it by the directory glob
they already use and an agent has a supported way to finish one. A
private store beside that log was how an earlier version produced
events that could wake an agent forever and never be handled (codex
a43bd85). A presented event carries its `source@seq` ack selector,
stamped at presentation time from the row's own sequence — the WAL
stores no copy, exactly as it stores no copy of a watch's prompt.

**Deduplication is unbounded.** A duplicate id is never stored as
another logical event, however old. Not "never presented twice": the
event it duplicates is STILL re-presented on every arm until it is
acked, and conflating the two would trade at-least-once away for
dedup (codex 1bf9001). Nothing about a backend's replay behaviour is
clank's to police, so there is no horizon past which a re-sent event
comes back. That is why an id INDEX lives beside the log rather than
in it: the WAL's identity retention is capped at `LOG_IDENTITY_CAP`,
sized for GitHub's feed, and an id that aged out of it would come
back as new. One file per id, named by a hash of it, holding the id
verbatim so a read can tell a collision or a truncated write from a
miss. Damage is named and treated as NOT seen, because the replay is
what repairs it; suppressing it would lose the event permanently.

**A state directory per source**,
`.clank/agents/<label>/sources/<name>/`, keyed by the config's name,
created by clank and handed over in `CLANK_SOURCE_STATE_DIR`. Clank
NEVER reads it. A backend that wants a cursor keeps it there.

**Restarted with exponential backoff.** Growing delay, a cap, and the
streak RESET after a run long enough to count as healthy — or a
producer merely restarted nightly creeps to the cap and stays there
looking fine. Restarting on every wake is acceptable: making that
cheap is the backend's job, and the state directory is how.

**A source that cannot stay up surfaces.** After a threshold, a
failure event enters the log like any other, so an agent waiting on a
producer that will never start finds out. ONE outstanding report per
source: an unacked row already wakes the agent on every arm, so a
second is a nag — and once acked, a later breakage is news again,
which is why this cannot go through the id index. The failure kind is
clank's own and a producer cannot forge it.

**Spawned by the lease holder only — and everyone else TAILS.**
`IngestLease` is per-source and key-generic. Without it, two
`clank wait`s for one agent would each spawn the user's command AND
each allocate sequence numbers from its own handle, so two events
would share a seq and one ack would cover both. A contender that
merely gave up was its own bug: nothing else tails command rows for
that wait, so it received neither the backlog nor anything logged
afterwards and never took over when the holder left. It follows the
log, retries the lease, and carries its presented identities into the
takeover — the lifecycle `github/supervisor.rs` already ran.

**The arm LOADS the log before it appends.** That is what restores
the sequence allocator from the persisted records; a supervisor that
only read rows re-issued seq 1 on every arm. It is also the path that
quarantines a corrupt log — and quarantine takes the id index with
it, because the index is derived and an entry that outlives its event
refuses the producer's replay of a pending event the inbox no longer
holds.

**A source that cannot run fails the ARM.** An empty argv or a bad
glyph refuses before the wait starts, rather than printing from
inside a task that then returns and leaving a healthy-looking wait
with a hole in it.

**The glyph is config, not per-line**, validated against Unicode
data: one scalar, not a control, not zero-width, one display column.
An untrusted producer never reaches the renderer's layout.

Validated at every READER, not once at load. The log and the TUI join
the CURRENT config, independently of any wait, so an operator editing
a glyph after the fact reaches the renderer with nothing having
checked it — and "validated at load" was the belief that left that
hole open (codex 9a53f39). One function, called by the arm and by
each reader; a refused glyph costs the glyph, never the event.

**Content is untrusted text** and goes through `one_line` — first line
only, control bytes dropped — before it reaches a TUI row or the web
page.

### Milestone 2 — one shape, with a GitHub carve-out

Every READER sees one shape: source, kind, content, time,
instructions, ack selector, failure — plus GitHub's own extras (PR
link, number, actor, sub-kind, content ref) in a carve-out the
inspector and the renderers special-case. One projection, and adding
a backend means one new arm in it and none anywhere else.

Done SECOND, so the shape is designed against two real
implementations rather than one and a guess.

**The PERSISTED variants stay separate, and this is the finding, not
a shortcut.** `content` already means two different things on the
wire: a typed `ContentRef` object on a GitHub row
(`{"ref":"review","number":600,"id":…}`) and a plain string on a
command row. One field cannot be both, so a merged variant would
stop parsing every GitHub row already written — a durable-format
migration whose blast radius is exactly the pending work this plan
exists to protect. Merging the wire needs a hand-written
deserializer; the readers, which is where the duplication actually
hurt, need none.

Readers, and the inventory was WRONG the first time. It counted
"6 production readers outside `github/`" and wrote off the sites
inside `github/` as constructors. `github/timeline.rs` is not a
constructor — it is THE presentation reader, the one that feeds the
TUI log and the event page, and it filtered every non-github row out
with a comment saying why: "the github wait path only logs github
events today, so this is belt." That stopped being true in milestone
1, and the belt became a silent drop: a command event stayed pending
in its WAL and never appeared on screen (codex 4e47a2a).

The lesson is the measurement, not the miss. A directory is not a
boundary; what a file DOES is. The readers are `events.rs`,
`wait.rs`, `status_tui/scroll.rs`, `github/timeline.rs` and its
consumers — `log.rs`, `status_tui/render.rs`, `status_tui/mod.rs`.

**Which backend produced a row is CARRIED, never inferred.** Reading
it off the carve-out — GitHub if a url, number or actor is present —
was wrong in both directions: a sparse GitHub row read as a command
event, and a command source named like a repo read as GitHub. Merge
aliases are namespaced by it too, or a source named `o/r` emitting id
`123` merges with GitHub repo `o/r` at feed id `123` and one ack
reaches both. And `--no-github` filters on it, rather than discarding
the whole timeline — which was the same thing only while GitHub was
the only backend.

## Tests

- A duplicate id never becomes another logical event, and its AGE
  does not matter: ingest far past `LOG_IDENTITY_CAP`, assert the WAL
  has forgotten the first id, and re-send it. It stays suppressed —
  which also proves the index, not the WAL, is doing the work.
- Unacked work is re-presented on every arm until it is acked —
  asserted in the SAME test as the dedup, so "stored once" cannot
  quietly replace the delivery contract.
- The whole public lifecycle, through `events.rs`'s own path rather
  than the WAL: a command event is listed, addressed by the exact
  selector the WAKE handed the agent, acked, and then stays acked
  against the producer replaying it.
- A damaged index entry is named with its path and id, and the replay
  repairs it; an unreadable entry is damage, not absence.
- A broken source reports once while the report is owed, and again
  once it has been acked.
- The state directory exists before the command starts and its path
  reaches the environment; clank reads nothing from it.
- A line with no `id` is refused and named; a malformed line is
  reported and the NEXT line still arrives.
- `kind` defaults to the source name, and an absent `content` still
  renders.
- A glyph that is a control, zero-width, or wider than one column is
  refused, naming the source — at the arm AND at every reader,
  including one edited after the events were logged and viewed with
  no arm in between. The event survives; the glyph does not.
- Backoff grows, caps, and resets after a healthy run; a source that
  keeps dying produces a failure event rather than silence, and a
  producer cannot forge one.
- A producer whose descendant holds stdout open is still noticed when
  it dies, and restarted — reading to EOF would strand it.
- Only the lease holder spawns the child.
- Two arms of the real supervisor: the second does not re-issue the
  first's sequence, and an ack of the first does not cover it.
- A crash between the log append and the index write does not
  duplicate the event; a quarantined log does not leave an index
  entry refusing the replay.
- A non-holder tails, receives what the holder logs, takes over when
  it leaves, and never runs a second producer.
- After milestone 2: a command event and a GitHub event project to the
  same row, and the GitHub one still carries its PR link, number,
  actor and sub-kind. Asserted through `timeline_snapshot` and the
  TUI event page, not only the CLI text renderers — the reader that
  dropped command events was the one no test reached.
- A command source named like a repo, emitting an id a GitHub watch
  also has, stays a separate event with its own ack target.
- A WAL of REAL persisted bytes — lifted from a live agent's log, not
  round-tripped through today's types, which would prove only that
  the struct agrees with itself — still loads WARM, with its
  un-baselined row still pending and its typed content ref intact.

## Out of scope

- GitHub's ingest. It keeps its `Obs` records, its baseline and its
  bounded identity handling: those answer "which feed events are
  new?", which is its own question. Moving them behind this boundary,
  or making GitHub an ordinary command backend, is a later plan.
- `clank send` and the message inbox. That is state, not events.
- Webhooks or a relay for command sources. Stdout is the transport.
