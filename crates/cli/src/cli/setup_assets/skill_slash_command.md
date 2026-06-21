
## `/clank` slash command

User invoked `/clank` with arguments: "$ARGUMENTS"

- **If $ARGUMENTS is empty**: run `clank auto status` and print the
  output, then suggest `/clank config`.
- **If $ARGUMENTS is `config`**: use `AskUserQuestion` to offer:
  - "Enable auto-mode" → `clank auto on`
  - "Disable auto-mode" → `clank auto off`
  Run the chosen command, then re-print state. (Role is roster-derived —
  change it with `clank agent promote` (elevates an agent to master — a
  roster op, distinct from promoting a queued plan) / `clank agent add`,
  NOT via `clank auto`.)
- **Otherwise**: run `clank $ARGUMENTS` and relay the full stdout.

No commentary; just the command output. The user cannot see tool results
directly — you must include the output in your response text.
