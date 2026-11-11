# one-watch-for-many-repos
# `clank status --watch -j` watches many repos in one process, so a dashboard needs one child, not one per repo

## Why

On 2026-11-11 clankstack (the fs-war-room site) ran 88 `clank status --watch -j --repo <r>` processes, one per repo, about 28% CPU between them. Each holds its own file watchers and its own tokio runtime, and before `work-only-when-something-happens` each walked its worktree on a 5-second beat.

LLFourn asked whether clankstack should use clank as a library. It shouldn't. A linked library carries the clank it was compiled against, and the monitors write state in whatever shape the installed clank uses. That is the version skew `no-process-outlives-its-build` solved, by keeping consumers on a CLI process that re-execs into the installed build. One process watching many repos keeps that boundary and drops the per-repo cost.

LLFourn, 2026-11-11: "you can queue the plan to get clank watch to accept a bunch of repos."

## Build

1. **Many repos.** `clank status --watch -j` accepts `--repo` more than once, and `--repos-from <path|->` to read newline-separated repo paths (a file, or stdin). With one repo and no new flag, behaviour and output are byte-for-byte unchanged.
2. **One stream.** In multi-repo mode, each output line is one JSON object for one repo: the existing snapshot plus a `"repo": "<path>"` field. A repo's line is emitted only when its own snapshot changes. Lines from different repos never interleave mid-line.
3. **The set can change.** With `--repos-from -`, stdin stays open. A line `+<path>` adds a repo and `-<path>` removes one, so clankstack can follow its config without restarting the child. EOF on stdin keeps the current set.
4. **Event-driven, shared.**
   - One process, one runtime and one watcher set. Each repo gets the same event sources `work-only-when-something-happens` defines (worktree, `.clank/`, git state, its monitor's wakes), plus the one watch on the installed binary.
   - A repo's work runs only when one of its own events fires.
   - No per-repo or global poll.
5. **Failures stay per repo.**
   - A repo that can't be opened, or whose watcher fails, emits one line `{"repo": …, "error": "…"}` and is retried only when its path changes or it is re-added. The other repos carry on.
   - A watcher failure for the whole process (the binary watch, stdin) ends the process with an error.
6. **Upgrade.** The process re-execs into a newly installed build like any status watch, carrying its repo set: argv, plus the current stdin-added set passed through `--repos-from` a handoff file. Afterwards it re-emits every repo's snapshot once.
7. **Docs.** The README's status section documents the flags and line format, and recommends one multi-repo watch for dashboards. Change `clank status --help` and regenerate the book reference page.

## After it lands

Once the PR is merged and the build installed, message clankstack's master (`clank send --repo /Users/llfourn/src/clankstack --master -m "…"`). Say that the feature exists, give the flags and the line format, link the README section, and say that switching its 88 per-repo children to one is now possible. LLFourn, 2026-11-11: "you should message clank stack when this work is done."

## Out of scope

- Changing clankstack's code. It is another repo; its own agents switch it to one child.
- Non-JSON multi-repo output.

## Tests

No monitor or real binary is started; the watchers are behind their existing seam.

- **Single repo unchanged:** the one-repo output is byte-identical to today's (fixture).
- **Many repos:** an event in repo A emits only A's line, and B and C stay quiet. Each line carries its `repo`.
- **Add and remove:** `+C` starts watching C and emits its snapshot; `-B` stops B's watchers and emits nothing more for it.
- **A broken repo:** a nonexistent path emits one error line and the others carry on. Removing it and adding it back once it exists starts it normally.
- **Upgrade handoff:** the repo set, including stdin-added repos, survives the re-exec (handoff fixture), and every repo is re-emitted once.
- **Idle:** with no events, no repo's snapshot is computed (counted through a seam).
- **Mutation:** emitting every repo on any repo's event fails the many-repos test. Dropping stdin-added repos at re-exec fails the handoff test.

## Done when

One `clank status --watch -j` process can serve a dashboard of any number of repos, doing work only for the repo where something happened, and it survives installs with its repo set intact.

## As landed

- One `clank status --watch -j` process watches any number of repos: repeated `--repo`, `--repos-from FILE`, or `--repos-from -` with live `+PATH`, `-PATH` or bare `PATH` lines. Each line is that repo's snapshot plus `repo` (absolute, symlinks resolved), written only when that repo changes. The format is fixed at start, so it stays multi-repo as the set shrinks to one or none.
- With one repo and no new flag, output is byte-identical to before (fixtures `status-watch-one-repo.{jsonl,txt}`). Wakes now gather over a fixed 200ms window from the first one, not a sliding one.
- Work is per repo: each has its own `GitStatus` cache and watchers, every wake carries the repo's generation so a removed or re-added repo's leftover wakes drop, paths are normalised and deduplicated, and a repo's watchers are committed before its first snapshot. No poll.
- One FSEvents stream per process covers every worktree, restarted once per membership batch; after a restart every repo is invalidated and looked at again, since changes during the gap are lost. Events reach every repo whose roots contain the path, and that repo's `WakeFilter` decides. FSEvents is still built only in `status.rs`.
- Shared external Git config (global, XDG, a shared excludes file) is watched once per process (`ElsewhereHub`) and invalidates every repo that reads it.
- Failures stay per repo: a repo that can't be opened, faults, or fails before its first snapshot is one `{"error","repo"}` line, recovered through a kernel watch on its path. A malformed list line is an error line. Only a stdin or stream failure ends the process.
- Upgrade: the watch re-execs with a hidden `--handoff <file>` (fixture `status-watch-handoff.json`) carrying the set as it stands, argv removals included, and stdin's state including a half-read line. The new build keeps reading the same stdin and writes every repo's line once. `--handoff` replaced the plan's `--repos-from` handoff, because argv cannot carry removals or a half-read line.
- Cost: 88 repos reach first output in about 12s, at about 14 fds and 2 idle threads per repo with no monitor (about 3 fds with one). Multi-repo mode raises `RLIMIT_NOFILE`.
- Also fixed: the zellij runtime-request test asserts the request is queued when the wake fires, and is off the known-flaky list.
- Left open: a linked worktree also walks when the main checkout's index changes, because its filter counts the shared git dir as its input (one extra walk, correct result). Non-UTF-8 repo paths are refused.
