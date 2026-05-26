# agent-blocks

## Summary

Agents can block a plan or the whole repo to request human
intervention. Blocks are files under the agent's `.clank/`
directory. While blocked, wfw shows "waiting on human" instead
of normal work. The user unblocks by responding.

## Layout

```
.clank/agents/<agent>/blocks/<plan>.md   — plan block
.clank/agents/<agent>/blocks/REPO.md     — repo-wide block
```

Blocks are gitignored (local to the agent's machine). The
file body explains why the agent is blocked.

## Semantics

- **Plan block**: the blocked plan shows "waiting on human"
  in status and wfw. Other plans and queue promotion proceed
  normally.
- **Repo block** (`REPO.md`): all work stops. wfw returns
  only the block item. Queue promotion is suppressed.
- Blocks are not commit-scoped — they apply to the plan or
  repo as a whole.
- Multiple agents can have blocks simultaneously.

## Creating a block

```
clank block <plan> -m "reason"
clank block --repo -m "reason"
```

Writes `.clank/agents/<self>/blocks/<plan>.md` or `REPO.md`.
The agent's label is resolved the same way as `clank as`.

## Unblocking

The user responds at `.clank/human/<plan>.md` or
`.clank/human/REPO.md` (git-tracked so all agents see the
response). The block file is deleted when the response is
read by wfw. Alternatively:

```
clank unblock <plan> -m "response"
clank unblock --repo -m "response"
```

This writes the response file and deletes the block.

## wfw integration

During derive_status or after:
1. Scan `.clank/agents/*/blocks/` for any agent's blocks.
2. If `REPO.md` exists from any agent → emit a single
   `WaitItem::HumanBlock` with scope=repo. No other work.
3. For each plan with a block → that plan's work item becomes
   `WaitItem::HumanBlock` with scope=plan. Other plans
   proceed normally.
4. If a response exists in `.clank/human/` for the block,
   the block is considered resolved — delete the block file
   and resume normal work.

## Status

`clank status` shows blocks prominently:

```
plan: foo
  BLOCKED: waiting on human (claude)
  reason: plan is drifting from user intent
```

## Stop-hook rendering

```
- blocked: plan `foo` waiting on human — claude asked:
  "plan is drifting from user intent"
  Respond via `clank unblock foo -m "your answer"`
```

## Hooks

New hook event `hooks.human_block` fires when a block is
created. The hook should make it hard for the user to miss
the request (e.g. `say` on macOS).

## Skill updates

Add guidance to both skill files: when reviews become
contentious, when the plan drifts, or when the work feels
unwise, use `clank block` instead of continuing.

## Tests

- Agent creates plan block → wfw returns HumanBlock for
  that plan, other plans proceed.
- Agent creates repo block → all work suppressed.
- User responds via clank unblock → block clears, work
  resumes.
- Status shows block reason.
- Stop-hook renders block item.
- human_block hook fires.
