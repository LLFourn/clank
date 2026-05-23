# drop-stop-hook-state

## Summary

Remove the per-(label, session) state file and progress guard
machinery from the Stop hook. Replace with the simplest possible
approach: delete the `stop_hook_active` check entirely so the hook
fires on every turn-end regardless of chain position.

Claude's 8-block cap (which resets on tool use between fires) is
a sufficient safety net for claude. Codex has no native cap — we
accept that risk as theoretical (an agent receiving a continuation
prompt and producing zero output is contrived) and can reintroduce
a guard if it becomes a real problem.

## What goes

- `crates/cli/src/stop_hook_state.rs` — entire module deleted.
- `crates/cli/src/lib.rs` — remove `pub mod stop_hook_state`.
- `crates/cli/src/cli/stop_hook.rs` — remove `progress_guard`
  function, `ProgressGuard` enum, state imports, and the
  `progress_guard(...)` call site inside the hint/wait dispatch
  arm. The auto_mode match goes back to a flat three-arm match
  (Off → Silent, Hint → compute_hint, Wait → compute_wait).
- `crates/core/src/hook_io.rs` — revert the `stop_hook_active`
  doc comment to something shorter that just says "chain marker;
  currently not acted on by the adapter."
- `crates/cli/tests/stop_hook_integration.rs` — remove all
  `progress_guard_*` tests. Remove
  `repo_with_reviewable_work_for_alice` helper if only used by
  those. Remove `claude_stdin_with_progress` helper if unused
  after cleanup; revert `claude_stdin` to its pre-progress-guard
  shape (no `last_assistant_message` field). Restore the codex
  inline stdin to its pre-progress-guard shape.

## What stays

- The auto_mode dispatch (Off/Hint/Wait) — unchanged.
- hint/wait outcome compute functions — unchanged.
- All non-progress-guard tests — unchanged.

## Tests

No new tests. The existing hint/wait continuation tests already
verify that the hook issues continuations correctly. The removed
`stop_hook_active_short_circuits_silent` test was encoding the
old bug; it should NOT be restored.

Verify that `stop_hook_active=true` + hint mode with work pending
now fires a continuation (this is the whole point — the old guard
would have returned Silent).

## Acceptance criteria

- `stop_hook_state.rs` does not exist.
- No reference to `progress_guard`, `StopHookState`,
  `hash_progress`, `read_state`, `write_state` anywhere in the
  codebase.
- `stop_hook_active=true` does not cause Silent — the hook fires
  normally regardless.
- All remaining tests pass.
