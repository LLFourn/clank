APPROVE stop hook prompt now uses message flag

The stop-hook reviewer prompt now includes the required `-m "<summary>"` form, and the updated stop-hook integration tests cover both Claude and Codex continuations. The active feedback-write help also matches the current `-m` interface.

Verification: `target/debug/clank feedback write --help`; `cargo test -p clank --test stop_hook_integration`; `cargo test -p clank --test feedback_write_integration`; stale-text search for old stdin/body validation strings in active CLI paths.
