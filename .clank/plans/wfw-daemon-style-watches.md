# wfw-daemon-style-watches

## Summary

`clank wfw` should return to the old Trinity daemon's coarse
watching strategy for Git and Clank metadata. The current watcher
uses fine-grained watch roots such as `git_dir/HEAD`,
`git_dir/logs/HEAD`, `git_common_dir/refs`, `packed-refs`, and
individual `.clank/{plans,feedback,finished}` directories. That is
more precise on paper, but it is not robust in the environment we
care about most: agents running `clank wfw` from Codex.

The key observation from debugging is stark:

- Outside the Codex sandbox, the current fine-grained watcher wakes
  quickly on `git commit -m`.
- Inside the Codex sandbox, the same binary and same repro script
  can miss the fine-grained Git file/ref events and `wfw` times out.
- Temporarily switching `wfw` to the old daemon-style directory
  watches fixed the sandbox repro: five consecutive commit wakes
  returned in roughly 230ms with no polling.

So this is not a normal macOS `notify` failure and not a stale binary
issue. Something about Codex sandboxing breaks or filters native
notifications for the individual Git files/refs we currently watch.
Directory-level watches still work.

## Current vs Old Watch Shape

Current `clank wfw` watches:

```text
<resolved worktree gitdir>/HEAD          non-recursive
<resolved worktree gitdir>/logs/HEAD     non-recursive, if present
<git common dir>/refs                    recursive
<git common dir>/packed-refs             non-recursive, if present
<repo>/.clank/plans                      recursive
<repo>/.clank/feedback                   recursive
<repo>/.clank/finished                   recursive
```

The old filesystem-truth Trinity daemon watched:

```text
<repo>/.trinity                 recursive
<resolved worktree gitdir>      non-recursive
```

For a regular repository the resolved gitdir is `<repo>/.git`. For a
linked worktree it is the per-worktree gitdir pointed to by the
`.git` file, usually under `<main-repo>/.git/worktrees/<name>`.

## Proposed Change

Change `clank wfw` to use broad invalidation roots:

```text
<repo>/.clank                  recursive
<resolved worktree gitdir>     non-recursive
```

Keep using `notify::RecommendedWatcher`; do not add polling as the
primary mechanism. The old daemon shape is sufficient to make the
sandbox repro pass, and it also better matches how Git actually
updates metadata: lockfiles, appends, renames, and temporary files in
or under the gitdir. `wfw` does not need to know which exact Git file
changed; any relevant Git metadata movement can simply trigger a
refold.

The implementation should preserve linked-worktree behavior. If a
case truly requires shared refs from `git_common_dir`, add a broad
watch root for the relevant common directory rather than returning to
fragile individual file watches. Prefer coarse invalidation over
precise-but-unreliable notification paths.

## Tests And Repro

Keep a script-level repro for the Codex sandbox issue. The repro
should fail fast on any non-zero `wfw` exit and print enough detail to
see whether wakeups are real watcher wakeups or timeout/heartbeat
fallbacks. The current useful repro is:

```text
/private/tmp/clank-watch-repro/repro-git-commit-timeout.sh
```

The important behavior is:

- before the fix, inside the Codex sandbox, it can fail with
  `rc=2` and `stderr=wfw timed out`;
- outside the Codex sandbox, the same binary usually passes;
- after broad directory watches, inside the Codex sandbox, it should
  pass repeatedly with sub-second latency after `git commit -m`.

Add or update Rust tests for the watch-root selection and path
resolution. Do not require a Rust unit test to reproduce Codex's
sandbox notification behavior directly if the behavior only appears
under the Codex tool sandbox. The reproducible artifact for that
specific environment bug can remain a shell repro, while Rust tests
cover the deterministic pieces: resolved gitdir selection, `.clank`
root creation, and broad watch root registration.

## Acceptance Criteria

- `clank wfw` no longer watches individual Git files such as
  `HEAD`, `logs/HEAD`, or `packed-refs` as its primary wake signal.
- `clank wfw` watches `.clank` recursively instead of separately
  watching `.clank/plans`, `.clank/feedback`, and `.clank/finished`.
- The Codex-sandbox repro passes with the default debug binary after
  rebuilding.
- Existing `wfw` integration tests still pass, including linked
  worktree coverage.
- No polling fallback is introduced in this plan. If a future fallback
  is needed, it should be explicit and justified separately.
