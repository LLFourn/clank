# Clank

Clank is a peer-review workflow for multi-agent development. Each repo's
`.clank/` holds:
- `plans/` — one `.md` per active plan; `finished/` — finalized plans
- `queue/` — queued plan drafts awaiting promotion
- `config.json` — local, gitignored repo config: the repo ROSTER as a
  flat list of agents, each with a role (`master`, a reviewer tier:
  `commit` / `plan` / `final` / `gate` — gate folds into both the plan-
  and final-stage reviews — or `standby`). NOT a single "master" setting.
- `agents/<label>/config.json` — per-agent local state, gitignored
  (session binding, auto-mode)

## Always (every role)

- **Already bound**: a pane clank opened binds at session start and
  rebinds after `/clear`; run `clank as <agent>` only in a session
  you started by hand. Your ROLE is roster-derived — master, reviewer
  or standby because the roster says so, not a flag.
{{WORK_LOOP}}
- **Mark all background work**: anything outliving a command (builds,
  tests, servers, subagents) runs as `clank run --desc "two words" --
  <cmd>`, {{BACKGROUND_RUN}}, or is declared with `clank attend
  "<name>"`. No exceptions, for any tool.
- **Never wait for it in a turn**: start it, end the turn. A run wakes
  you when its process ends; its output and exit status are where you
  launched it. {{DECLARED_ENDS}} It checks in once at
  `--expect` (5m); messages don't end it. Never poll it or start a wait.
- **clank detects no background work**: what you don't mark isn't
  tracked; messages wait for your turn to end. A turn silent 45m while
  owing work is nudged, which interrupts it.

## Core commands (run via {{SHELL}})

- `clank status` — current plan + gate state
- `clank auto on|off` — toggle this session's Stop-hook auto-mode

## Messages (every role)

Use `clank events read <sel>` to read a message and record delivery; act, then
`clank events ack <sel>` (it re-wakes until acked). `list`/`show` are metadata-only.
Reply to its sender/repo with `clank send [--repo P] (--to <label> | --master) -m "…"`.
