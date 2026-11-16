# the-purge-protections-return

Invariants: I13.

## Why

9bcaf3e0's test purge fixed real flakes but deleted the only tests of five behaviours:

1. Turning the remote off joins every async task (`num_alive_tasks`).
2. The release `run()` refuses a dirty or off-tag binary, end to end.
3. Production `runtime_dir` resolution.
4. `grok_setup::launch_ready` does not start the probe early.
5. `tui_is_the_command`: source and README name no retired command.

Two isolation problems also remain:

- `cargo test --test it` uses the live `/tmp/clank-run-<uid>` unless run through `scripts/test.sh`.
- `/tmp/clank-ut-*` directories leak from a static `OnceLock` `TempDir` that is never dropped.

## Build

1. Restore each protection as a library-level test. Never spawn the binary. Each must fail on the mutation that breaks its behaviour; record the mutation in the commit message.
2. The `it` harness sets its own runtime dir, so it never touches the live one however it is invoked.
3. The shared unit-test directory is removed at process exit, or lives under `target/`.
4. List each restored test under the invariant it protects in `docs/invariants.md`, where one applies. The remote-off join, the release check and runtime-dir resolution may protect no listed invariant. Do not invent one; say so in the commit.
5. Before restoring each test, read what 9bcaf3e0 deleted (`git show 9bcaf3e0 -- <file>`). Write the replacement at the library core, never spawning the binary. Where the original was flaky, the replacement must not inherit the cause; the inherited-fd lesson from `the-serve-loop-survives-a-held-event-log` applies to any test that holds a lock.

## Acceptance

- Five restored tests, each mutation-checked.
- `cargo test --test it` without the script touches nothing under the live runtime dir.
- No `/tmp/clank-ut-*` survives a test run.
