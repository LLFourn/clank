---
description: Show or change clank config for this agent + repo.
---

User invoked `/clank` with arguments: "$ARGUMENTS"

- **If $ARGUMENTS is empty**: run `clank auto status` via shell and
  print the output, then suggest `/clank config` for an
  interactive picker.
- **If $ARGUMENTS is `config`**: use codex's structured-question
  tool (`elicitation_request` or equivalent) to present these
  options to the user:
  - "Enable auto-mode" → `clank auto on`
  - "Disable auto-mode" → `clank auto off`
  - "Switch role to master" → `clank auto on --role master`
  - "Switch role to reviewers" → `clank auto on --role reviewers`
  Run the matching command via shell, then re-print state.
- **Otherwise**: pass arguments through. Run `clank $ARGUMENTS`
  via shell and print the output.

No commentary; just the command output.
