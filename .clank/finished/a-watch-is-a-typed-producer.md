# a-watch-is-a-typed-producer

Invariants: I6.

## Why

`events-follow-the-master` keeps watch identity in strings: `watch:<id>` directories and keys, a fake `agents/inbox`, and an `"unassigned"` label. These are re-parsed at about ten places per serve.

- An agent labelled `watch:1` cannot open its own logs.
- Duplicate validation goes by target, not by recipient, so a master-targeted watch can duplicate the master's own source, giving two producers for one stream.
- The "transferred legacy source" rule is reimplemented in eight places.
- `retarget` and `transfer` accept off-roster targets.
- Watch tombstones are never collected (this repo has 20).

## Build

1. **A typed producer, resolved with its storage.** It has two forms:
   - `Producer::Agent(AgentLabel)`;
   - `Producer::Watch { id, storage }`, where `storage` is `WatchStorage::Repo` for `.clank/watch-events/<id>`, or `WatchStorage::Agent(label)` for a transferred watch whose log stays under that agent's `events/` (today's `Watch::legacy_agent`).

   It is resolved once from the repo config where the config is loaded: by the monitor's hosting and by the CLI's own read. It is carried with the item, its source key and its presentation, so `EventLog::open` takes a resolved directory and reads no configuration. Retargeting a watch or a master handoff changes only its recipient, never `storage`, so a log is never relocated or duplicated.
2. **One explicit text boundary.** User-facing selectors keep their current grammar: `watch:<id>:<source-key>@<seq>` (today's `watch_store::selector_key`, `log_location` and `events.rs:298`), timeline keys and existing durable presentation records that name one. The boundary keeps today's check that the selector's source key matches the recorded watch's key, before any log is opened, and rejects a mismatch.
   - One parse and format pair in `watch_store` converts between that text and `Producer`. It is used only by the CLI selector parser, display and reading existing records, and every other production site uses `Producer`. A source gate checks that no other site builds or matches the `watch:` prefix.
   - An agent label that happens to read `watch:1` is a `Producer::Agent`. It never reaches the selector parser as a watch, because store paths and keys come from `Producer`, not from names.
   - Selectors, WAL records, presentation records and the roster's watch records keep their forms.
   - Where a producer is an identity across a process boundary, it travels tagged, not as display text: `Control::Restart`'s author and each entry of the published sources account (`sources.json`) are `{"agent":"<label>"}` or `{"watch":{"id":<id>,"storage":...}}`. An agent labelled `watch:1` and watch 1 display alike, so display text cannot name either. This changes the monitor wire, so the protocol is bumped to 25 (I8). An older monitor's `sources.json` reads as unreadable, the state that already means "another build wrote it", until that monitor is replaced; nothing parses its display names.
3. **One producer per stream, across transitions.** A stream is identified by its recipient and source key.
   - **Adding** a watch, or **retargeting** one (`clank events watch retarget`), is refused if the result would give a recipient two producers of one source. The error names the other producer.
   - A **master change** (`agent make-master`) is never refused because of watches. Collisions it creates are resolved deterministically, without losing or misattributing anything:
     - **Repo watch against the agent's own declaration:** the repo watch hosts, and the agent's `wait_events` declaration is suppressed. That is the same rule that suppresses a transferred legacy source.
     - **Repo watch against repo watch:** for example, watch A targets Master and watch B targets beta, and beta becomes master. The watch with the lower ID hosts. The other's producer is suppressed, so it ingests nothing new. Its own log, cursor and unacked events are untouched, and they stay owed and presented under its own key. Nothing is merged, moved or dropped.
     - **Reporting:** `clank status` reports each suppression and which producer won.
     - **Lifting:** when the transition that caused a suppression is undone (the master changes again), the suppressed producer resumes from its own cursor, with the normal catch-up semantics.
   - The off-roster `retarget`/`transfer` targets are refused.
4. **The legacy-source transfer stays, as one method.** It is not a hidden migration. `clank events watch transfer` is an explicit user command that moves a still-supported per-agent `wait_events` source to a repo watch, and `WatchStorage::Agent` keeps its log in place. Live watches depend on it; this repo's `@watch:1` is one.
   - The rule becomes one method on the typed model, `Watches::hosts_legacy(agent, source)`, which every one of the eight call sites uses.
   - A legacy source is hosted from the agent's config only while no watch, removed or not, has transferred it, and no master-targeted watch suppresses it (step 3).
5. **Tombstone collection is deferred.** Tombstones carry three facts:
   - ID non-reuse: `insert` allocates the maximum existing ID plus one, so dropping the highest tombstone would let its ID be reused;
   - transfer suppression: a removed transferred watch still suppresses its agent's declaration;
   - the historical storage address of its log.

   Collecting them safely needs a home for each fact, and none of that is needed for typed identity. So none is collected here, and the Why's tombstone count is noted as expected, not a bug.
6. Delete the dead `#[cfg(test)]` code in `events/watch.rs`, and the tests that only exercise test-only code.
7. **Registry:** I6's `Today` drops watch identity, and this plan leaves I6's `Plans`.

## Acceptance

- **Storage:** a transferred watch's log is opened from its agent directory with no config read. Retargeting it, or handing the master over, leaves the log where it was, with the same file.
- **Selectors:** an existing full selector `watch:<id>:<source-key>@<seq>` still acknowledges, and one whose source key does not match the watch is rejected without opening a log. An agent labelled `watch:1` reads and acks its own logs and is never `Watch(1)`, and `Watch(1)`'s log is untouched by that agent.
- **Gate:** the `watch:` prefix appears only in the one parse and format pair. Mutation: a second site fails the gate.
- **Uniqueness:**
  - adding a master watch that duplicates the master's own source is refused;
  - retargeting a watch into a duplicate is refused;
  - make-master onto an agent whose own source a master watch duplicates succeeds, hosts one producer and reports the suppression;
  - make-master onto beta, where master watch A and beta-targeted watch B share a source, succeeds. The lower ID hosts, the other ingests nothing new, and its existing unacked events still present under its own key and ack against its own log;
  - handing the master on lifts both suppressions, and the suppressed producer resumes from its own cursor.
- **Transfer:** `Watches::hosts_legacy` is the only reader of the rule. Removing a transferred watch does not resurrect its legacy source. The existing `transfer_preserves_storage_and_never_resurrects_a_removed_watch` passes.
- Retargeting to an off-roster label is refused.
