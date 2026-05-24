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

## Implementation

Nothing to implement. This plan exists solely to trigger
lifecycle events.
