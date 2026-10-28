# ci-runs-on-linux
# CI runs on Linux

## Problem

There is no `.github/workflows/`. Reviewers run the test gates by hand on macOS,
the only platform clank is developed on. Nobody has ever built clank on Linux or
run its tests there, so we don't know whether the code works there at all. There
are 26 `cfg(target_os …)` sites, plus process-group, `/proc` versus `ps`, and
path assumptions. A Linux CI job is the cheapest way to find out, and from then
on it keeps Linux working. This is `ci-gates-every-push` from
`RELEASE-CHECKLIST.md`, scoped to Linux.

## Change

1. **`.github/workflows/ci.yml`**, triggered on push and on pull requests, with a
   single Linux job (`ubuntu-latest`, x86_64). It provisions git, Node (the
   OpenCode plugin harness) and a C compiler (`rquickjs`). It runs:
   - `cargo fmt --all --check`
   - `cargo clippy --workspace --all-targets -- -D warnings`. If clippy doesn't
     pass today, fix the warnings in this plan; don't drop the step.
   - `cargo test -p clank --lib`
   - `cargo test -p clank --test it`
   - the tests of the other workspace crates

   The test steps run with no agent-session variables (`CLANK_AGENT`,
   `CLAUDE_CODE_SESSION_ID`, `CODEX_THREAD_ID` and the rest of
   `agent_env::SESSION_VARS`). CI never sets `CLANK_SKIP_NODE_HARNESS`.
   `#[ignore]` tests stay ignored, and the job summary lists them as not run.
   Cache the cargo registry and `target/` so that a run costs minutes.
2. **Pin the toolchain** in `rust-toolchain.toml`, and declare `rust-version` in
   the manifests. CI uses the pinned toolchain.
3. **Make the Linux run pass.** Every failure that only shows up on Linux is fixed
   at its cause:
   - Real platform differences get a cfg-gated implementation, with a test on
     each side.
   - Tests that assume macOS (paths like `/private/tmp`, `ps` output, Gatekeeper,
     the case-insensitive filesystem) are fixed so they hold on both platforms.
   - Never skip a test on Linux to get green. If a test genuinely cannot run on
     Linux, it becomes an explicit `#[cfg]` with a WHY comment, and the plan's
     final commit lists each one.
4. **Keep macOS working.** Everything still passes locally on macOS. This plan
   adds no macOS CI job (macOS runners cost about 10x as much on GitHub).

## How it is verified

There is no Linux machine or container here. The workflow can only be exercised
on GitHub, which means pushing to `origin` (`git@github.com:LLFourn/clank.git`).

- **Pushing needs Lloyd's explicit go-ahead.** It publishes the branch. Raise a
  `clank block ask` before the first push, and say which branch.
- After that, iterate on the CI branch. Read failures with `gh run view --log`.
- Before the first push, catch what can be caught locally:
  `cargo check --target x86_64-unknown-linux-gnu` (and `cargo clippy` for that
  target, if a cross C toolchain isn't needed for it) to find compile errors in
  cfg-gated code.

## Out of scope

- macOS, Windows or arm64 CI jobs.
- Release builds, binaries, tags.
- Book and screencast scripts (`scripts/book/test_record.py`,
  `scripts/check-timeline-ui.cjs`); record that they're excluded in the
  checklist item.

## Acceptance

- A green CI run on Linux for the plan's final commit, linked in the finish
  message.
- `RELEASE-CHECKLIST.md`: `ci-gates-every-push` is marked done for Linux, citing
  the run. The macOS and other-platform parts stay open.
- No test was disabled to get green without being listed and justified.
- No WHAT comments.
