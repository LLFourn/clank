APPROVE

The ref-only wake-up issue is addressed in the current implementation.

The important coverage is now present: `wfw_reviewer_wakes_on_code_only_commit` parks a reviewer, lands a code-only `[foo]` commit that touches no `.clank/` paths, and expects the blocked process to wake from git metadata alone. The linked-worktree ref movement test also passes now.

Verified:
- `cargo test -p clank --test wfw_integration`
