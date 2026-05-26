# test-home-isolation-audit

## Summary

Find and fix integration tests that inherit the developer's real
`HOME` and therefore read `~/.clank/config.json` or user hook
configuration during test runs. The motivating symptom is tests
triggering a local audible hook such as `review foo`.

## Problem

Most command-spawning tests now use scoped temp homes, but some
helpers still call `Command::new(clank_bin())` without setting
`HOME`. Any command path that loads user config can then see the
operator's real `~/.clank` state and run local hooks.

Initial suspects from a quick scan:

- `crates/cli/tests/stop_hook_integration.rs`: `run_clank` and
  `run_stop_hook` clear session env, but do not set `HOME`.
- `crates/cli/tests/feedback_read_integration.rs`: direct
  `Command::new(clank_bin())` calls do not set `HOME`.
- `crates/cli/tests/feedback_write_integration.rs`:
  `run_feedback_write` does not set `HOME`.
- Re-scan the remaining `crates/cli/tests/*_integration.rs`
  files for the same pattern; do not assume this list is complete.

## Implementation

- Add a small scoped test environment/helper pattern for integration
  tests that spawn the `clank` binary. Each spawned `clank` process
  should set `HOME` to a temp dir unless the test intentionally
  exercises user-level config discovery.
- Keep clearing `CLANK_AGENT`, `CLAUDE_CODE_SESSION_ID`, and
  `CODEX_THREAD_ID` in helpers unless a test explicitly sets them.
- Convert unscoped `Command::new(clank_bin())` usages in integration
  tests to the scoped helper.
- For tests that intentionally cover user config, make the temp
  home explicit and write the needed `~/.clank/config.json` there.
- Prefer a shared helper only if it stays simple across integration
  test crates; otherwise per-file `TestEnv` helpers are fine.

## Tests

- Run `cargo test`.
- Add a regression that seeds a temp home with an audible-looking
  or sentinel-writing hook command and verifies representative
  tests/commands do not read the real user home.
- Add or document a lightweight audit command such as:

  ```
  rg -n 'Command::new\(clank_bin\(\)\)' crates/cli/tests
  ```

  Every hit should either set `HOME` before spawning or be covered
  by an explicit comment explaining why inheriting user `HOME` is
  intentional.

## Acceptance

- Running the test suite must not invoke hooks from the developer's
  real `~/.clank/config.json`.
- No integration test relies on the real user home unless that is
  the behavior under test and the reason is documented inline.
