# pending-work-nobody-is-waiting-for-is-noticed
# Pending work nobody is waiting for is noticed

## Why

Some agents leave no readable failure in their record: a hung harness, or a
record clank cannot find or parse. This is the fallback for those, §6 of the
finished research plan `research-agent-stalls-and-provider-capacity-recovery`.
It builds on the eligibility fold, record watches and scheduler of
`a-failed-turn-is-reported-and-retried`.

## What to build

- `recovery.stall_after` in repo config: default 40 min, `off` to disable.
- The deadline: an eligible agent with no failed turn in its record gets one
  deadline, cancelled by everything that ends eligibility.
- At expiry it is **notice-only for every harness**: one timeline row and a
  status-line state, "<agent> has had pending work for 40 min and is not waiting
  for it". It never starts or interrupts a turn.
- It posts nothing when the record shows the latest turn cancelled or
  interrupted. A person stopped it.
- It posts nothing and re-arms when the record grew since the deadline was
  armed.
- After a notice, nothing further until eligibility resets.

## Acceptance

- A readable recorded failure is left to the failed-turn plan.
- A slow tool (the record growing) is re-armed and gets no notice.
- An interrupted latest turn gets no notice.
- A parked deliverer or proven attendance means no deadline at all.
- Progress just before the deadline cancels it.
- `off` disables the fallback, while recorded failures still recover.
- A restart re-derives the deadline without a duplicate notice.
- Cost: one timer per eligible agent; no work while it waits.

## As built

- **`recovery.stall_after`** takes clank's duration grammar (`45s`, `40m`,
  `1h`) or `off`; `0` is also `off`. A value it cannot read is ignored with
  a warning, and the default stands.
- **One deadline per eligible agent,** kept in
  `.clank/agents/<label>/stall.json`. Its state is one of:
  - `waiting`, holding the deadline and the record's length when armed;
  - `quiet`, after a stopped turn or a recorded failure, until the record
    grows;
  - `told`, latched until eligibility ends.

  The deadline rides the recovery driver's timer. A `quiet` stall wakes on
  the record's watch, and only when the record has grown. Nothing runs
  while it waits.
- **Progress is the record's mark,** not its parsed turns: its path,
  inode, length and modification time. A slow tool's output counts, and so
  does a same-size replacement or a session's record moving to another
  path. A record armed again after a restart is progress only if its mark
  changed.
- **Eligibility is folded, not sampled at the deadline.** The fold's
  facts carry whether a question or block is pending, or attendance is
  proven. The attendance and block files live under `.clank/`, so they
  fold already, with no polling.
  - Losing eligibility ends the stall, including a `told` latch.
  - Regaining it starts a fresh interval.
  - At the deadline the monitor still checks the agent as it is then.
- **A storage failure holds this stall** for `RETRY_LOOK` on every path,
  folds included. The notice is emitted only once `told` is on disk.
  - A held decision is owed whether or not any stall was ever stored.
  - When the hold ends, the decision is rebuilt from the agent's
    eligibility and policy at that moment. A first arming, a clear, or an
    `off` set during the hold all come out right.
  - Every pass at a passed hold either clears it or moves it on.
  - What is observed during a hold, such as a re-arm or an eligibility
    reset, updates the stall as observed, separately from the stall as
    stored. The hold delays only the write.
  - A told stall is latched only once it is stored for that interval.
    One not yet stored is a candidate, judged by a single rule
    (`stall_verdict`) at the point it is stored, whichever path stores
    it: a fold, a record event or a deadline.
    - A moved record re-arms it.
    - A lengthened interval keeps it waiting until its new deadline.
    - A stopped or failed latest turn quiets it.
    - An ineligible agent clears it.
    - At a deadline the gate still applies first.
  - The notice follows the stored transition and is keyed by the
    interval (session and `since`). It is suppressed only when that same
    interval was already told.
