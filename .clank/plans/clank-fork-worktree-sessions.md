# clank-fork — worktree + forked team sessions

`clank fork <name>` — create a linked git worktree and seed it so
the WHOLE TEAM continues there in forked sessions: try a
divergent approach, or host a PR review, without disturbing the
original repo's sessions.

**Opening follows the bare-verb convention** (finalized in
clank-open-zellij-context, which this plan inherits): run INSIDE
a zellij session, `clank fork <name>` defaults to OPENING the new
tab (master + reviewers in the fork) — `--no-open` opts out.
Outside zellij it does not auto-spawn a session; the worktree
path prints as the sole stdout line either way, so manual
composition still works:

```sh
clank open --repo "$(clank fork --no-open myfeature)"
```

## Command

```
clank fork <name> [<source-repo>] [--branch <ref>] [--path <dir>]
           [--prompt <text>] [--no-open]
```

- `<name>` — worktree dir name = new branch name = zellij tab.
- `<source-repo>` — defaults to the cwd's git toplevel.
- `--branch <ref>` — base the worktree's new branch on `<ref>`
  instead of source HEAD (the PR-review hook: fork onto a PR's
  fetched head).
- `--path <dir>` — override the default destination
  `<source-repo>/.clank/worktrees/<name>`.
- `--prompt <text>` — extra orienting context appended to the
  standard orientation prompt (see Sessions).
- `--no-open` — skip the default tab-opening when inside zellij
  (for scripting / manual composition). Outside zellij there is
  nothing to skip: fork never auto-creates a session.

## Decided design

1. **Precondition (hard-error)**: every registered agent in the
   resolved team (master + commit/gate reviewers) must have a
   bound session in the source repo. A fork with nothing to fork
   refuses, listing the agents missing sessions.
2. **Worktree**: `git worktree add` at the destination, NEW
   branch `<name>` off source HEAD (or `--branch <ref>`).
   Location default `.clank/worktrees/<name>`: gitignored (never
   pollutes main-repo status), one known place (the dir IS the
   registry — no separate state), basename feeds the tab name.
3. **Seeding**: only the gitignored `.clank/` bits need copying —
   repo config (team selection) + per-agent launch specs.
   Tracked state (`plans/`, `finished/`) arrives with the
   checkout.
4. **Sessions: fork-on-launch with an orienting prompt** (no id
   minting — ids materialize via the normal env-var binding when
   panes first start). Both tools fork CLEANLY — verified against
   installed binaries 2026-06-10 after lloyd challenged the stale
   research claim that codex couldn't:

   | tool   | launch command |
   |--------|----------------|
   | claude | `claude --resume <id> --fork-session` |
   | codex  | `codex fork <id> [PROMPT]` |

   True diverged copies, originals untouched — no shared-thread
   twins (the duplicate-session footgun class). Every forked
   session receives a standard orientation prompt — "you are in
   worktree `<name>` (branch `<ref-base>`) of `<repo>`, forked
   for: <--prompt text or 'parallel work'>" — codex via the fork
   PROMPT arg, claude via its initial-prompt launch config. The
   one mechanism serves both use cases: divergent experiments
   (context carried + reorientation) and PR review (context +
   "you're reviewing PR #N").
5. **Teardown: no wrapper.** `git worktree remove
   .clank/worktrees/<name>` is the whole teardown — no clank
   registry to clean, gitignored so forgotten forks pollute
   nothing, sessions in ~/.claude|~/.codex are harmless files.
   `fork`'s help + success output name the one-liner; plain
   rm -rf also works (git auto-prunes stale metadata). Same house
   rule that killed demote: don't wrap one git command in a verb.

## Open questions (sizing)

- **`repo_basename` on a linked worktree — VERIFIED ✓ (promote
  time)**: `RepoBasename::from_repo_root` is just the path's
  `file_name` (ids.rs), so a worktree at
  `.clank/worktrees/myfeature` yields `myfeature` — tab naming
  AND per-worktree identity isolation fall out for free. No fix
  needed.
- **Implementation seam (promote-time note)**: `agent start`
  already splits bootstrap (first launch binds via env-var hook)
  vs resume (`compose_launch` + per-tool `session_restore_args`,
  agent.rs:184/261/387). Fork seeding = a one-shot
  "fork_from: {tool, session_id, prompt}" in the WORKTREE's
  gitignored agent config, consumed by the bootstrap path to
  launch `claude --resume <src> --fork-session` /
  `codex fork <src> [prompt]`; the forked id then binds normally
  and subsequent starts hit the plain resume path.
- Does the worktree's `.clank/agents/` session-binding flow Just
  Work when panes launch with the env-var hook in a non-main
  worktree (wfw already supports linked worktrees — confirm the
  binding side).
- Zellij interplay: `clank open zellij --repo <worktree>` from
  INSIDE the main session opens a tab (current behavior, correct
  here); from outside it would create/attach session
  `clank-<name>` — both fine, note in help.

## Verification (in-process, no binary spawning)

- fork core: worktree exists on branch `<name>` off the right
  base; seeded config matches source team; launch specs carry the
  per-tool fork commands + orientation prompt; missing-session
  precondition refuses with the agent list; `--path`/`--branch`
  respected; stdout = path only.
- repo_basename(worktree) returns the worktree name.

## Relationship to clank-pr-review-mode

PR review rides on this: `clank fork pr-123 --branch <pr-head>
--prompt "reviewing PR #123: <title>"` gives the team a worktree
on the PR with oriented forked sessions; pr-review-mode adds the
review-artifact loop (cycles on refs/clank/pr-review/<pr>) on
top. Land fork FIRST.

## Status

Stub — design settled with lloyd 2026-06-09/10 (insist-on-
sessions; .clank/worktrees default; fork-on-launch with clean
forks on BOTH tools + orientation prompt; --branch for the PR
case; no teardown wrapper). Promotion-ready: queue when it's
time.
