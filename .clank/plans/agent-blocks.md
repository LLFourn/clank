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

## Semantics

A block is active when `blocks/[plan/]<name>.md` exists
and `unblocks/[plan/]<name>.md` does not.

- Plan block: only that plan's work is suppressed. Other
  plans and queue promotion proceed normally.
- Repo block: all work suppressed.

## Commands

```
clank block <name> -m "question"
clank block <name> --plan <plan> -m "question"
clank unblock <agent> <name> -m "answer"
clank unblock <agent> <name> --plan <plan> -m "answer"
clank clean
```

`block` writes the block file for the current agent.
`unblock` writes the matching unblock file (user runs this).
`clean` removes matched block/unblock pairs, finished
plans, and other stale artifacts.

## wfw behavior

1. Scan all agents' `blocks/` and `unblocks/`.
2. Unmatched repo block → suppress all work, emit
   `WaitItem::HumanBlock`.
3. Unmatched plan block → suppress that plan's work, emit
   `WaitItem::HumanBlock`. Other plans proceed.
4. Block with matching unblock the agent hasn't consumed →
   emit `WaitItem::HumanAnswer`.

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

Removes:
- Matched block/unblock pairs (both files deleted)
- Finished plan files in `.clank/finished/`
- Other stale artifacts

## Tests

- Plan block without unblock → wfw suppresses that plan.
- Repo block without unblock → wfw suppresses all work.
- Matching unblock → wfw returns HumanAnswer.
- Other plans proceed while one is plan-blocked.
- clank clean removes matched pairs.
- Status shows active blocks.
