APPROVE

This is only the release version bump from `0.5.0` to `0.6.0` in `Cargo.toml` and `Cargo.lock` after the approved implementation. No issues found.

I did not rerun the full workspace tests for this metadata-only commit. The previous implementation commit passed:

```sh
cargo test --workspace --exclude trinity-frontend
```
