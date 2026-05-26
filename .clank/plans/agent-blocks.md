# agent-blocks

## Summary

Agents can block a plan or the whole repo to request human
input. The user responds via `clank unblock`, and the agent
receives the answer through wfw.

## Layout

```
.clank/agents/<agent>/blocks/<plan>.md  — plan block
.clank/agents/<agent>/blocks/REPO.md    — repo block
.clank/agents/<agent>/answers/<plan>.md — user's answer
.clank/agents/<agent>/answers/REPO.md   — user's answer
```

All gitignored (under agents/ which is already gitignored).
One block file per agent per plan. The file body is the
agent's question/reason.

## Creating a block

```
clank block <plan> -m "reason"
clank block --repo -m "reason"
```

Writes the block file. The agent's label is resolved via
the standard identity resolver.

## Answering (unblock)

```
clank unblock <agent> <plan> -m "answer"
clank unblock <agent> --repo -m "answer"
```

Writes the answer file. Does NOT delete the block — wfw
handles cleanup.

## wfw behavior

On each cycle, for each agent:

1. If `answers/<plan>.md` exists → emit
   `WaitItem::HumanAnswer { plan, answer }`. Delete both
   the answer and block files. The agent gets the answer
   exactly once.

2. If `blocks/<plan>.md` exists (no answer) → that plan
   shows "waiting on human" instead of normal work. Other
   plans proceed normally.

3. If `blocks/REPO.md` exists (no answer) → all work
   suppressed. Only the block item is returned.

4. If `answers/REPO.md` exists → emit HumanAnswer, delete
   both files, resume all work.

## Status

```
plan: foo
  BLOCKED: waiting on human (claude)
  reason: is this the right API shape?
```

## Stop-hook rendering

```
- blocked: plan `foo` waiting on human — claude asked:
  "is this the right API shape?"
  User: `clank unblock claude foo -m "answer"`
```

When the answer arrives:
```
- answer: plan `foo` — human said:
  "yes but use trait objects"
```

## Hooks

`hooks.human_block` fires when a block is created so the
user gets notified (e.g. `say`).

## Skill updates

Add to both skills: when reviews become contentious, the
plan drifts, or the work feels unwise, use `clank block`.

## Tests

- Block plan → wfw returns HumanBlock, other plans proceed.
- Block repo → all work suppressed.
- Unblock with answer → wfw returns HumanAnswer once, then
  block and answer files are deleted.
- Status shows block reason.
- Stop-hook renders block and answer items.
