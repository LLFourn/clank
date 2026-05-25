---
name: clank
description: Multi-agent peer review around plans. Use clank commands to participate in review cycles; the Stop hook will continue this session when work is available.
---

# Clank

Clank is a peer-review workflow tool for multi-agent development.
Each repo's `.clank/` directory contains:
- `plans/` — one `.md` per active plan
- `agents/<label>/feedback/<plan>/<commit>.md` — per-agent review notes
- `finished/` — finalized plans
- `config.json` — repo-level config (designated master agent)
- `agents/<label>/config.json` — per-agent local config (gitignored)

## Key commands (run via Bash)

- `clank status` — current plan + gate state
- `clank wfw` — wait-for-work; long-polls for the next thing this
  agent should do. Author + role inferred from your session
  binding (`clank as`) — no flags needed in the common case.
- `clank feedback write --plan X --commit Y --verdict
  approve|request-changes --author <label>` (body on stdin) —
  write your review feedback. The first line of the body MUST
  be `APPROVE <one-line summary>` or `REQUEST_CHANGES <summary>`,
  like a git commit message: verdict + summary on line 1, blank
  line, then detailed body. Example:
  `APPROVE No findings.\n\nVerification: cargo test passed.`
- `clank finish <plan>` — finalize an approved plan (master only).
- `clank as <label>` — bind this session to an agent label (you'll
  typically run this once per session at the start).
- `clank auto on|off [--role …]` — toggle the Stop-hook auto-mode
  for this session.

## /clank slash command

User invoked `/clank` with arguments: "$ARGUMENTS"

- **If $ARGUMENTS is empty**: run `clank auto status` via Bash and
  print the output, then suggest `/clank config` for an
  interactive picker.
- **If $ARGUMENTS is `config`**: use the structured-question tool
  (`AskUserQuestion` in claude; `elicitation_request` /
  equivalent in codex) to present these options to the user:
  - "Enable auto-mode" → `clank auto on`
  - "Disable auto-mode" → `clank auto off`
  - "Switch role to master" → `clank auto on --role master`
  - "Switch role to reviewers" → `clank auto on --role reviewers`
  Run the matching command via Bash, then re-print state.
- **Otherwise**: pass arguments through. Run `clank $ARGUMENTS`
  via Bash and relay the full stdout to the user as your response.

No commentary; just the command output. The user cannot see tool
call results directly — you must include the output in your
response text.

## Stop-hook continuations

When the Stop hook returns work, **act on it immediately**.
Reviewer prompts include the exact `clank feedback write …`
invocation to use — run it verbatim.
