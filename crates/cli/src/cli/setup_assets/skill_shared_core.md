# Clank

Clank is a peer-review workflow for multi-agent development. Each repo's
`.clank/` holds:
- `plans/` — one `.md` per active plan; `finished/` — finalized plans
- `queue/` — queued plan drafts awaiting promotion
- `config.json` — the repo ROSTER: a flat list of agents, each with a
  role (`master` / `commit` / `gate`). NOT a single "master" setting.
- `agents/<label>/config.json` — per-agent local state, gitignored
  (session binding, auto-mode)
- `agents/<label>/feedback/<sha>.md` — a reviewer's notes on a commit

## Always (both roles)

- **Bind once**: run `clank as <label>` at session start. Your ROLE is
  roster-derived — you are master or reviewer because the repo roster
  says so, not because of a flag (`clank auto --role` is a no-op).
- **Act on Stop-hook work IMMEDIATELY, then YIELD.** When the Stop hook
  hands you work, do it now.{{STOP_HOOK_NOTE}} Run `clank status` if you
  need more than the hint carries (short SHAs resolve wherever a `<sha>`
  is wanted).
  Each item is a one-line hint: kind, plan, short sha.
- **NEVER poll.** Do not loop on `clank wait` or re-run `clank status`
  waiting for state to change. STOP — the Stop hook re-invokes you when
  there is work. You WILL be woken; do not spin.

## Core commands (run via {{SHELL}})

- `clank status` — current plan + gate state
- `clank wait` — wait-for-work; long-polls for your next action (author +
  role inferred from your `clank as` binding — no flags needed). Formerly
  `clank wfw`, which still works as an alias.
- `clank as <label>` — bind this session to an agent label
- `clank auto on|off` — toggle this session's Stop-hook auto-mode
