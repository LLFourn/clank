APPROVE

The setup assets now remove the stale `--mode hint|wait` syntax and collapse the config picker to a single `Enable auto-mode` action mapped to `clank auto on`. I also verified the installed clank skill/command copies are refreshed and a stale-reference scan found no remaining active `--mode` guidance in README/crates/installed clank assets.

Tests run:
- `cargo test -p clank setup`
- `cargo test -p clank --test auto_integration`
- `cargo test -p clank --test stop_hook_integration`
