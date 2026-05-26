---
name: clank
description: Multi-agent peer review around plans. Use clank commands to participate in review cycles; the Stop hook will continue this session when work is available.
---

# Clank

Clank is a peer-review workflow tool for multi-agent development.
Each repo's `.clank/` directory contains:
- `plans/` — one `.md` per active plan
- `agents/<label>/feedback/<commit>.md` — per-agent review notes
- `finished/` — finalized plans
- `config.json` — repo-level config (designated master agent)
- `agents/<label>/config.json` — per-agent local config (gitignored)

## Key commands (run via shell)

- `clank status` — current plan + gate state
- `clank wfw` — wait-for-work; long-polls for the next thing this
  agent should do. Author + role inferred from your session
  binding (`clank as`) — no flags needed in the common case.
- `clank feedback write --commit Y --verdict
  approve|request-changes --author <label> -m "<message>"` —
  write your review feedback. `-m` is the review message (like
  `git commit -m`): first line is a summary, then details. The
  tool prepends the verdict to the file. Write messages as you
  would a commit message.
- `clank finish <plan>` — finalize an approved plan (master only).
- `clank as <label>` — bind this session to an agent label (you'll
  typically run this once per session at the start).
- `clank auto on|off [--role …]` — toggle the Stop-hook auto-mode
  for this session.

## Stop-hook continuations

When the Stop hook returns work (codex shows it as
"Stop hook (blocked) feedback: …"), act on it immediately.
Reviewer prompts include the exact `clank feedback write …`
invocation to use — run it verbatim.

**Queue promote items**: do NOT blindly promote. Read the
queued plan file, evaluate whether it is well-scoped and
ready to implement given the current codebase. Edit or
rescope as needed — split into smaller plans if appropriate
(leave unready parts in the queue). Only run
`clank queue promote <name>` when the plan is ready.

## /clank slash command

See `~/.codex/commands/clank.md` for the interactive flow.
