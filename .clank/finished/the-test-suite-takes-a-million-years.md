# the-test-suite-takes-a-million-years

`cargo test` on this machine was killed after twenty-plus minutes,
still compiling. A plain `cargo build -p clank` had taken 1m26s moments
earlier; a targeted `cargo test -p clank --lib open_zellij` took 15s to
build and 0.1s to run. The suite is not slow because the tests are
slow. It is slow before a single test runs, and nobody has measured
where.

## What is known without measuring

- 1,696 tests across the workspace; 22 files under `crates/cli/tests/`,
  8,968 lines. Cargo compiles EACH of those files into its own test
  executable, and each one links the whole `clank` library into itself.
  That is 22 links of a large crate, plus the lib's own test executable,
  plus `core`'s — and linking is the step a loaded machine punishes
  hardest. (These tests call library functions in-process like any
  unit test; none spawns the `clank` binary — checked: no
  `CARGO_BIN_EXE`, no `assert_cmd`, no `Command::new("clank")`. The
  cost is purely how Cargo packages them.)
- 88 `Command::new("git")` sites in test code build fixture repos by
  subprocess; six integration files and five `src` files contain real
  `thread::sleep` / `time::sleep`. The lib already uses tokio's paused
  time (`test-util`) in places, so the pattern for not sleeping exists.
- Even `du -sh target` timed out at two minutes. The target directory
  may be a problem of its own (stale incremental caches, one
  fingerprint per binary, debuginfo for 24 binaries).
- The machine is permanently under load from reviewer builds in
  worktrees and a dozen `clank wait` pollers. The suite cannot fix
  that, but it can stop being the thing that makes it unbearable.
- "Broken": the run that was killed was not failing, it was building.
  Whether a test HANGS when it does run is unknown and part of what to
  measure.

## Measured (2026-09-04, this machine, reviewers active)

Warm rebuild after touching one lib file (`open_zellij.rs`),
`cargo test --no-run --timings`: **57 s wall, 690 CPU-seconds** over
360 units. The lib compiles in 10 s. The other 680 s is 25 units that
each compile a sliver of test source and then link the whole `clank`
lib: 22 integration binaries at 31–34 s apiece, the bin (33 s), the
bin's own test target (13 s), and the lib test (13 s). A fully warm
`--no-run` is 0 s: nothing here is a stale-fingerprint problem.

That is the build. It is not the million years. Running `--list` on
the freshly built binaries — no test executed, the harness only
prints names — took:

```
fork_integration --list   (17 MB)   49.4 s   0.00 s user  0.00 s sys
pick_integration --list   (17 MB)   62.1 s   0.01 s user  0.01 s sys
clank lib test  --list    (71 MB)   85.2 s   0.01 s user  0.01 s sys
same binary, second run              0.005–0.02 s
```

Zero CPU in the process, and `syspolicyd` at 53–62 % the whole time.
**macOS Gatekeeper assesses every freshly built executable on its
first launch**, and on this machine (load 13–18, four 7 GB python
jobs, the reviewers' builds) that is a minute per binary. Twenty-four
test binaries, each new after every lib change, is the twenty minutes
that got killed — and every targeted `cargo test -p clank --lib` this
session quietly paid the 85 s too; it was folded into what looked
like build time. Linking is real and comes second.

What that justifies, in order:
1. **Grant the terminal Developer Tools permission** (System Settings
   → Privacy & Security → Developer Tools → add Ghostty; for
   Terminal.app, `sudo spctl developer-mode enable-terminal`). It
   exempts everything the terminal spawns from the per-binary
   assessment — the documented remedy for "my freshly compiled binary
   takes ages to start the first time". A machine setting, so the
   user's to make; this plan documents it in the README and puts the
   measurement here so the reason survives. Measured after: the same
   `--list` on a fresh binary should be milliseconds.
2. One `tests/it` harness — 22 assessments and 22 links become 1
   each. Worth doing even with the permission granted: 690 → ~140
   CPU-s of linking per lib change, and no reliance on a per-machine
   setting to keep the suite runnable.
3. `main.rs` holds only the README tests, and that is the whole
   reason a 13 s bin test target (one more fresh binary to assess)
   exists: move `Cli` into the lib, the tests with it, and set
   `test = false` on the bin.
4. The three boundary gates and the JSON-literal gate read source
   files and never call the lib; as standalone binaries each is a
   full lib link and a fresh binary to assess. Inside the harness
   they cost nothing extra.
5. Debuginfo: the lib test binary is 71 MB, and both the link and the
   assessment scale with it. `debug = "line-tables-only"` for the
   test profile, measured after 1–4 against the new baseline.

## After (same day, machine busier still)

One harness, no bin test target, gates inside. Warm rebuild after
touching the same lib file: **4 units, 154 CPU-seconds** (was 25
units, 690). Wall 77 s against 57 s before — but the lib compile
alone went 10 s → 44 s between the two runs, so the machine was ~4×
busier; CPU-seconds is the like-for-like number: −78 %. Fresh
binaries for Gatekeeper to assess per lib change: 2 (lib test,
harness) instead of 24.

The harness: 246 tests, 34 s, one process, green on the first run —
the `serial()` lock that kept the wait tests apart within a binary
now keeps them apart across the former binaries too. `--list` of the
harness equals the source manifest, which equals the pre-move
manifest plus the one new gate fixture; the bin's 5 tests are the 5
in `cli::command`.

Full workspace run after: **388 s wall, all green** — 1210 lib, 246
harness, 251 core, 18 doc/misc — against a run killed at twenty-plus
minutes before any test executed.

The Developer Tools grant for the terminal did NOT reach this
session: a fresh binary's first launch was still 25 s with
`syspolicyd` at 40 %. The zellij server is re-parented to launchd
(this shell ← claude ← zellij, ppid 1), so nothing under it is in
the terminal's responsibility tree. Documented in the README: add
zellij itself, or restart the sessions from a covered terminal.

Item 5 (debuginfo) is off the list: the 71 MB lib test binary is
40 MB `__TEXT` and 32 MB `__LINKEDIT`, no DWARF segment — debuginfo
is already split out, and `line-tables-only` would change little.

## Deliverable

A short written finding in this plan — numbers, not impressions — and
the changes it justifies, in order of measured payoff. Measure first;
do not consolidate binaries or delete sleeps on a hunch.

1. **Where the wall-clock goes.** `cargo test --no-run --timings` for
   the compile/link breakdown per binary; then the run itself with
   per-test timing (`cargo nextest` if it is on the machine, else
   `--report-time`), on an otherwise idle machine if one can be had
   and on this one either way. Record cold and warm (`touch` one file
   in `open_zellij.rs`) numbers — warm is what a working turn pays.
2. **Cut what the numbers name.** Expected candidates, to be confirmed
   or dismissed by step 1:
   - One test executable instead of 22: a `tests/it/main.rs` that
     declares the existing files as modules. Same tests, same
     in-process calls, same fixtures — one link. This is the standard
     remedy for exactly this symptom and is likely the biggest single
     win; it also runs the tests in parallel inside one process instead
     of one executable after another.

     **Moving the files moves what the gates scan.** Several test
     files are enforcement gates that walk the source tree by path,
     and each has its own idea of where `tests/` is (codex on
     4b43be1). Audited now, so the consolidation cannot silently
     defang one:
     - `no_json_literal_config_writes.rs` — NON-recursive
       `read_dir(tests/)`, immediate `.rs` children only. Under
       `tests/it/` it would stay present and green while scanning
       nothing. Must become recursive, with a fixture that scans a
       synthesized nested tree and proves a nested violation is found.
     - `zellij_ownership_boundary.rs`, `zellij_cost_boundary.rs` —
       recursive `rs_files` over `crates/*/{src,tests}`; nested files
       are already in scope. The ownership gate's own "tests tree is
       in scope" self-check keys on `/tests/` in the path, which
       `tests/it/…` still satisfies.
     - `git_boundary.rs` — `src/` only by design; unaffected.
     - `agent.rs`'s `include_str!("agent.rs")` self-scans and the
       README tests use `Cli::command()`; both in `src/`, unaffected.
     Any gate not on this list that turns up during the move is
     handled the same way before the move lands.
   - Sleeps that wait on real time in tests, replaced by the paused
     clock or by the seam the code under test already offers.
   - Git fixture setup shared per binary or per module where tests
     only read the fixture, instead of a fresh `git init` + commits per
     test.
   - Dev-profile settings that trade debug fidelity for link time
     (`debug = "line-tables-only"` or `0` for the test profile, and
     `split-debuginfo`) — measured, because on macOS the answer is not
     obvious.
   - A `target` that has grown past usefulness: size it, and if
     stale-incremental is the cause, say what to clean and how often.
3. **A fast path that is documented.** Whatever the result, the
   working rule stays: a turn runs the tests for the module it touched
   and the gates that scan its files, never the world. Put the exact
   invocations in `CLAUDE.md` so every agent uses them. The full suite
   is for `clank finish` and CI, and after this plan it should be
   something a person is willing to wait for.

## Tests

- The suite's ENUMERATED tests are unchanged by any consolidation,
  compared canonically. Before the move, `cargo test -- --list`
  prints each integration binary under a `Running tests/<stem>.rs`
  header with bare `<name>: test` lines; after it, the one harness
  lists them as `<stem>::<name>: test`. So the comparison is the
  multiset of `<stem>::<name>` — built from header + name before,
  read directly after — and it must be identical, multiplicity
  included (codex on 42edc77). Lib and bin tests are unchanged in
  form and are compared as they are. An unchanged count would not
  notice a gate that still exists but scans nothing; a raw name diff
  would report every moved test as changed.
- Every scanning gate above still finds a planted violation after the
  move: for the JSON-literal gate, a nested fixture file; for the
  boundary gates, their existing self-checks.
- A recorded before/after for `cargo test --no-run` warm rebuild and
  for the full run, in this plan's finding.
- No test spawns the clank binary or leaves a process behind; the
  existing bans stay in force.

## Out of scope

- The machine's background load (reviewer worktrees, `clank wait`
  pollers) — separate problem, separate plan if it needs one.
- CI configuration.
