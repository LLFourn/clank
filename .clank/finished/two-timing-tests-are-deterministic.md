# two-timing-tests-are-deterministic
# Three timing tests stop flaking

## Why

Two tests fail intermittently, and each failure blocks a PR at random:

- `web::tests::supervisor::a_dead_subscription_is_restarted_with_backoff`
  (`cli/web/mod.rs` ~1594). It runs under `start_paused`, yet Linux CI
  saw 7 resubscriptions in 20 s, where the test allows 4 to 6 (PR #11,
  2026-11-05). It passes when run alone. Under the full suite's load,
  something in the supervisor appears to advance on real time, or on
  work outside the paused clock, so restarts outrun the doubling
  backoff.
- `wake_receipt_acceptance::shared_monitor_recurring_timer_watches_config_and_ack_then_rearms_native_deadline`
  (`it/wake_receipt_acceptance.rs` ~655). It failed 1 run in 4 locally.
  The timer is anchored 100 s in the past and repeats every 10 s, so a
  run that crosses a slot boundary appends a new slot before the
  "pending replay appends nothing" assertion.

- `cli::monitor::tests::a_monitor_that_hangs_up_mid_handshake_is_replaced`
  failed once in a full `-p clank --lib` run on 2026-11-05, and passed
  three times when run alone. Its cause is not yet diagnosed; this plan
  diagnoses it and makes it deterministic like the other two.

## What changes

Each test is made deterministic by diagnosing its race, then
coordinating its phases explicitly. Widening tolerances, adding real
sleeps, or relying on repeated success does not count as a fix.
Production code changes only where the diagnosis shows a production
defect, and the As-landed note says which case each test was.

### Supervisor backoff (`web/mod.rs` ~1594)

`pane_poll` already schedules on tokio's clock
(`tokio::time::Instant::now().into_std()`, ~780). The test helper
`pass()` (~1523) mixes paused sleeps with `spawn_blocking`, std sleeps,
and real child and reader completions, so under load the paused clock
can advance past several backoff deadlines while blocking work catches
up.

- **Diagnose**, treating the cause as a hypothesis until traced. Tokio
  1.53.1 already inhibits auto-advance while its own `spawn_blocking`
  tasks run (`time/clock.rs:127-134`, `runtime/blocking/schedule.rs`).
  Independent child and reader threads have a different lifecycle and
  are the more likely suspects, but the trace decides. A production clock
  change is made only if the trace shows a production defect.
- **Synchronize** the subscription lifecycle: each subscribe and each
  death is an observable event the test awaits before advancing the
  clock deliberately.
- **Assert the schedule, not a total.** With `S(t)` the time of each
  subscription, the restart gaps `S(k+1) - S(k)` must strictly increase
  and double. The tolerance is one pass granularity, and the first gap
  is about 1 s. A fixed delay of any length fails this, where the old
  "4..=6 in 20 s" bound accepted some fixed delays.

### Recurring timer (`it/wake_receipt_acceptance.rs` ~600-700)

The test has three guarantees in sequence:
1. offline slots coalesce into one pending event;
2. a pending replay appends nothing to the WAL;
3. after an ack, the next native deadline wakes a fresh hook within
   `LIMIT`.

The race is that a slot boundary can pass between (1) and (2), so the
monitor appends a new slot during the replay.

- **Controlled logical time for the replay.** The no-append replay
  assertion runs against a clock the test controls, never against wall
  time. The monitor's timer source takes an injected clock: a seam in
  production code, defaulting to the system clock. The test freezes it
  for phases 1 and 2, so no slot boundary can occur during the replay.
- **A genuine native deadline after the ack.** In phase 3 the test
  advances the injected clock to the next slot. The monitor's native
  deadline then fires through its real timer path, the same code that
  arms and fires it in production, only driven by the controlled clock.
  The fresh hook is woken by that deadline, not by a config edit or a
  producer wake.
- **Watchdogs bound only progress.** `LIMIT` stays a watchdog against a
  stall. It never asserts that healthy processing finishes before an
  incidental slot boundary.
- All three guarantees stay independently falsifiable (see Acceptance).

### Monitor handshake (`monitor.rs` ~11406)

The guarantee is that after a peer hangs up mid-handshake, the client
attaches as `Attached::Shared` with exactly one replacement start.

- **Diagnose** the race between the hang-up, the client noticing it, and
  the replacement monitor publishing its descriptor.
- **Coordinate** the fixture explicitly: the hang-up and the
  replacement's readiness are each signalled events the test sequences,
  not timeouts.
- **Keep** both assertions, `Attached::Shared` and exactly one
  replacement start. A larger allowance or a retry is not the fix.

## Acceptance

- **Supervisor:** the schedule assertion passes. Mutations each fail it:
  a fixed 1 s delay, a fixed 4 s delay, and a delay that grows linearly.
- **Timer:** three mutations, one per guarantee, each fail the test:
  - expanding offline slots instead of coalescing them fails (1);
  - re-appending on a pending replay fails (2);
  - not re-arming the native deadline after an ack fails (3) within
    `LIMIT`.
- **Handshake:** a mutation that starts a second replacement fails the
  exactly-one assertion. One that returns before attachment fails
  `Shared`.
- **Repeat evidence**, supplemental and reported with exact commands and
  counts: each of the three tests run 50 times while the rest of its
  suite runs concurrently, plus a full `-p clank --lib` and `--test it`
  pass.

## As landed

- **Supervisor (`web/mod.rs`), test only.**
  - The reader is a `std::thread`, which the paused clock doesn't wait
    for. A tick could therefore see a dying subscription as alive and
    reset the backoff to 1 s. Traced: an 8 ms death reproduced
    "7 subscriptions in 20 s".
  - The test now blocks until the reader finishes before it steps the
    clock, then steps it in `STEP` (250 ms) increments. It asserts that
    the first gap is about 1 s, within `STEP`, and that each later gap is
    strictly larger and within `STEP` of double the previous one. The
    observed trace was `[1s, 2s, 4s, 8s, 16s]`, and it stayed exact with
    a child that took 200 ms to die.
  - Production is unchanged.
- **Handshake (`monitor.rs`), a production fix.**
  - On Darwin, a hello write that races the peer's close can fail with
    `ENOTCONN`. `hung_up` now counts that as a hang-up.
  - The test runs three hang-up orderings through a connect seam, and
    `a_hello_that_finds_the_connection_gone_is_a_hang_up` covers the
    `ENOTCONN` case.
- **Timer.**
  - The extra WAL bytes were the first wake's presentation record,
    committed after the hook exited. They were not a slot boundary.
  - `timer_source::clock` is the system clock in production. A
    file-driven clock is used only under the `test-clock` feature, which
    reaches the test builds through a self dev-dependency.
  - 450d6e4c gives a due slot a single acceptance path, the deadline
    (`due_at` → `try_entry`). `refresh`, `arm` and `retry` only arm, and
    `wait`'s first pass calls `due()` once after `arm()`.
  - The test forces a watcher invalidation right after advancing the
    clock, so a wake that came from anywhere but the deadline cannot
    pass.
- **Mutations.** Each was applied by a `python3 -u` script that asserted
  its anchor occurred once, ran the covering test, and restored the file
  from memory with a byte compare; the diff hash was identical before and
  after.

  | Mutation | Result |
  |---|---|
  | S1: backoff fixed at 1 s | caught, gaps `[1,1,1,1,1]s` |
  | S2: backoff fixed at 4 s | caught |
  | S3: backoff grows linearly | caught, gaps `[1,2,3,4,5]s` |
  | H1: start a second replacement | caught by the exactly-one assertion |
  | H2: return before attachment | caught, `Alone` |
  | H3: drop `NotConnected` from `hung_up` | caught by `a_hello_that_finds_the_connection_gone_is_a_hang_up` |
  | T1: expand offline slots instead of coalescing | caught: "every slot … in one event" |
  | T2: re-append the presentation on a pending replay | caught: "pending replay appends nothing" |
  | T3: never re-arm after the ack | caught: "timer armed" is never logged within `LIMIT` |
  | T4: ignore `Wake::TimersDue` | caught at the first hook |
  | T4b: ignore every `TimersDue` after the first | caught **15 of 15** runs, with `wake_receipt_acceptance::` looping concurrently |
  | T2b: remove the `Pending` early return | survived |

  T2b is equivalent while the clock is frozen: nothing new is due, so
  nothing is appended.
- **Repeat runs.** The harness ran each test N times with its suite
  looping concurrently. Each run executed the test binary directly, under
  the env scrub, `--exact`, with `timeout 180`, and counted the runs
  that printed `test result: ok. 1 passed`. In shell:

  ```sh
  SCRUB="env -u CLANK_AGENT -u CLAUDE_CODE_SESSION_ID -u CODEX_THREAD_ID -u OPENCODE_SESSION_ID -u GROK_AGENT -u GROK_SESSION_ID -u CLANK_BOOTSTRAP_SESSION_ID"
  ( while [ ! -e stop ]; do timeout 600 $SCRUB "$BIN" "$SUITE" >/dev/null 2>&1; done ) &
  for i in $(seq 1 50); do
    timeout 180 $SCRUB "$BIN" "$TEST" --exact 2>&1 | grep -q "test result: ok. 1 passed" && pass=$((pass+1))
  done
  touch stop; wait
  ```

  `BIN` is the `clank` lib test binary or the `it` binary under
  `target/debug/deps/`.

  | Test | Concurrent `SUITE` filter | Passed |
  |---|---|---|
  | supervisor backoff | `cli::web::` | 50/50 |
  | supervisor backoff | the whole lib binary | 50/50 |
  | handshake | `cli::monitor::` | 50/50 |
  | handshake | the whole lib binary | 100/100; 99/100 before the `ENOTCONN` fix |
  | `ENOTCONN` classification | `cli::monitor::` | 50/50 |
  | recurring timer, after the single-acceptance change | `wake_receipt_acceptance::` | 50/50 |

- **Full suites** after 450d6e4c, both with the env scrub:
  - `cargo test --locked -p clank --lib`: 3211 passed, 0 failed, 4
    ignored;
  - `cargo test --locked -p clank --test it`: 378 passed, 0 failed, 2
    ignored.
- **Cost to watch.** The self dev-dependency may make test builds compile
  clank twice. A follow-up that measures and speeds up the suite is
  proposed.
