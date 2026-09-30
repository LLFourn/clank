# a-block-pauses-the-review-cycle-not-the-world
# clank — a block should pause the review cycle, not everything

## Why

A `clank block` is a question for a human. While one is unanswered, the agent should stop
doing REVIEW-CYCLE work — there is no point reviewing or promoting while the thing the
question is about is undecided. That is the whole intent.

What it does instead is stop nearly everything. `check_blocks` sets `suppress_all` for any
unanswered block repo-wide, and its own doc states the position plainly: *"Blocks are
repo-wide, so there is no partial suppression and nothing downstream routes work around
one."* Downstream, `suppress_all` nulls the entire `items_for` projection.

So a blocked agent stops being woken by things that have nothing to do with the block.
Most legibly **`clank send` messages**: `message_items` is reached only through `items_for`,
and the code says as much: *"Reached only through `items_for`, which no pass calls while a
block pends."* A human asking a blocked agent a question, or another agent sending it one,
gets silence until the block is answered — including, absurdly, a message that says what
the answer is.

## Both routes, and only one of them was broken

`wait.rs` reads, in order: `check_blocks`, then `if !br.suppress_all { items_for(…) }` —
with the lifecycle events extended onto the result INSIDE that guard. So a block skips
`items_for` wholesale, and everything it produces goes with it.

The CHANNEL route is untouched by that. A parked wait's `sources.rx` drains below the
guard and merges whatever `suppress_all` says, so github and command events always did
reach a blocked agent. That half was never broken and this plan does not change it.

What the guard takes is the rest: `message_items` is reached only through `items_for`, and
so are the lifecycle notices. That is the bug — a human asking a blocked agent a question,
or another agent sending it one, gets silence until the block is answered, including a
message that says what the answer is.

So there are two classes to place, not three, and the fix has to hold at BOTH routes: the
initial pass (where a `--peek` turns an empty result into "nothing to deliver") and the
watcher loop (where the channel merges in). Each used to decide what a block does for
itself, and two copies of that decision is how they could disagree about a class of item.

## The line to draw

The work is to classify what `items_for` produces, not to build a new path:

- **Review-cycle work** — `status.work_for`'s reviewer, ad-hoc and PR-review items, and
  `queue_promote_outcome`. These SHOULD be suppressed: they are the cycle the block exists
  to pause.
- **Events** — messages, and the repo-lifecycle notices (`finished:` and friends) that are
  events like any other. These should keep waking. They are not the review cycle and the
  block says nothing about them. The channel's github and command rows join them at the
  merge, which is where the two routes become one list.

  `drop_items_not_for` stays exactly where it is, on the channel drain. Surviving a block
  must not become a way around the eligibility question — *"a durable row does not become
  this agent's just by being on disk"* — because whose row it is and whether a block holds
  it are different questions.
- **The `Blocked` item itself** — today it is built and then discarded, because
  `repo_items` becomes `None`. Whether the agent should still be told what it is blocked on
  is a question this plan should settle rather than inherit.

## Questions

1. **Which items are review-cycle and which are events?** `items_for`'s outputs must be
   classified one by one, and the classification named in the type rather than inferred at
   the call site — otherwise the next item added inherits whichever behaviour the code
   happens to give it.
2. **Does an agent blocked on its OWN question still get messages?** It should — that is
   the case where a message most likely carries the answer.
3. **What about `PromoteFromQueue`?** It is repo work but not review work. Suppressing it
   is probably right (starting a new plan while a question pends is the thing a block is
   for), but it should be decided rather than defaulted.
4. **Does anything downstream assume "a block means no items at all"?** The comment on
   `queue_promote_outcome` says there is "no Blocked context to co-surface here and no
   suppression to apply" precisely because no pass reaches it under a block. Relaxing the
   suppression makes that reachable, and that assumption must be found and fixed rather
   than discovered in production.
5. **Where does the classification live so a PEEK and a PARK cannot diverge again?** The
   bug above exists because the two paths reached the same rows differently — a channel for
   one, the projection for the other — and only one of them was under the block. Whatever
   `items_for` grows, the peek and the park must agree, and the plan should say which
   single place enforces that rather than leaving two call sites to be kept in step.

## Answered

1. **Which items are review-cycle and which are events?** By PROVENANCE, not by a predicate
   over `WaitItem`: `items_for` returns `Owed { review, events }`. `work_for` and
   `queue_promote_outcome` are the review cycle by definition; the message log, the
   lifecycle inbox and the channel's sources are events. A new producer must name a field to
   compile, and an item added to an existing producer inherits that producer's side — which
   is the answer, not an accident. A predicate over the enum would have needed 21 arms, six
   of them unreachable (`ForCommit`/`ForFinished`/`ForBlocked` never reach the decision,
   since observer mode returns before `check_blocks`), and an unreachable arm classified by
   guesswork is not a classification.

2. **Does an agent blocked on its OWN question still get messages?** Yes, and nothing
   distinguishes the case: `check_blocks` sets `suppress_all` for any pending block whoever
   raised it, and the class split is what decides delivery. The one asymmetry is the
   ANSWER: `Unblocked` outranks everything and arrives alone, because it is what ends the
   block and nothing should bury it.

3. **What about `PromoteFromQueue`?** Suppressed — starting a new plan while a question
   pends is precisely what a block is for. It is review-cycle work, and now that a block no
   longer skips `items_for` the function does run and its result is dropped by
   `present_with_blocks`. One suppression, in one place, rather than a second decision at
   the producer.

4. **Does anything downstream assume "a block means no items at all"?** Two comments
   asserted it and are corrected: `check_blocks`'s ("no partial suppression and nothing
   downstream routes work around one") and `queue_promote_outcome`'s ("no pass calls [it]
   while a block pends … no suppression to apply"). The one BEHAVIOURAL site, the TUI's
   promote lamp (`render.rs`), assumes only that a pending block stops PROMOTION — which
   is still true, so it is correct unchanged. Nothing in `items_for` has a side effect
   (`scan_queue_no_dups`, `inbox::pending` and `pending_external_items` are all reads), so
   running it under a block costs a projection read and changes nothing on disk. The stop
   hook already renders a `Blocked` item with the right discharge — *"you CANNOT clear this
   — only the human can answer it"* — so the context needed no new wording.

5. **Where does the classification live so a PEEK and a PARK cannot diverge?**
   `present_with_blocks`, called by both. The initial pass and the watcher loop each used to
   read `suppress_all` and each chose to skip the projection; that duplication is what let
   the two paths disagree about a class. There is one decision now, and both callers take
   its result.

   It took two goes. The first put the decision BEFORE the parked wait's channel drain, so
   the channel route was still outside it — a blocked park returned a source item raw, and
   an answered one returned the answer with company (codex on 344280f). A beat is now
   MERGED first (`beat_owed`: projection + lifecycle + channel, deduped by selector) and
   decided once, so there is no route left that reaches the agent without passing the
   decision. The lesson generalises: the bug was never about which items a block hides, it
   was about there being more than one path to an agent.

**The `Blocked` item, settled:** it is CONTEXT and never a wake of its own. It leads the
list when events accompany it, so an agent woken by a message knows why its plan work went
quiet — and a block with no event debt delivers nothing at all, because a block pends until
a human answers and a bare `Blocked` would re-fire at every turn end until then.

## Acceptance

- A `clank send` to an agent with an unanswered block WAKES it, with a test that fails
  against today's behaviour.
- **A blocked `--peek` returns the event classes and not the review-cycle ones**, with a
  regression case at the peek specifically — not only at a parked wait. A peek returns
  whatever the initial pass produced and then stops, so an `items_for`-level test cannot
  see the difference between "suppressed" and "reported as nothing".
- **A pending block does not disturb the channel route**, which was already correct: an
  external event still reaches a blocked agent, and is still dropped when
  `drop_items_not_for` says it is not this agent's.
- **A parked wait's CHANNEL item meets the same decision**, tested against the loop and not
  only the helper: a helper test passes against a loop that calls it and then appends
  around it.
- Review-cycle wakes are still suppressed while a block pends, with the existing tests for
  that unchanged rather than rewritten to suit.
- The classification lives in one place, so a new wait item must choose a side to compile
  rather than inheriting one — and the peek and the park read that one place, so they
  cannot disagree about a class again.
- Question 4 answered: every site that assumes total suppression identified, not just the
  ones the tests happen to cover.

## Out of scope

The block mechanism itself — how blocks are raised, answered or scanned. The eligibility
filter on external rows, which is correct and stays exactly as it is. Anything about which
agent a block belongs to: blocks stay repo-wide, only their BLAST RADIUS narrows.
