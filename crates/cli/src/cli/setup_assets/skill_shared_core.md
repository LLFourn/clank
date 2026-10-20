# Clank

Clank is a peer-review workflow for multi-agent development. Each repo's
`.clank/` holds:
- `plans/` — one `.md` per active plan; `finished/` — finalized plans
- `queue/` — queued plan drafts awaiting promotion
- `config.json` — local, gitignored repo config: the repo ROSTER as a
  flat list of agents, each with a role (`master`, or a reviewer tier:
  `commit` / `plan` / `final` / `gate` — gate folds into both the plan-
  and final-stage reviews). NOT a single "master" setting.
- `agents/<label>/config.json` — per-agent local state, gitignored
  (session binding, auto-mode)
- `agents/<label>/feedback/<sha>.md` — a reviewer's notes on a commit

## Always (both roles)

- **Already bound**: a pane clank opened binds at session start and
  rebinds after `/clear`; run `clank as <agent>` only in a session
  you started by hand. Your ROLE is roster-derived — master or
  reviewer because the roster says so, not a flag.
{{WORK_LOOP}}
- **Long background work**: run it as `clank run --desc "two words"
  -- <cmd>`, or for a running task `clank attending <task-id> --desc "…"
  --pid <pid>`. Past `--expect` (5m) you get one check-in. Its completion
  still wakes you: never poll it or start a wait.

## Core commands (run via {{SHELL}})

- `clank status` — current plan + gate state
- `clank auto on|off` — toggle this session's Stop-hook auto-mode

## Messages (both roles)

Use `clank events read <sel>` to read a message and record delivery; act, then
`clank events ack <sel>` (it re-wakes until acked). `list`/`show` are metadata-only.
Reply to its sender/repo with `clank send [--repo P] (--to <label> | --master) -m "…"`.
