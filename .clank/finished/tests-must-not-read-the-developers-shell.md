# tests-must-not-read-the-developers-shell
# Tests must not read the developer's shell

## Why

Four `stop_hook` tests fail on this machine and pass in a clean shell:

```
$ cargo test -p clank --lib cli::stop_hook::tests::auto_off
FAILED — "hook: cannot resolve `claude`'s role (this repo has no master
agent...)" where the test expects Silent { why: AutoOff }

$ env -u CLANK_AGENT cargo test -p clank --lib cli::stop_hook::tests::auto_off
ok. 3 passed
```

The tests bind a temp repo's session to `codex` and assert the outcome.
`CLANK_AGENT` is the highest-precedence identity override
(`agent_env.rs:255`), so a developer who exports it — which every agent
running inside clank does — silently retargets the code under test at a
label the fixture never created. The tests were not wrong about clank;
they were reading the room.

Found while working on `the-agents-name-is-said-once`: a full `--lib`
run showed 4 red that had nothing to do with the diff, which costs a
diagnosis every time and trains the reflex to ignore red.

## The model

**A test's inputs are the fixture's, and the fixture's only.** The
process environment is an input like any other, and `agent_env.rs`
reads five variables from it. Any test that exercises identity
resolution inherits whatever the developer's shell happens to hold —
`CLANK_AGENT` here, but `CLANK_SESSION` and the rest are the same
hazard.

There are two shapes of fix and they are not equivalent:

1. Clear the variables inside each affected test. Rust runs tests in
   threads of one process, so `std::env::set_var` in one test races
   every other — this is the wrong shape, and it is why the problem
   should not be solved test by test.
2. Make the reader take its environment as an argument, so a test
   passes an explicit one and the binary passes the real one. The
   process environment stops being an ambient input to anything below
   `main`.

Two is the real fix. The question the plan must answer is how far the
seam has to reach: `agent_env.rs` alone, or every caller that builds
`IdentityInputs`.

## Deliverables

1. **An explicit environment under the three readers.** `agent_env.rs`
   makes six `env::var` reads and already owns the list
   (`SESSION_IDENTITY_VARS`). Give it the shape `dns.rs` uses for the
   same problem — a small trait, a process-backed impl, and `_in`
   variants of `resolve_identity_from_env`, `detect_session_from_env`
   and `explicit_label_from_env` that take one. The ~20 command entry
   points keep calling the process-backed versions unchanged; the seam
   exists for what tests drive.
2. **Thread it as far as the tests reach, and no further.** The four
   red tests go through `stop_hook`, so `compute_outcome_with`,
   `orphaned_by_its_pane` and `session_started` take an environment and
   production passes `Process` at the hook's edge. The `#[cfg(test)]`
   `compute_outcome` wrapper states an EMPTY one, so every existing
   test becomes hermetic without being edited; a test that wants to
   state something uses `compute_outcome_in`.

3. **A gate**: `env::var` of an identity variable appears only in
   `agent_env.rs`, in the same style as the git boundary. Naming a
   variable stays legal — the launcher scrubs them by name — because
   reading is the violation, not knowing.

4. **One copy of the scan.** Four gates already carry identical
   `rs_files` / `crates_dir` (and one `strip_test_code`). The fifth
   does not get its own: they move to `tests/it/common`, which is what
   they were always for.

## Tests

- The four `auto_off` / hand-started-session tests pass with
  `CLANK_AGENT=claude` exported, which is how they fail today.
- A stated environment is READ, not merely tolerated: an override
  stated by a fixture still outranks the session binding, through the
  seam, and grok's detection finds its session on the stated disk and
  finds nothing without one (codex on 5607750 — falling back to the
  real `HOME` would put the developer's sessions back in).
- **The contamination matrix is a discovery step, not a committed
  test.** Exporting each of the six variables in turn across the whole
  `--lib` suite is how the scope was found — `CLANK_AGENT` fails four
  tests, the other five are clean today. Making it a test would mean
  spawning child processes per variable, which this repo bans for good
  reason; and it would test the shell rather than the code. What makes
  it durable is the gate plus fixtures that state their environment.
- Mutation-check: re-introducing an ambient read fails the gate.

## Out of scope

- The stop hook's behaviour. Nothing here is a bug in what clank does;
  only in what the tests can see.
