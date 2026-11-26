# an-idle-master-raises-no-block
# The master can't raise a block while the repo is idle

## Why

A block exists to pause the review cycle: while one is pending, clank holds promote, review and finish
work until the person answers. When the repo is idle, with no active plan and nothing queued to
promote, there is no cycle to pause. A block raised then pauses nothing, yet it still shows red in
status, fires the `blocked` hook, and sits in the repo until someone answers it. An idle agent that
wants something from the person should say so in its own turn, or reply to whoever messaged it.

LLFourn: "when an agent tries to make a block when there's no active plan or promotion i.e. idle
state can you reject it."

## Build

- In `block.rs`, `admit` (or `raise`, after `admit`) refuses an agent-raised block when the repo is
  idle. Idle means no active plan and an empty queue. A non-empty queue means a promotion is owed, so
  it is not idle.
- The refusal says why and what to do instead. For example: "refusing to raise a block: the repo is
  idle (no active plan, nothing queued), so there is no review cycle to pause. Ask the person in
  your own turn, or reply with `clank send`."
- This covers every block an agent raises, `--permission` blocks included. The person answers a
  permission block by typing into the agent's terminal anyway, so asking in the turn reaches them
  the same way.
- The person is not refused: `Identity::Absent` raises blocks at any time, and so does the TUI's
  pause.
- Read "idle" from the same plan/queue state that routing and `promotable` use. Do not re-derive
  it, so the block check can't disagree with what the wait hands the master. If no shared
  predicate exists yet, add one and use it in both places.
- The master skill text (`setup_assets`) and the README section on blocks gain one line: no blocks
  while idle.

## Tests

- In-process, idle repo (no plan, empty queue): the master's `block ask` is refused with the idle
  message, and no block files are written.
- An active plan: the master's ask is admitted.
- A queued plan and no active plan (a promotion is owed): admitted.
- The person (no agent identity) in an idle repo: admitted.
- Mutation-check the idle guard. Delete it, and the first test must fail.

## Done when

It's merged on `master`.
