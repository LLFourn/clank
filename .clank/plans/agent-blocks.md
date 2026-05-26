# agent-blocks

## Summary

Agents can block a plan or the whole repo to request human
input. The user responds via `clank unblock`, and the agent
receives the answer through wfw.

## Layout

```
.clank/agents/<agent>/blocks/<plan>.md  — plan block
.clank/agents/<agent>/blocks/REPO.md    — repo block
```

All gitignored (under agents/ which is already gitignored).
One block file per agent per plan. The file body is the
agent's question/reason. When answered, the user overwrites
the file with the answer (prefixed ANSWER or DECLINE).

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
clank unblock <agent> <plan> --decline -m "just continue"
clank unblock <agent> --repo -m "answer"
```

`unblock` replaces the block file content with the answer,
prefixed with `ANSWER` or `DECLINE`:

```
ANSWER yes but use trait objects
```
or
```
DECLINE just continue with best effort
```

The block file stays on disk as a record. The agent is
responsible for deleting it after reading (via
`clank block --clear <plan>`).

## wfw behavior

On each cycle, for each agent's blocks:

1. Read the block file. If it starts with `ANSWER` or
   `DECLINE` → emit `WaitItem::HumanAnswer { plan, answer,
   declined: bool }`. The agent reads the answer and
   should delete the block file.

2. If the block file does NOT start with `ANSWER`/`DECLINE`
   → the block is still pending. That plan shows "waiting
   on human". Other plans proceed normally.

3. `REPO.md` block → all work suppressed until answered.

Agents that receive a `DECLINE` must not re-create the
same block unless something material changes.

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
  Clear with `clank block --clear foo`
```

When declined:
```
- declined: plan `foo` — human said:
  "just continue with best effort"
  Clear with `clank block --clear foo`
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
- Unblock with answer → wfw returns HumanAnswer.
- Unblock with decline → wfw returns HumanAnswer with
  declined=true.
- Agent clears block after reading answer.
- Status shows block reason and answered/declined state.
- Stop-hook renders block, answer, and decline items.
- Block visible in status even when hooks are disabled.
