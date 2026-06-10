# zellij-session-dedup — `clank open zellij` attaches instead of duplicating

Running `clank open zellij` repeatedly creates a NEW zellij
session each time (zellij auto-names them: undulating-cactus,
friendly-magpie, unique-quasar…), each spawning fresh
`claude --resume` / `codex resume` panes for the SAME agent
sessions. Closing a terminal only DETACHES a zellij session — the
server and its agent panes keep running — so the duplicates pile
up invisibly. Observed twice in one day: 2-3 live instances of
every agent racing on the same stop-hook work items, interleaved
edits corrupting each other (lloyd 2026-06-10).

## The fix: deterministic session name + attach-or-create

- Name the session deterministically per repo:
  `clank-<repo-basename>` — today `compose_spawn_argv` passes no
  `--session`, so zellij invents a random name every spawn,
  making duplicates undetectable.
- On `clank open zellij`:
  1. Already inside a zellij session (`$ZELLIJ` set): keep
     today's behavior (the layout opens as a new tab in the
     current session).
  2. A LIVE session named `clank-<basename>` exists →
     `zellij attach clank-<basename>` — resume the running
     panes; do NOT spawn new agent instances. Print what
     happened ("attached to existing session").
  3. The named session exists but is DEAD/EXITED (serialization
     is off, so it can't resurrect) → delete it
     (`zellij delete-session`) and create fresh.
  4. No session → create with the layout + the deterministic
     name.
- Detection: parse `zellij list-sessions` (note: output is
  ANSI-colored; `--no-formatting`/`-n` or strip), or probe the
  attach. Pin the exact mechanism at sizing — including how
  `options --session-serialization false` combines with
  attach-vs-create argv shapes (`zellij --session <name>
  --layout <path> options …` for create; plain `attach` for
  resume).

## Verification

- In-process compose tests: spawn argv includes
  `--session clank-<basename>`; attach argv shape for the
  exists case; the $ZELLIJ branch unchanged.
- The decision fn (list-sessions output → create/attach/delete+
  create) is pure over the parsed session list — unit-test the
  three branches with canned output (live / exited / absent).

## Related, out of scope

- The deeper per-agent guard (pidfile per (agent, session);
  `clank as`/`agent start` refusing when the bound session
  already has a live process anywhere — covers duplicates
  spawned OUTSIDE zellij too, like the original ttys005 twin).
  Separate stub if wanted; this plan kills the common path.
- `zellij-default-layout` (in flight) touches the same file
  (compose_spawn_argv/run) — LAND THIS AFTER it to avoid
  conflicts.

## Status

Stub — queued lloyd 2026-06-10 ("clank open zellij should
resume one if it's already running somehow").
