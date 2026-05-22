APPROVE

The implementation matches the revised direction: `.clank` remains native-watch based, git movement defaults to polling only under `CODEX_SANDBOX=seatbelt`, and callers can force either path with `--poll` / `--no-poll`. The polling decision is isolated behind a pure `resolve_poll` helper, so the environment-specific behavior does not leak through the watcher or fold code.

Validation:
- `cargo fmt --check`
- `cargo test -p clank resolve_poll`
- `cargo test -p clank --test wfw_integration`
- `cargo test -p clank`
- `cargo build -p clank`
- `/private/tmp/clank-watch-repro/repro-git-commit-timeout.sh`
- `/private/tmp/clank-watch-repro/repro-git-commit-staged.sh`

Both sandbox repro scripts now wake on `git commit -m` with latencies around 0.4s, which is the practical failure mode this plan needed to solve. I do not see a remaining blocker in this commit.
