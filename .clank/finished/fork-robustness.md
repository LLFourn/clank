# fork-robustness

`clank fork` currently fails hard on repos that aren't fully set up:
no roster → bail; any roster member without a bound session → bail.
Make forking robust: session forking becomes BEST-EFFORT (warn and
create fresh sessions instead of failing), and the fork's team can
come from a user-scope team template instead of the source repo.

## The three cases

1. **Source repo was never `clank init`ed (no roster)** — warn and
   continue with the DEFAULT team: resolve the user-scope team named
   `default` (same resolution as `init --team`, refs against the
   user-scope agents library) and seed the fork's roster from it. If
   the user library has no `default` team, keep a hard error — but it
   must name both remedies (define a `default` team, or pass
   `--team <name>`).

2. **`--team <NAME>`** — seed the fork's roster from the named
   user-scope team template instead of copying the source's
   `.clank/config.json`. Members that also exist in the SOURCE repo
   with a bound session get their sessions forked; the rest get fresh
   sessions (case 3). Unknown team / dangling refs → fail fast BEFORE
   any mutation (this error is legitimate — it's a typo, not a
   degraded environment).

3. **Roster member with no bound session** — never fail. Warn
   ("no session for `X` — a fresh session will be created on launch")
   and write a spec that bootstraps instead of forking. The
   "every team member needs a live session" bail and the underlying
   "a fork with nothing to fork is meaningless" precondition are
   deleted — a fork where every session is fresh is meaningful; it
   gets the worktree, the roster, the queue seeds, and orientation.

## Mechanism

- `ForkSpec.from_session` becomes `Option<String>`. `Some` → today's
  fork-the-session launch. `None` → `compose_fork_launch` falls
  through to a bootstrap-style launch that still carries the spec's
  ORIENTATION prompt as the initial prompt — a fresh session in a
  fork must not start blind. (Serde: old on-disk specs carry the
  string; `None` simply omits — one-shot consumption via
  `take_fork_spec` is unchanged.)
- Every fork-roster member gets a spec (orientation is universal);
  `from_session` is filled per-member from the SOURCE repo's agent
  config, best-effort.
- Carbon-copy of per-agent settings (auto_mode/wait_timeout) stays,
  applied only to members that exist in the source.
- With `--team` (or the default-team fallback), the fork's
  `.clank/config.json` is BUILT from the resolved roster (as
  `init --team` writes it), not copied from the source. Non-roster
  config sections (review/hooks/diff/zellij) copy from the source
  when a source config exists; document whichever is chosen in the
  code.
- Warnings go to stderr alongside fork's existing eprintln reporting;
  stdout stays the sole worktree path (composition contract).

## Interaction

- `--team` + case 1: `--team` wins (no default-team lookup).
- Case 1 + case 3 compose: a roster from the default team has no
  source sessions at all → every member bootstraps fresh, one warning
  summarizing it.
- `--draft` seeding is orthogonal and unchanged.

## Tests (in-process, extend fork_integration.rs; spec-compose units)

- No-roster source + user library with a `default` team → fork
  succeeds with a warning, fork roster == default team, every spec
  has `from_session: null`.
- No-roster source + no `default` team → error naming both remedies;
  no worktree.
- `--team custom` where one member matches a source agent with a
  bound session and one is new → matched member's spec forks the
  session, new member's spec is `from_session: null`, fork roster ==
  the template.
- Copy-roster path with ONE unbound member → fork succeeds, that
  member `from_session: null`, others forked (the old bail is gone).
- `agent start --print` (or the compose unit) with a `None` spec →
  bootstrap launch that carries the orientation prompt.
- Unknown `--team` → fail before worktree creation.

## Acceptance

- All three cases fork successfully (or fail fast only on a genuine
  typo: unknown team, dangling team refs).
- Warnings on stderr; stdout still exactly one line (the worktree
  path).
- Old on-disk fork specs still parse.
- clippy/fmt/suites green at baseline.
