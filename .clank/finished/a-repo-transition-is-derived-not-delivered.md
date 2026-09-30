# a-repo-transition-is-derived-not-delivered
# clank — retire the `repo-lifecycle` event source

## Why

`repo-lifecycle` WAS an event SOURCE, declared in `cli/wait.rs` beside the wait itself and
sitting in the same per-agent WAL as the github and command feeds, carrying two rows:
`WaitItem::Finished`
("a plan finalized after this agent's lifecycle inbox began. Replayed until the master
acknowledges the event") and `WaitItem::AdHocSettled` ("an ad-hoc gate reached a terminal
verdict. Replayed until ack"). It had a baseline watermark
(`repo-lifecycle:baseline:v1`), an `IngestLease`, a durable journal per agent and
explicit acks — `clank events ack repo-lifecycle@418`.

That is the entire apparatus of an OUTSIDE feed, applied to two transitions the repo's
own fold already computes from its own files. A github event needs a WAL because the
event happened on another machine and will never happen again. "A plan finalized" is
visible in `.clank/finished/`, in the commit, and in `RepoState`, every time anyone
looks. The repo is posting letters to itself about facts it already knows.

## How it got here

Worth recording, because no plan was ever about this. Three steps, and the design was
settled in review of a plan on another subject.

**1. The seed — `adhoc-continue-wake-investigation` (1b0d88f6, 2026-08-09).** A CONTINUE
verdict on an ad-hoc commit woke nobody: `work_for`'s ad-hoc arms route `Unreviewed` →
reviewers and `ChangesRequested` → master, and route a settled gate to no one. The plan
weighed exactly the alternative this one revisits, and rejected it in one sentence:

> A persistent item is impossible (master items have no ack surface — an always-derived
> "review passed" item would wake forever)

So it copied `detect_finished`'s trick instead: `StartupSnapshot` captured the pending
ad-hoc shas at arm, `detect_adhoc_settled` diffed them, and the item fired "once per
parked wait by construction." **Nothing durable.** The plan also recorded an option (b)
it did not take — *"the current no-item behavior is correct and the TUI/status must make
the continued state legible so agents stop expecting a wake"* — which this plan should
re-open, since three redesigns have passed over its reasoning.

**2. The collateral damage — `investigate-wait-generation-removal` (80f01612).**
Removing wait admission and claims let several waits coexist for one agent, which broke
a mechanism whose correctness rested on there being one observer. That plan's step 3
foresaw it in a single clause, and expressed no preference:

> Audit edge-triggered wait items such as finished-plan and ad-hoc-settled notices: if an
> older wait can be their only observer, make them replayable to a later wait or retire
> the obsolete notice.

**3. The reviews chose, on the third pass.** All from grok, on that plan's commits:

- `90ea5e1` REQUEST_CHANGES — *"Finished and AdHocSettled are still one-shot, and the
  generation fence that used to kill the only observer is already gone. […] A wait that
  starts after the transition never sees it."*
- `fc2c3b2` CONTINUE, not finished — *"Those notices have to be replayable, or retired,
  now that wait.gen is gone."*
- The implementation then tried RETIRING them, and `b8ece2e` REQUEST_CHANGES sent it
  back: *"Retiring these notices deletes the only wake, which is not what obsolete means.
  **AdHocSettled existed because a settled ad-hoc gate has no follow-on work item, and
  the master's loop still has to wake.** `work_for` does not emit either. […] **Log the
  settle and the finalize in the event inbox, the way a draft change is logged**, so a
  later wait can still see an unacked one."*

That last sentence is where `repo-lifecycle` comes from. It is a reviewer's remedy for
damage caused by an unrelated removal, adopted because the alternative on the table was
deletion and deletion had just been refused. It was never proposed, weighed or accepted
as a design in its own right, which is why there is no discussion of it to find.

## The claim that has to be answered, not bypassed

grok's objection is the load-bearing one and it is not obviously wrong:

> a settled ad-hoc gate has no follow-on work item, and the master's loop still has to
> wake

Both halves need testing before anything is deleted:

- **Does the master have to wake?** If a settled gate obliges the master to DO something,
  that obligation is a function of gate state and belongs in `work_for` as a derived
  item — where every other obligation lives — not in a journal. If it obliges nothing,
  the wake says "review passed, nothing owed", and a wake that asks for no action is the
  thing option (b) was for.
- **What discharges it?** This is the question that killed derived state in step 1, and
  the answer may have changed. `wakes-state-their-discharge-condition` since established
  that every wake names what ends it, and a derived item's discharge is a state change —
  the master's next commit, the plan leaving `plans/`. Name the state change, or concede
  there is none and stop waking.

Note the shape of the trap: the objection to a derived item was that it would "wake
forever" for want of an ack surface. The fix for that is a discharge condition, and the
project built a durable event log with manual acks instead. If the answer here turns out
to be "there is no state change that discharges it", that is an argument against the
WAKE, not for the journal.

`Finished` deserves its own answer rather than riding along: the master is who runs
`clank finish`, so a master-only notice that a plan finalized is telling an agent what it
just did. If that is right it is a strictly easier deletion than `AdHocSettled`; if it is
wrong, the reason should be written down this time.

## Answered

**1. Does a settled ad-hoc gate oblige the master to act? No.** Three independent checks
agree:

- **Its discharge is circular.** `wakes-state-their-discharge-condition` made every wake
  state what ends it, and these two are the only ones whose answer is not a repo state
  change (`stop_hook.rs:2299`):

      Reviewer          → "fires until you write feedback for that sha"
      AdHocReview       → "fires until feedback exists for that sha"
      FixCommitTag      → "fires until the commit's plan tag is corrected"
      PromoteFromQueue  → "fires until the item is promoted or removed from the queue"
      MultiplePlansOpen → "fires until one plan is active"
      Finished | AdHocSettled → "fires until the event is acked"

  Every other item ends when the repo changes. These end when the agent says it looked.

- **Nothing teaches an agent to act on them.** `skill_master.md` and
  `skill_shared_core.md` do not mention `adhoc-settled` or `finished:` at all. There is no
  instruction anywhere to do anything with either.

- **Nothing consumes them.** Outside the WAL plumbing, both appear only in renderers —
  `stop_hook.rs:2430,2444`, `wait.rs:1839,1854`, `events.rs:333,339`,
  `status_tui/scroll.rs:111`. `work_for` emits neither, which is exactly what grok
  observed in `b8ece2e`; the conclusion drawn there was that the wake must be preserved,
  and the conclusion available now is that there was never any work behind it.

  The rendered line is the whole feature: `adhoc-settled 9bc459f4 (gate continued) —
  ack clank events ack repo-lifecycle@418`. A sha, a gate, and an instruction to dismiss.

**2. What state change discharges it? None, because nothing is owed.** The question that
killed derived state in 2026-08 — *"an always-derived 'review passed' item would wake
forever"* — has no answer that is not the ack itself, and an ack surface is not a
discharge condition. That is an argument against the WAKE, not for the journal.

**3. Does `Finished` survive independently? No, and for a stronger reason.** The master is
who runs `clank finish`, so a master-only notice that a plan finalized tells an agent what
it just did. The one case where somebody else finalizes — a human running `clank finish`
in another pane — is already covered by derived state: the fold sees `plans/` emptied, and
if there is next work `PromoteFromQueue` fires for it. If there is no next work, there is
nothing to wake for.

That is observable rather than theoretical. Over this session the master received
`finished: <plan> @ <sha> — ack …` after each of four finalizes and every time did exactly
one thing: acked it. What actually carried the next work each time was
`promote: <name>` — a derived item.

**4. Anything assuming these exist?** See the scope below; the renderers are the whole
list, and `--for finished` is a different mechanism that stays.

## Scope of the removal

43 production references across `crates/core/src/wait.rs`, `cli/wait.rs`,
`cli/stop_hook.rs`, `cli/events.rs`, `cli/pr_review.rs`, `cli/status_tui/scroll.rs`.
What must be handled:

- The source, its baseline, its `IngestLease` and its journal; `clank events` must stop
  offering a source that no longer exists, without breaking github or command rows.
- **Live repos have unacked rows on disk** (`.clank/agents/*/events/repo-lifecycle.jsonl`
  exists in this repo for two agents). A new binary must not choke on them, and the plan
  must say whether they are drained, migrated or ignored — per
  `verify-the-invariant-not-your-machine`, fixture the OLD bytes rather than reasoning
  from a clean tree.
- `a-block-pauses-the-review-cycle-not-the-world` classified these notices as EVENTS in
  `Owed { review, events }`. Removing them removes a member of that class; the
  classification must still compile and still mean something.
- `--for finished` / `WaitItem::ForFinished` is observer mode, a different mechanism, and
  must not be caught in the sweep.
- Per `sweep-the-belief-not-the-wording`: grep the CLASS (lifecycle / settled / finalized
  notice), not just the identifiers renamed here, and report which hits were judged
  correct rather than a zero count.

## Questions

1. Does a settled ad-hoc gate oblige the master to act? If yes, what act — and it belongs
   in `work_for`.
2. What state change discharges "the master has seen it"? If none, the wake goes.
3. Does `Finished` survive independently of `AdHocSettled`, and on what argument?
4. If both wakes go, what makes the state legible instead — `clank status`, the TUI, the
   log? Option (b) from step 1 named this and never got built.
5. What happens to rows already on disk in live repos?
6. Does anything else want to become a lifecycle event later? If a future need is the
   real justification, say so and keep it; if not, the source goes rather than waiting.

## Acceptance

- `repo-lifecycle` is gone as an event source: no baseline, no lease, no journal, no
  selector, no ack path — or it is KEPT with a written argument that survives question 1,
  which is an acceptable outcome of this plan provided the argument is recorded.
- Whichever way it goes, questions 1–3 are answered in this file with reasoning, so the
  next person finds a decision instead of an artifact.
- A repo carrying unacked `repo-lifecycle` rows from an older binary works, with the old
  bytes as a fixture.
- Github and command events are untouched: same WAL, same acks, same at-least-once.
- `--for finished` still observes a finalize.
- If a wake is removed, the state it announced is legible somewhere a human or agent
  actually looks, and a test covers that surface.

## Landed

- The source is gone: `LIFECYCLE_SOURCE`, `LIFECYCLE_BASELINE`, `sync_lifecycle_events`,
  `WaitItem::Finished`, `WaitItem::AdHocSettled` and their `WaitKind`s, the JSON and human
  renderers, the stop-hook lines and the TUI arm. `beat_owed` loses its lifecycle
  parameter: the channel is now the only route into `Owed::events` besides the message log.

- **`--peek` is side-effect-free for the first time.** Its own doc has always claimed
  *"Unlike a real wait it is SIDE-EFFECT-FREE: it fires no lifecycle or idle hooks and
  promotes nothing"* (`mod.rs:554`), but `sync_lifecycle_events` ran in the initial pass —
  before the peek's return at `wait.rs:229` — so every peek by a master appended rows and
  a baseline observation to a durable WAL. Removing the source makes the documented
  contract true rather than aspirational. Nobody was looking for this; it fell out.

- **Old rows are inert, verified with this repo's own bytes.** The log format promises a
  recognized `t` whose fields no longer parse is FOREIGN, not corruption, and the fixture
  holds it to that: three legacy rows read back as foreign, zero readable, and a live
  source in the same directory still lists. That matters because the listing walks the
  whole directory — a log reported corrupt would print on every `clank events` in every
  upgraded repo, and an unacked row would have become unackable. The files stay on disk;
  foreign lines are preserved for custody by design.

- **The surfaces that replace the wake are tested — there are TWO, not one.** The first
  draft of this claimed `clank status` was the only place a finalize is legible, which is
  wrong: `select_plans_and_finished` returns `last_finished: None` whenever any plan is
  active, so `status` shows it only on an idle repo (codex on 777fb2c). That is the
  intended design — the status view answers "what needs doing" — and the retired wake also
  fired when another plan was running, so the claim had to be corrected rather than the
  code changed.

  - `status.rs:828` renders `last finished: <plan> (finalized <sha>)` on an idle repo. It
    was untested; now covered.
  - `clank log` marks every finalize with `⚑` regardless of what else is active, and names
    the plan under it. Now covered for exactly the active-plan case `status` cannot show.
  - Ad-hoc gates were already covered by `status_shows_adhoc_commits_with_one_active_plan`.

- **The apparatus the source needed is gone too.** `EventLog::all_action_keys` (its
  docstring was about lifecycle identities specifically) and `append_handled_inbox` had no
  callers left, and `Transport::Repo` had no producer — the compiler said so in two dead-
  code warnings that went unread until review (codex on 777fb2c). `stop_hook::WaitItem.gate`
  went with them. `finalized_at` STAYS on that struct despite no kind producing it: during
  an upgrade an older `clank wait --json` still emits a `finished` item, this build renders
  it through the unknown-kind path, and that fallback is what gives the line a sha.

- Two block tests used `Finished` as their example of the EVENT class. They now use a
  message, which is what that class is for and what the block plan's own regression uses.

## Out of scope

The event WAL itself, which is correct for the sources that genuinely come from outside.
`clank events` as a command. The ad-hoc review gate's behaviour — which verdicts route to
whom is settled; this plan changes only what happens AFTER one settles. The block
classification's structure.
