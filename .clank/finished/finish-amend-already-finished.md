# finish-amend-already-finished

`clank finish --amend` short-circuits on `AlreadyFinished` before
the amend path is reached, so it cannot be used to refresh the
finalize commit (e.g. to fix a stale commit message or re-sign).

## Current behaviour

`crates/cli/src/cli/finish.rs::run` calls `dispatch_readiness`
which returns `false` for `FinalizeReadiness::AlreadyFinished`,
causing `run` to return early. The subsequent
`args.amend && !head_is_finalize_for(...)` guard and the
`finalize(..., amend=true, ...)` call are never reached when the
plan is already in `finished/`.

## Why `finalize()` cannot handle this directly

`finalize()` assumes the plan still lives in `.clank/plans/`: it
copies that file into `finished/`, falling back to writing an
*empty* finished marker if the plan file is missing, then runs
`git rm` on the (missing) plans path. Routing AlreadyFinished
through `finalize(..., amend=true, ...)` would clobber the real
finished marker with an empty file and fail on the `git rm`.

So the amend-on-already-finished case needs its own path that
does NOT touch the worktree files.

## Fix

In `run`, branch on `(readiness, args.amend)`:

- `Ready`, amend or not → existing `finalize(..., amend, msg)`.
- `Blocked` → existing error.
- `AlreadyFinished`, no amend → existing "already finished" no-op.
- `AlreadyFinished` + amend → new `amend_already_finished()` path:
  - require `head_is_finalize_for(stem)` (else bail as today);
  - run `git commit --amend -m <msg>` (where `msg` is
    `args.message` or the default `[<stem>] finish`);
  - no file ops, no `git add`/`git rm`.

`dispatch_readiness` stays a pure preview decision; the amend
case is handled by the caller, which already knows the amend
flag.

## Tests

In `finish.rs` tests:
- `amend_already_finished_rewrites_message`: seed repo, finalize
  once (HEAD = `[foo] finish`), call `amend_already_finished`
  with `Some("new msg")`, assert HEAD subject == `new msg` and
  `.clank/finished/foo.md` is unchanged (non-empty, same content).
- `amend_already_finished_rejects_when_head_is_not_finalize`:
  after the finalize, add an unrelated commit on top, then call
  the amend path; expect the same error as the existing
  `--amend requires HEAD to be a finalize commit` check.

## Out of scope

- Re-running the readiness check / approvals re-verification on
  amend. Amend's job is to rewrite the existing finalize tree, not
  to re-validate it.
