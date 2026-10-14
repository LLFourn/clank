# a-repeated-item-says-what-discharges-it
# A repeated delivery tells the agent what discharges each item, by its kind, and never offers a block for work that is not a plan's

## Why

An agent was seen ignoring its events while this escalation was repeated to it
(`wake_repeat::escalation`, appended by `stop_hook::escalated` once the same items
have been delivered `ESCALATE_AT` times):

> These items have been delivered to you 4 times unchanged. Ending your turn does not
> discharge pending Clank work … If it genuinely depends on the user, describe the
> dependency with `clank block create <name> -m "<question>"`. A block asks a person
> to resolve a real dependency and pauses review work; it is not a mute button. …

It is wrong in two ways:
- **It offers a block for everything.** A block belongs to a plan. For an event, the
  one way out the text names does not apply, so the agent is left with nothing it can
  do, and ends its turn again.
- **It treats "depends on someone else" as a reason to stop.** It names only the
  user. But an item can wait on anything: the user, another agent, a running process,
  an external system. None of those is a reason to leave an acknowledgeable event
  unanswered. It can be answered or acked now, saying what it waits on, and followed
  up when that arrives.

The wording is the same whatever was delivered, because `escalation(count)` sees only
the count. The items, and how each is discharged, are already classified per kind
(`stop_hook::discharge_of`, total over `WaitKind`). The escalation should use that
classification, not a second one.

## What to build

### 1. One classification of how each item is discharged

`discharge_of` stays total over `WaitKind`. Alongside its condition text it gives each
kind's class, so a new kind fails to compile until it is classified for escalation
too. The classes:
- **Acknowledgeable events:** `Message`, `GithubEvent`, `CommandEvent`, `TimerEvent`,
  `DraftChanged`.
  - **The instruction:** handle it now. Read it with `clank events read <selector>`,
    the receipt-bearing read. Reply if the source asks for one, with
    `clank events reply <selector> -m <text>`. Then discharge it with
    `clank events ack <selector>`, or with `clank events ack <selector> --reply
    <text>` where a reply is required.
  - **Its own selector and reply requirement.** The instruction is built from each
    item's selector and `requires_reply`, as the ordinary wake already renders them
    for a message, not from its handle.
  - **A required reply is still required.** Depending on someone or something else
    does not waive it. The reply says what it waits on, and the agent follows up
    when that arrives.
  - **`DraftChanged` keeps its rule:** read the draft, do not reply to the edit, then
    ack.
  - **These are never a reason to end the turn:** the agent must discharge them
    before it stops.
- **Workflow steps:** `Master` with its reason, `Reviewer`, `FixCommitTag`,
  `PromoteFromQueue`, `AdHocReview`, `AdHocRevise`, `PrReviewer` and `PrMaster`.
  - **The instruction:** the step the kind's existing condition names. For example,
    commit work addressing the feedback, write feedback for that SHA, run `clank
    finish`, promote or remove the queue item, commit the revision, or act on that
    round.
  - **A block is named** (`clank block create <name> -m "<question>"`) only for an
    item that carries a plan, and only as the way to record that the plan's next
    step depends on a person.
- **`Unblocked`:** `clank block clean`.
- **`Blocked`:** human-owned. The escalation keeps the existing boundary: the agent
  cannot clear it, must not act on it and must not answer it. Its repetition is the
  human not having answered yet. Nothing here tells the agent to do anything about it.
- **Idle, observer-only kinds, and a kind this build does not know:** no ack, no
  block, no instruction invented.

### 2. The escalation sees the delivered items

`escalated` passes the items it is extending to the escalation, not just the count.
The escalation renders one instruction line per item from the classification in (1),
using that item's own fields: selector, reply requirement, plan, SHA and name.

### 3. What stays

The escalation keeps the existing paragraph's limits:
- follow the user's latest instruction, any explicit pause, and the actual reason
  given for a rejected action;
- repeated delivery grants no new authority.

These apply to every class.

### 4. Mixed deliveries

Each item is treated by its own class:
- a plan block does not discharge an event;
- an event's dependency does not become a plan block;
- a `Blocked` item in the same delivery does not excuse the others.

## Acceptance

- **Required-reply message:** a repeated message requiring a reply gets
  `clank events read <selector>` and `clank events ack <selector> --reply <text>`
  with its real selector. The text says waiting on something does not waive the
  reply, and does not mention `clank block`.
- **Event without a required reply:** it gets the read and `clank events ack
  <selector>`, and the act-now instruction.
- **`DraftChanged`:** keeps "do not reply to the edit".
- **Workflow items:** ad hoc review and ad hoc revision get their steps, feedback for
  the SHA and a revision commit. A plan-carrying master item gets its step, and may
  name `clank block create`. A promote item does not name a block.
- **`Unblocked`:** gets `clank block clean`.
- **`Blocked`:** gets the human-only boundary, and no action or answer.
- **Idle and unknown kinds:** get no ack and no block command.
- **A mixed delivery** (message, plan item and `Blocked` together): each line is by
  its own class, and the block is offered only for the plan item.
- **The kept limits** (user's instruction, pause, rejection reason, no new authority)
  are present in every escalation.
- **The threshold is unchanged:** nothing below `ESCALATE_AT`.
- **Wired through the hook:** a continuation through `escalated` carries the item
  lines for the items the hook delivered.
- **The classification is total:** adding a `WaitKind` without classifying it fails
  to compile.

## Out of scope

- The escalation threshold, and the counting in `wake_repeat`.
- What `clank block` and `clank events` accept.

## As built

- **The classification** (`stop_hook.rs`):
  - `classify(kind, reason)` is the one total classification. Each arm returns both
    the wake's `Discharge` condition and the escalation's `Duty`, so the two cannot
    classify a kind differently. `discharge_of` projects the condition. The duties
    are `Event`, `Step`, `Unblocked`, `HumanOnly` and `Nothing`: idle, observer-only,
    and a master reason with no condition.
  - Each item is addressed by its full identity (`WaitItem::identified`): its kind,
    master step, plan, name, PR number, round, short SHA, and event selector.
  - An event's selector is its own, or else its presentation's
    (`EventPresentation::selector`, `source@seq`), so a GitHub event is addressed
    as the real wait projection carries it.
  - `duty_line` renders one item from its own fields:
    - **events:** read with `clank events read <selector>`, then
      `clank events ack <selector>`, or `--reply <text>` where a reply is required.
      The required-reply text says waiting does not waive the reply. `DraftChanged`
      says do not reply to the edit.
    - **steps:** `discharge_of`'s condition, plus `clank block create` only when the
      item carries a plan;
    - **`Unblocked`:** `clank block clean`;
    - **`Blocked`:** the human-only boundary.
  - An unknown kind, or `Nothing`, gives no line.
- **The wiring:** `ReceiptCapture::delivering` keeps the delivered items' duty lines
  beside the delivery it counts. `escalated` passes them to
  `wake_repeat::escalation(count, duties)`.
- **The escalation paragraph:** it keeps the user's-instruction, pause and
  rejection-reason limits, adds "repeated delivery grants no new authority", and
  lists each duty. It no longer names `clank block` itself.
- **Tests:**
  - each kind's line: required-reply message by selector and not its handle;
    command event; draft; ad hoc review and revise; master with a block; promote
    without one; unblocked; blocked; idle and unknown with none;
  - a mixed delivery;
  - the threshold and kept limits;
  - `escalated`;
  - the real hook path (`Continuations`) carrying a message's ack after
    `ESCALATE_AT` deliveries;
  - through the real wait projection (`cli::wait::render_json`), each item is
    addressed by its own identity:
    - a GitHub event by its presentation's selector, both read and ack;
    - ad hoc review and revise by their SHA;
    - a PR reviewer and PR master by number and round.
