# a-message-is-an-event-like-any-other

## Goal

`clank send` leaves a message; the recipient sees it as a row in the timeline, opens it there, and acks it the way every other event is acked. One event model, one ack, one place to look.

This repo has already made this move once. `1970fc88 [a-command-is-an-event-source-like-any-other]` turned command sources into durable event sources with the same WAL, the same selectors and the same ack as github's. Messages are the family that was left behind.

## Problem

A message and an event answer the same question — *something arrived that wants you* — and the two answer it with entirely different machinery.

| | a message | an event |
| --- | --- | --- |
| stored | one file per message under `.clank/agents/<label>/inbox/`, ordered by a `NNN-` priority prefix | append-only WAL at `.clank/agents/<label>/events/<key>.jsonl`, with a `.seen` index |
| identified by | its file stem, the "handle" | `<source>@<seq>`, and a `MemberKey` per agent copy |
| fanned out by | the SENDER writing into the recipient's directory | one event, per-agent copies, merged into a `MergedEvent` with `members` |
| handled by | `clank inbox done <handle>`, which REMOVES the file | `clank events ack <selector>`, which APPENDS an ack; the event stays |
| woken by | `message_items` re-scanning the inbox each arm (`wait.rs:1363`) | unacked entries re-presented each arm |
| seen in the TUI as | a row under its agent, plus a read-only page | a row in the timeline, plus a page with actions |

The costs are not cosmetic:

1. **A handled message is destroyed.** "Done" deletes the file, so there is no record that it ever arrived, who sent it, or when it was dealt with. Every other wake family leaves history that `clank events list` can still show. An agent cannot be asked "what did you get told last week?".

2. **The operator looks in two places.** The timeline is the place things that happened appear, and a message — the most human thing that happens in this system — is the one that does not. It sits in a panel row instead, so a person reading the timeline does not see that an agent was sent instructions.

3. **Reading and handling are split across surfaces.** The TUI's inbox page is deliberately "a READER, not a menu: anything that would act on a message belongs to `clank inbox`" (`input.rs:988`). The event page is not so limited — it acks. So the one wake family a human actually authors is the one a human cannot finish from the TUI.

4. **Two spellings of one verb.** `clank inbox done` and `clank events ack` mean the same thing to an agent: this no longer wants me. An agent has to know which kind of thing woke it before it can say so.

## Design direction

**A message becomes an event, produced by a source like any other.** `clank send` appends to an event log rather than writing a file into someone's directory; the recipient's wake, the timeline row, the page and the ack all come for free from the machinery that already exists.

Four decisions, **taken here** rather than left to implementation. Each names the alternative and why it lost, so none is reopened by accident.

- **The source is one per repo, and a message event's member is its recipient.** `clank send` addresses exactly one agent — `--to` and `--master` are a required mutually exclusive group (`cli/mod.rs:684`) and `send::run` resolves one `AgentLabel` — so a message event has one member today. The event model already carries N, which is how a github event reaches several agents; this plan produces one and does not widen `send`. Rejected: a source per recipient, which is the inbox directory under a new name.
- **Priority goes.** `--priority` and `clank inbox reprioritise` (`cli/mod.rs:734`) are removed, and messages wake in arrival order like every other event. This is a deliberate capability loss and belongs in the release note.

  The alternative was to keep it, and it costs more than it looks. An event log is append-only, so a reprioritise cannot rewrite the record — it has to append a second one that the projection folds in, the way an ack does. That is real machinery, added to the shared event model, for a knob whose only job is reordering an inbox this plan removes. If priority turns out to be load-bearing in practice, that fold-in record is the design to reach for; it is not worth building first.
- **The body is carried on the event**, the way a command source's content already is. The github fetch path does not apply: there is no remote object to read, the text is local and complete when the message is sent. Its first line stays the subject the timeline row shows.
- **Existing inboxes migrate at first read**, not drain. Draining means keeping two wake paths alive until a condition — "the inbox is empty" — that nobody observes and nothing reports.

  Migration invents no send time, but it cannot read one off every file either. `Message.sent` is `Option<String>` and `inbox::parse` returns `None` for a hand-written or legacy file, which the inbox accepts deliberately (`a_hand_written_message_projects_without_a_stamp`). Nor can it lean on the sequence number: `priority_dir::scan` takes any `*.md`, so `500-note.md` is a valid inbox file and `inbox::seq_of` gives it nothing.

  So date each file by the best thing it actually has, and label what that was:

  - a STAMPED file, by the time it records;
  - an UNSTAMPED one, by its modification time — an observation about the FILE, not a claim about the message;
  - one with neither readable, by the migration's own time, which places it last. A file that cannot say when it arrived is not going to be given an arrival; appearing as "arrived when we found it" is the honest answer.

  ORDER comes from `priority_dir::scan`'s own `(priority, name)` sort — the order the inbox reads today, defined for every file it accepts — and migration appends in it, so files sharing a timestamp keep it.

- **Identity is the RECIPIENT and a per-recipient unique name.** A stem is unique inside one agent's inbox and nowhere else: with one repo-wide source, two recipients can each hold `500-00000001-note`, and a bare stem as the action key would alias them into one row, because `timeline::merge` groups across agents by source plus key. Namespacing by recipient is what makes two independent messages two.

  Within a recipient, a MIGRATED message is named by its stem, which the inbox already made unique there. A NEW send is named by a collision-resistant id minted when it is sent — not by anything the log issues, because `EventLog` is a handle on one (agent, source) pair and its `next_seq` counts within that log, so it cannot number sends to different agents against each other. A shared allocator would be new cross-agent state on the write path; a minted id needs none and cannot collide by construction.

- **Migration is at-least-once, and it finishes.** Append the event, make it DURABLE, then delete the file — never the other order, which is the one way a message is lost beyond recovery. A crash between the two leaves the file and the event both; the next read finds the ident already accounted for, deletes the file, and mints nothing. A crash before the append leaves the file readable by the old path.

  **The custody rule is a RETENTION rule, not a second bookkeeping store.** Two independent durable writes cannot prove exactly-once across an unbounded restart delay: whichever order they are in, a crash between them leaves one written and the other not, and any store that could disambiguate is the store that forgets. An index written after the append has a window before it is durable; an index written before it cannot tell "claimed" from "claimed and appended".

  So the log must not forget while the evidence still exists: **a migrated message's identity is retained for as long as its source file does.** The identity horizon (`LOG_IDENTITY_CAP`, the newest 1000) is what makes an unbounded delay dangerous, and the rule removes exactly that: no migration identity is dropped while any inbox file remains. When the last file is deleted there is nothing left to deduplicate against, and the identity may age out like any other.

  This asks something of the shared log's compaction, which the priority decision above refused to do — deliberately, and the difference is the point. Priority wanted a per-source field for a knob; this wants a retention floor for CORRECTNESS, and without it the plan cannot promise a message is presented once. A guarantee that rests on "the next read happens soon" is not a guarantee, because nothing schedules that read.

  Then the ordinary path is: append, make durable, delete the file — never the other order, which is the one way a message is lost beyond recovery. Migration ENDS by removing the inbox directory, and its absence is the record that nothing remains to migrate.

**The ack is the event ack, and `clank inbox done` goes.** One verb. If the command is kept it is an alias that defers to the same core, and its help says so.

**A handled message stays readable, within the log's retention.** This is the point of moving, and it is BOUNDED rather than permanent — but the bound is not a simple count, and saying it as one would be wrong in the direction that matters.

Two thresholds apply, in this order. A message log is rewritten only when it grows past `COMPACT_AT`; until then nothing is folded and every handled message is still readable, however many have arrived since. When a rewrite does happen, it keeps the newest `DISPLAY_CAP` (200) handled rows of THAT log verbatim and folds the rest to identity observations. Unhandled records are kept at any age.

So: 200 is per message log and applies only at a rewrite, not a live ceiling; handled events in other sources never displace a message; and an old message can stay readable long past 200 newer ones simply because nothing has triggered a compaction. The release note says that rather than a number that sounds exact and is not.

Whether messages deserve a longer retention than a github event is a real question and this plan does not answer it: it would mean a per-source cap in the shared log, which is a change to every source's storage. Named here so the next reader knows it was seen and deferred, not missed.

## Work

- Model a message as an event: produced into the same WAL, carrying the same selector, fanned out to the addressed agents as per-agent copies, and re-presented until acked — the shape `a-command-is-an-event-source-like-any-other` established.
- Route `clank send` through that producer. Preserve what it already accepts — `--to <label>`, `--master`, `--repo`, the body — minus `--priority`, per the decision above.
- Give the message wake the event discharge contract. `WaitKind::Message`'s current line — "fires until the message is handled — `clank inbox done <handle>`" (`stop_hook.rs:2308`) — becomes the events one, naming the selector.
- Put the message in the timeline, with the same acknowledgement mark every other event gets, and open it on the event page with its body and its ack action.
- Retire the panel's inbox row and the read-only inbox page, or say why either earns its place beside the timeline row. Two surfaces for one thing is what this plan is removing.

  *Taken: both retired.* Neither earned its place, and the deciding fact was not taste — both read the FILE inbox, which migration empties, so after this plan they can never display anything again. Keeping the row would have meant repointing it at the message log, which is building the second surface rather than justifying one.
- Retire `clank inbox` command by command, all four: `show` becomes `clank events show`, `done` becomes `clank events ack`, `list` becomes the timeline, and `reprioritise` goes with priority. Keep an alias only where a live skill or doc depends on it, and update those instead where it does not. `clank send` keeps its name and everything it accepts except `--priority`.
- Sweep for the belief, not the wording: every skill, README, error string and test that tells an agent to run `clank inbox done` has to change with it, and the reviewer skills say it too.

## Acceptance

- `clank send --to <label>` makes a timeline row for that agent, carrying the sender, the subject and the send time, and wakes that agent until it is acked.
- A message is acked with the same command and the same TUI action as any other event, and acking the last unhandled copy clears it from the wake the same way.
- A handled message is still readable afterwards — who sent it, when, and that it was handled — where today it is deleted, for as long as the log's retention keeps it verbatim. The bound is asserted, not assumed: a test shows a handled message readable, and the release note states the cap rather than implying permanence.
- A message reaches its ONE recipient, who is woken until they ack it. Multi-recipient fanout is capability the event model already has and this plan does not expose: `send` still takes one `--to` or `--master`, and an acceptance asserting several copies would be testing a `send` that does not exist.
- `--priority` and `clank inbox reprioritise` are gone, messages wake in arrival order, and the release note says so. Nothing accepts a priority argument and silently ignores it.
- Nothing in the repo tells an agent to run a command that no longer exists: skills, docs and error strings name the surviving verb.
- Messages already sitting in inboxes when this lands are readable in the timeline afterwards, including hand-written ones with no metadata at all — the shape `a_hand_written_message_projects_without_a_stamp` pins today.
- An unstamped message migrates without a fabricated send time, and a file with no sequence number at all — `500-note.md`, which the inbox accepts — migrates too, ordered where `priority_dir::scan` already puts it.
- Two recipients each holding a file of the same stem produce two distinct messages, not one merged row; and two sends made independently to two recipients never merge either.
- Migration run twice over the same inbox produces one event per file, not two. Interrupting it between the append and the delete, and running it again, also produces one — asserted by driving that interruption, not by reasoning about it.
- Interruption is driven at BOTH seams, not one. A crash after the durable append and before the file is deleted, and a crash before the append lands at all, each resumed after enough unrelated events to push the identity horizon past the migrated message: both produce exactly one message. The second seam is the one an index written after the append cannot cover, which is why the guarantee is a retention rule and why the test has to reach past compaction rather than retry immediately.
- A migration interrupted before its append leaves the file where it was, still readable.
- A completed migration leaves no inbox directory, and a run over a repo with none does nothing rather than failing.
- While any inbox file remains, no migrated message's identity has been dropped from the log, however many unrelated events have arrived since — asserted by arriving them.

## Release note

Three things a person will notice, and they belong in the finish message
verbatim rather than being left for someone to discover:

- **`clank inbox` is gone.** `list` is the timeline, `show` is
  `clank events show <selector>`, `done` is `clank events ack <selector>`. The
  wake names the selector, so nothing has to be looked up.
- **`clank send --priority` and `clank inbox reprioritise` are gone**, and
  messages wake in arrival order. This is a capability removed, not one
  postponed: an append-only log cannot reorder a record in place, and the
  fold-in record that would allow it is machinery the shared event model does
  not otherwise need.
- **Handling a message no longer destroys it, and the reading is BOUNDED.** A
  handled message stays readable until its own message log is compacted, which
  happens once that log grows past `COMPACT_AT`; a compaction then keeps the
  newest `DISPLAY_CAP` (200) handled rows of that log and folds older ones to
  identity observations. So an old message often outlives 200 newer ones —
  nothing displaces it until a rewrite happens, and other sources' events
  never do. "Readable afterwards" is a real change from deletion; "readable
  forever" is not a promise the log makes, and neither is an exact count.

## Scope

The message family only. No change to github or command sources, to the ack core, to the WAL format beyond what carrying a message requires, or to what wakes an agent besides messages.

Out of scope: widening `clank send` to several recipients, which the event model would support and this plan does not add; the event page's layout beyond showing a message body; and the CLI/TUI mark inconsistency in `log.rs`, which is its own plan.

One consequence to state plainly rather than discover: today `clank inbox done` DESTROYS a message, and after this it does not. A handled message becomes durable history that can be read back. That is the point of the move, and it also means a message cannot be un-sent by handling it — anything that treated deletion as privacy would change meaning. Nothing in this repo appears to, but it is a property change, not an implementation detail.
