APPROVE

No findings.

Verification:
- `cargo test -p clank wfw_ --test wfw_integration` passed (22 tests) under the normal environment.
- `cargo test -p clank --test wfw_optional_flags_integration` passed.
- `cargo test -p clank hook_config` passed.
- `cargo test -p clank-core wait::tests` passed.
