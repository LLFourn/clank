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
  approve|finished|request-changes --author <label> -m "<message>"`
  — write your review feedback. `-m` is the review message (like
  `git commit -m`): first line is a summary, then details. The
  tool prepends the verdict to the file. Verdicts:
  - **APPROVE**: this commit's work is good. Mid-flight signal —
    master keeps working. When you approve but don't think the
    plan is fully done, include a one-sentence reason it's not
    FINISHED yet (e.g. "tests still missing"). This keeps master
    oriented on what's left.
  - **FINISHED**: this plan is done — `clank finish` should run.
    For a research-style plan whose output is the document
    itself, mark FINISHED on the plan-text commit when the
    document is complete.
  - **REQUEST_CHANGES**: something needs to change.
- `clank finish <plan>` — finalize a FINISHED plan (master only).
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

**Blocks**: if reviews become contentious, the plan is
drifting from user intent, or the work feels unwise, use
`clank block create <name> --plan <plan-stem> -m "question"`
to ask the human. Scope is mandatory: `--plan <stem>` targets
one plan (the usual case); `--all` suppresses every wfw item
across all plans + queue items (rarely the right call).
Use `clank block clean` to acknowledge answered blocks.

