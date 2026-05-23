APPROVE

The stale `HookOutcome::Silent` documentation is fixed, the removed guard symbols no longer appear under `crates/cli/src`, `crates/cli/tests`, or `crates/core/src`, and the stop-hook integration suite passes.

Tests run:
- `cargo test -p clank --test stop_hook_integration`
