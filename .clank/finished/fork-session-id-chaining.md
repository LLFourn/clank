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

## Findings + fix

Ruled OUT (code + real-data): clank never writes `from_session` into
a config — it only passes it to `codex fork` / `claude --resume`
(grep-confirmed). And clank binds whatever `CODEX_THREAD_ID` /
`CLAUDE_CODE_SESSION_ID` reports (`agent_env::detect_session_from_env`
via `clank as`). Live evidence from the real frostsnap repo: the two
one-level forks (pr-496, pr-497) bound DISTINCT, correct codex ids
(neither is main's), so the env-based bind captures the NEW forked id
correctly at one level. So the bug is NOT clank stamping the ancestor
id at bind time.

ROOT CAUSE found in clank: the fork spec `fork.json` is documented
ONE-SHOT ("consumed on first launch") but `clank agent start` only
`load`ed it — never deleted it. Confirmed live: pr-496/pr-497 still
have `fork.json` on disk post-launch, with `from_session` = main's
ids. A lingering spec is an ancestor-resurrection trap: a fork's
binding can be cleared (e.g. `bind_session_to_agent` clears a stale
duplicate that shares a session id), and the next `clank agent start`
sees no bound session, RE-consumes the stale spec, and
`codex fork <ancestor>` again — so the agent lands back in an
ancestor-content session. A fork-of-a-fork then chains off the
grandparent. That matches "C shows A's last message".

Fix (implemented): `take_fork_spec` (load + delete) enforces the
one-shot contract; `clank agent start` consumes via it on a real
launch (`--print` peeks without consuming). Unit-tested
(`take_fork_spec_is_one_shot`): a consumed spec is gone, a relaunch
finds nothing → no ancestor re-fork.

Is CLAUDE affected? YES — and fixed by the same change. The bug is
TOOL-AGNOSTIC: `run_fork` writes a `fork.json` for every team member
(frostsnap's worktrees have a claude `fork.json` too), and the
non-deletion was in `load_fork_spec`/`agent start`, which are shared
across tools. So a claude fork whose binding is later lost would
likewise re-fork its ancestor via `claude --resume <src>
--fork-session`. `take_fork_spec` consumes the spec for any tool, so
both are fixed. lloyd hit it on codex, but it was never
codex-specific.

Live confirmation: CONFIRMED by lloyd — re-ran the fork chain with the
fixed binary (the one-shot delete is on master and was installed via a
later `cargo install`) and the chaining symptom is gone. So the
one-shot fork-spec fix resolves the reported bug; no codex-side
residual surfaced. (If it ever recurs, the check is: B's bound id ==
B's working session, and C's `fork.json` `from_session` == B's id.)
