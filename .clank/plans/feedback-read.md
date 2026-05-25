# feedback-read

## Summary

`clank feedback read` — show all feedback for a commit.

## Design

```
clank feedback read [--commit <sha>] [--repo <path>] [--json]
```

Defaults to HEAD. Shows every feedback file for the commit:
author, verdict, source path, and body. Not role-gated —
any agent can read feedback.

### Output (human)

```
commit abc1234
  codex  APPROVE: clean impl, one non-blocking nit
    .clank/agents/codex/feedback/abc1234.md

  claude  REQUEST_CHANGES: overwrought API in foo.rs
    - [P1] simplify the trait surface
    .clank/agents/claude/feedback/abc1234.md
```

### Output (--json)

```json
[
  {
    "author": "codex",
    "verdict": "approve",
    "summary": "clean impl, one non-blocking nit",
    "source_path": ".clank/agents/codex/feedback/abc1234.md"
  }
]
```

## Implementation

- Add `Read` variant to `FeedbackCmd` in `cli/mod.rs`.
- `FeedbackReadArgs`: `--commit` (optional, default HEAD),
  `--repo`, `--json`.
- `cli/feedback.rs`: resolve commit, walk
  `agents/*/feedback/<sha>.md`, read bodies, render.
