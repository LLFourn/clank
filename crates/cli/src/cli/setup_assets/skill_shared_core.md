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

- **Bind once**: run `clank as <label>` at session start. Your ROLE is
  roster-derived — you are master or reviewer because the repo roster
  says so, not because of a flag (`clank auto --role` is a no-op).
{{WORK_LOOP}}

## Core commands (run via {{SHELL}})

- `clank status` — current plan + gate state
- `clank wait` — wait-for-work; long-polls for your next action (author +
  role inferred from your `clank as` binding — no flags needed).
- `clank as <label>` — bind this session to an agent label
- `clank auto on|off` — toggle this session's Stop-hook auto-mode
