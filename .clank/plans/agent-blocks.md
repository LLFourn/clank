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

1. **Pending**: `blocks/` file exists, no matching `unblocks/`.
   Work is suppressed.
2. **Answered**: matching `unblocks/` file exists. wfw emits
   the answer. The agent deletes its own block file after
   reading. Once the block file is gone, wfw stops emitting.
3. **Cleaned**: `clank clean` removes orphaned unblock files
   (unblock with no matching block).

That's it. No consumed/ack state — the agent deletes its
own block file as acknowledgment.

Decline is just an answer whose content says "continue with
best effort." There's no separate state — the agent reads
the answer body and acts accordingly. Agents should not
re-create a block with the same question after being told
to continue.

- Plan block: only that plan's work is suppressed. Other
  plans and queue promotion proceed normally.
- Repo block: all work suppressed.

## Commands

```
clank block <name> -m "question"
clank block <name> --plan <plan> -m "question"
clank unblock <agent> <name> -m "answer"
clank unblock <agent> <name> --plan <plan> -m "answer"
clank unblock <agent> <name> --decline -m "continue with best effort"
clank clean
```

`block` writes the block file for the current agent.
`unblock` writes the matching unblock file (user runs this).
`--decline` prefixes the unblock body with `DECLINE ` so
wfw can surface it distinctly and agents know not to re-ask
the same question.
`clean` removes orphaned unblock files (no matching block).

## wfw behavior

1. Scan ALL agents' `blocks/` and `unblocks/`.
2. Pending repo block from any agent → suppress all work,
   emit `WaitItem::HumanBlock`.
3. Pending plan block from any agent → suppress that plan's
   work, emit `WaitItem::HumanBlock`. Other plans proceed.
4. Answered block where the calling agent is the blocker →
   emit `WaitItem::HumanAnswer { name, answer, declined }`.
   The agent deletes its block file to acknowledge.

## Status

Shows active blocks prominently:

```
plan: foo
  BLOCKED: waiting on human (claude)
  reason: is this the right API shape?
```

## Stop-hook rendering

```
- blocked: `api-shape-question` (plan: foo) — claude asked:
  "is this the right API shape?"
  User: `clank unblock claude api-shape-question --plan foo -m "answer"`
```

## Hooks

`hooks.human_block` fires on block creation.

## Skill updates

Add to both skills: when reviews become contentious, the
plan drifts, or the work feels unwise, use `clank block`.

## `clank clean`

Removes orphaned unblock files (unblock with no matching
block — the agent already deleted its block file).

## Tests

- Plan block without unblock → wfw suppresses that plan.
- Repo block without unblock → wfw suppresses all work.
- Matching unblock → wfw returns HumanAnswer.
- Agent deletes block file → wfw stops emitting answer.
- Other plans proceed while one is plan-blocked.
- clank clean removes orphaned unblocks.
- Status shows pending and answered blocks.
