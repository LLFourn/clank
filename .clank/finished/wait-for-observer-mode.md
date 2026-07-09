# wait-for-observer-mode
# wait --for: observe a foreign repo

`clank wait --for <event> [--repo <path>]` — block until something
happens in a repo, as a pure OBSERVER. This is how an agent in one repo
waits on progress in another (e.g. a coordinating agent watching a fork
until its plan lands). MVP events:

- `--for commit` — HEAD moved (any new commit).
- `--for finished` — a plan was finalized (the finished-plan set grew).
- `--for stopped` — the repo came to a stop-point: a plan finalized OR a
  new unanswered block appeared (someone needs the human). Fires on
  whichever lands first.

## Semantics

- **Observer, not participant**: with `--for`, author/role/session
  binding are never resolved and are ignored if passed (documented in
  `--help`). The foreign repo has no binding for the caller — that must
  not matter.
- **Delta from now**: a baseline (HEAD sha, finished-plan set,
  unanswered-block set) is captured synchronously at startup, before
  entering the watcher loop; the wait fires only on changes AFTER that
  point. A plan that finished last week never fires `--for finished`.
- `--timeout`/`--json`/`--poll` behave exactly as in normal wait
  (timeout still exits 2). `--peek` conflicts with `--for` (a
  delta-from-baseline probe has no baseline); clap-level conflict.
- Fire output mirrors the items envelope, one item:
  `for_commit {sha, subject}`, `for_finished {plan, sha}`,
  `for_blocked {agent, name}` — plus the matching one-line human
  rendering. `--for stopped` emits whichever of the latter two fired.
- No lifecycle side effects in observer mode (like `--peek`, it must
  not fire the idle hook or any notices).

## Sketch

- `WaitArgs` gains `#[arg(long, value_enum)] r#for: Option<WaitFor>`
  (`commit|finished|stopped`), conflicting with `--peek`.
- `run()` branches early: observer path skips identity/role resolution
  and work-item projection; captures the baseline from the same
  fold/state the loop already builds; reuses the SAME shared watcher
  (`repo_watch` — finished/ and blocks are wake dirs, HEAD moves wake
  via the gitdir watch; no watcher changes) and the same
  refold-per-wake loop, diffing against the baseline each pass.
- Finished detection comes from the fold's finished-plan set;
  unanswered blocks from the same block model status/wait already use;
  commit from HEAD sha comparison.

## Known limitation (accepted for MVP)

If the observer wait dies and is re-armed, the baseline resets — an
event inside the gap is missed. Callers who cannot tolerate that can
record state themselves before re-arming. An absolute anchor
(`--since <sha>`) can come later if this bites.

## Out of scope

- Multi-event combinators beyond `stopped`, plan-name filters,
  cross-repo work DELIVERY (this observes; it never assigns work).
- Stop-hook changes: an armed observer wait already counts as "a clank
  wait" for `is_clank_wait`/YieldArmed, which is the desired behavior
  (the agent IS watching something; its exit wakes them).

## Acceptance

- From an unrelated cwd with no session binding:
  `clank wait --repo <foreign> --for commit` blocks, then fires with
  the new sha after a commit lands in the foreign repo; exit 0.
- `--for finished` does NOT fire for pre-existing finished plans; fires
  when a plan finalizes after the wait started.
- `--for stopped` fires on a new block; also fires on a finalize.
- `--for` + `--peek` is a clap error; `--author`/`--role` with `--for`
  are accepted and ignored.
- `--timeout` in observer mode still exits 2 with the usual message.
- Tests are in-process (no binary spawning): fixture repo + the
  observer future raced against repo mutations, plus the timeout path.
- fmt/clippy at baseline; existing wait behavior untouched when `--for`
  is absent.
