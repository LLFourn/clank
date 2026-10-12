# a-stop-from-a-subdirectory-finds-its-repo
# A Stop from a subdirectory finds its repo

## Why

In `~/src/frostsnap/.clank/worktrees/split-backup-formal` the master, `claude`, stopped with
work pending (gate continued on 46d1df7), and nothing woke it. Its binding matched its
session and auto mode was on. Its transcript shows the Stop hook ran ("Clank: watching
for work", asyncRewake), yet no `clank wait --author claude` was ever parked, and no hook
process for the worktree survived. Forty minutes later the stall watchdog posted `⚡
@claude recovery "claude has had pending work for 40m and is not waiting for it"`.

The session's cwd at Stop was `…/split-backup-formal/docs/split-backup/formal`: the agent
had `cd`'d into the formal proof directory. `stop_hook::resolve_hook_repo` passes the
hook's cwd to `resolve_repo(Some(cwd))`, and `resolve_repo` only canonicalizes an explicit
path. Only the no-argument path walks up to the git work tree. So the hook took the
subdirectory for the repo; `clank status --repo <that dir>` fails with "does not appear
to be a git repository". The hook's own error text ("is not inside a git repo") shows the
intent was to discover the repo, not require the cwd to be its root.

That failure was also silent: the transcript's `stop_hook_summary` shows no errors and no
output. Making a hook that cannot decide visible to its session is a protocol change.
Today every `Failed` is a stderr diagnostic with exit 0, and exit 2 is reserved for a Claude
continuation. A wake on failure also needs a guard against an unfixable error waking a
session forever. So it is its own plan (`a-hook-that-cannot-decide-is-seen`, drafted
beside this one). This plan fixes the cause.

## What to build

An ambient cwd is where discovery STARTS. An explicit `--repo` is the exact path, as
today.

- **One shared helper for ambient cwds.** Add `resolve_cwd_repo(cwd)`, which discovers the
  git work tree containing `cwd` (`git_io::discover_work_dir`) and canonicalizes it.
  `resolve_repo(None)` already does this for the process cwd, so the two share it.
  `resolve_repo(Some(path))` stays exact.
- **Every caller holding a cwd it did not choose uses it.** The audit, each reported in As
  built with what changed or why not:
  - `stop_hook::resolve_hook_repo` (the Stop path and the recovery modes that reuse it);
  - the SessionStart handler's resolution (`stop_hook.rs`, `--session-start`);
  - `wait.rs`'s `automatic_grok` resolution from its env cwd;
  - native Grok's cwd resolution and validation (`grok_hook.rs`);
  - any other adapter that resolves a session or hook cwd.
- **The identity is the containing worktree.** A nested directory in a linked worktree
  resolves to that worktree, not the main checkout, and a symlinked spelling to the same
  canonical root. The hook resolves from the payload's cwd, not the hook process's own
  cwd, which may differ. Commands that run sources keep their own cwd.

## Acceptance

- A Stop whose payload cwd is a nested directory of a clank linked worktree arms the wait
  for that worktree's agent. The test builds a linked worktree, binds an agent, and runs
  the hook decision with the payload cwd nested and the process cwd elsewhere.
- A symlinked spelling of that nested directory resolves to the same canonical worktree.
- SessionStart from a nested cwd binds the session to that worktree's agent.
- `--repo <subdir>` stays exact (no discovery).
- A cwd outside any git repository behaves as today. The test pins today's outcome; it does
  not claim one.
- Mutation-checked: resolving the payload cwd literally, resolving from the hook
  process's cwd instead of the payload's, and resolving a linked worktree to its main
  checkout each fail a test.

## Out of scope

Surfacing a hook that cannot decide to its session (its own plan); the stall watchdog and
its recovery event, which did their job; leaked stop-hook processes in other worktrees.

## As built

- `cli::resolve_cwd_repo(cwd)` canonicalizes the cwd, discovers the git work tree that holds
  it, and canonicalizes that root. `resolve_repo(None)` now goes through it, and
  `resolve_repo(Some(path))` stays exact. Canonicalizing first was a finding of the
  symlink test: discovery walks up the path as spelled, so a symlink to a worktree's
  subdirectory placed in the main checkout resolved to the main checkout.
- The audit, by caller:
  - `stop_hook::resolve_hook_repo` (Stop, and the recovery modes that reuse it): now
    discovers from the payload cwd; `--repo` exact.
  - SessionStart: `stop_hook::session_start_repo`, the same rule; it still fails open.
  - `wait::automatic_grok`: its env cwd goes through `resolve_cwd_repo`; an explicit
    `--repo` still has to equal that worktree.
  - native Grok (`grok_hook::hook_repo` for dispatch, and `handle`'s validation): both
    discover from the payload cwd, so a session working in a subdirectory is accepted,
    and a repo that does not hold its cwd is still refused.
  - `web::claude_questions` (three hook-cwd resolutions: the label match, `record`, and
    the broker socket): the same bug class, a question asked from a subdirectory would
    have been lost; now `resolve_cwd_repo`.
  - `send::reply_route` already discovered and now uses the helper.
  - `events/operation.rs` resolves a sender repo recorded in a message, not a cwd, and
    is left as is.
- Tests drive the real paths: the Stop decision for an agent bound in a linked worktree,
  from a nested payload cwd with the process cwd elsewhere; symlink equivalence;
  SessionStart; native Grok's session_start and stop from a nested cwd, with a foreign
  repo refused; `automatic_grok` from a subdirectory. Mutation-checked, each caught:
  resolving the cwd literally (helper, SessionStart, Grok dispatch, Grok validation,
  automatic_grok), resolving from the process cwd, skipping the canonicalization, and
  resolving a linked worktree to its main checkout.
