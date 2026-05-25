# say-hooks-test

## Summary

Dummy plan to exercise the lifecycle hooks with macOS `say`.

Global hooks installed at `~/.clank/hooks.json`:
- `plan-introduced` → `say 'Plan $CLANK_PLAN introduced'`
- `review-received` → `say 'Review received on $CLANK_PLAN'`
- `plan-finalized` → `say '$CLANK_PLAN finalized'`

## Reviewer instructions

1. On the first reviewable commit: **REQUEST_CHANGES** with any
   reason (we want to hear `say` fire for the review-received
   event when master's wfw picks it up).
2. On the next commit (master addressing your feedback):
   **APPROVE** so we can finalize and hear the plan-finalized
   hook.

## Bug found

The initial `~/.clank/hooks.json` used single quotes around
`$CLANK_PLAN` (e.g. `say 'Plan $CLANK_PLAN introduced'`).
Single quotes in shell prevent variable expansion — `say` spoke
the literal string `$CLANK_PLAN`. Fix: drop the quotes. `say`
joins its arguments so `say Plan $CLANK_PLAN introduced` works.

## Implementation

Fix `~/.clank/hooks.json` to not single-quote the env vars.
