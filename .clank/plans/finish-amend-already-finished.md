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

## Fix

When `--amend` is set, skip the `AlreadyFinished` short-circuit.
Concretely: route `--amend` through a path that does not call
`dispatch_readiness` for the AlreadyFinished case. Two reasonable
shapes —

1. In `run`, if `args.amend`, only treat `Blocked` as fatal;
   `Ready` and `AlreadyFinished` both fall through to the
   existing `head_is_finalize_for` check + `finalize(..., true, ...)`.
2. Change `dispatch_readiness` to take `amend: bool` and return
   `true` for `AlreadyFinished` when amending.

Prefer (1) — keeps `dispatch_readiness` a pure decision on the
preview.

## Tests

Add a test in `finish.rs` tests module:
- seed a repo, create a plan, finalize once, then call
  `finalize(..., amend=true, message=Some("new msg"))` and assert
  HEAD subject is the new message and `.clank/finished/<stem>.md`
  still exists.

(The CLI-level dispatch in `run` is harder to unit-test; the
`finalize` function path is what changes behaviourally.)

## Out of scope

- Re-running the readiness check / approvals re-verification on
  amend. Amend's job is to rewrite the existing finalize tree, not
  to re-validate it.
