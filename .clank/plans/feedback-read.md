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
    "details": "No blocking findings.\n...",
    "source_path": ".clank/agents/codex/feedback/abc1234.md"
  }
]
```

`summary` is the first line after the verdict header.
`details` is everything after the blank line separator
(same split as `FeedbackBody::summary()` / `details()`).

## Implementation

- Add `Read` variant to `FeedbackCmd` in `cli/mod.rs`.
- `FeedbackReadArgs`: `--commit` (optional, default HEAD),
  `--repo`, `--json`.
- `cli/feedback.rs`: resolve commit, walk
  `agents/*/feedback/<sha>.md`, read bodies, render.

## Tests

- Human output shows author, verdict, summary, and body.
- `--json` output includes `summary` and `details` fields
  for a feedback file with both.
- Short ref finds full-SHA feedback file.
- Invalid ref errors cleanly.
