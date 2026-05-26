# agent-blocks

## Summary

Agents can block to request human input. A block is a file,
an unblock is a matching file. Blocked = block exists without
a matching unblock.

## Layout

```
.clank/agents/<agent>/blocks/<name>.md          — repo block
.clank/agents/<agent>/blocks/<plan>/<name>.md   — plan block
.clank/agents/<agent>/unblocks/<name>.md        — repo unblock
.clank/agents/<agent>/unblocks/<plan>/<name>.md — plan unblock
```

Gitignored (under agents/). The name is descriptive
(e.g. `api-shape-question`). The block file body is the
question. The unblock file body is the answer.

Root-level blocks are repo-wide. Blocks inside a plan
subdirectory only block that plan.

## Lifecycle

Three states per block:

Blocked = block file exists, no matching unblock.
Answered = block file + matching unblock exist.

The agent owns its block file — it can edit it, elaborate,
or delete the unblock to re-enter pending state. `clank clean`
removes matched pairs when both exist.

The unblock file body is the user's response. The agent
reads it and acts on it.

- Plan block: only that plan's work is suppressed. Other
  plans and queue promotion proceed normally.
- Repo block: all work suppressed.

## Commands

```
clank block <name> -m "question"
clank block <name> --plan <plan> -m "question"
clank block clean
clank unblock <agent> <name> -m "answer"
clank unblock <agent> <name> --plan <plan> -m "answer"
```

`block` writes the block file for the current agent.
`block clean` removes the calling agent's matched
block+unblock pairs (the ack step).
`unblock` writes the matching unblock file (user runs this).

## wfw behavior

1. Scan ALL agents' `blocks/` and `unblocks/`.
2. Pending repo block from any agent → suppress all work,
   emit `WaitItem::HumanBlock`.
3. Pending plan block from any agent → suppress that plan's
   work, emit `WaitItem::HumanBlock`. Other plans proceed.
4. Answered block where the calling agent is the blocker →
   emit `WaitItem::HumanAnswer { name, answer }`. The
   agent deletes its block file to acknowledge.

## Status

Pending:
```
plan: foo
  BLOCKED (claude): is this the right API shape?
```

Unblocked:
```
plan: foo
  UNBLOCKED (claude asked: is this the right API shape?):
    yes but use trait objects
```

## Stop-hook rendering

Pending:
```
- blocked: `api-shape-question` (plan: foo) — claude asked:
  "is this the right API shape?"
  User: `clank unblock claude api-shape-question --plan foo -m "answer"`
```

Unblocked:
```
- unblocked: `api-shape-question` (plan: foo) — human said:
  "yes but use trait objects"
```

## Hooks

`hooks.human_block` fires on block creation.

## Skill updates

Add to both skills: when reviews become contentious, the
plan drifts, or the work feels unwise, use `clank block`.


## Tests

- Plan block without unblock → wfw suppresses that plan.
- Repo block without unblock → wfw suppresses all work.
- Matching unblock → wfw returns HumanAnswer.
- Agent deletes block file → wfw stops emitting.
- Other plans proceed while one is plan-blocked.
- clank clean removes orphaned unblocks.
- Status shows pending and unblocked states.
