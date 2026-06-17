# fork-session-id-chaining

A fork-of-a-fork inherits the WRONG ancestor. Repro:

1. session A
2. from A: `clank fork` → session B
3. from B: `clank fork` → session C

In C, the last timeline message is from **A**, not B. So forking
from B used A's session id as the fork source — `clank fork` is
forking off the id B was forked FROM, not B's own current id. (Seen
on codex / frostsnap.)

## The invariant being violated

Each forked session must RE-BIND to its own new id, so that the
per-worktree agent config (`.clank/agents/<label>/config.json`) names
the session actually running in that worktree. `clank fork` then
reads that bound id as the fork source. If a forked session's config
keeps pointing at its grandparent, forks chain off the original
forever — exactly the symptom.

## Where to look

- `run_fork` records the source: `from_session = session.id` from
  `load_agent_config(&source, label)` (fork.rs:155, :245). So when
  run from B, it reads B's worktree agent config. If that holds A's
  id, the fork is off A. → the bug is upstream, in how B's config got
  bound.
- Re-bind path: a forked session is told to `clank as <label>`
  (fork spec prompt), which resolves the current session id from the
  env (`CODEX_THREAD_ID` / `CLAUDE_CODE_SESSION_ID`, agent_env.rs)
  and writes it to the worktree config. The summary's "the forked id
  binds via the env-var hook" lives here.

## Primary hypothesis

In a forked **codex** session, the session-id env var seen at
`clank as` time is the SOURCE id (the forked-from `A`), not the new
forked id (`B`). `compose_fork_launch` runs `codex fork <A_id>`,
which mints a NEW conversation — but if codex sets `CODEX_THREAD_ID`
to the forked-FROM id (or doesn't update it until/unless a turn
completes), `clank as` binds A's id into B's config. Then fork from B
reads A's id → C forks off A.

(Claude's fork is `--resume <src> --fork-session`; check whether it
has the same flaw or correctly reports the new id, since the user hit
this on codex — it may be codex-specific like the `-C/--cd` picker in
[[fork-codex-cd-flag]].)

## What the investigation must produce

1. Reproduce A→B→C and capture, at each level: the actual new session
   id, the value of `CODEX_THREAD_ID`/`CLAUDE_CODE_SESSION_ID` inside
   the forked session, and what landed in the worktree's
   `agents/<label>/config.json`. Pinpoint where A's id substitutes
   for B's.
2. Decide the fix by where it breaks:
   - if the env reports the source id → clank must capture the new
     forked id another way (codex may print the new id on fork; or
     query codex's latest session for that cwd; or a codex flag),
   - if the env is right but `clank as`/the hook reads stale/wrong →
     fix the resolution,
   - if `run_fork` reads the wrong config dir → fix the source
     resolution.
3. State whether claude is affected too.

## Verify live

The id capture can be checked the cheap way: in a forked codex
session, `echo $CODEX_THREAD_ID` and compare to the actual new
session id (and to the source id). That single comparison likely
localizes the bug before any code change.

## Tests

- Pure where possible (e.g. the source-id resolution in `run_fork`
  given a config). The end-to-end fork chain is verified manually /
  via git+config inspection (no clank-binary spawning in tests).

## Non-goals

- The codex working-dir picker ([[fork-codex-cd-flag]]) — separate
  codex-fork issue, separate plan.
