# the-suite-does-not-tell-the-truth-about-itself
# The suite does not tell the truth about itself

## Why

`cargo test -p clank --test it` is 316 passed, 2 FAILED, and a third test in
`--lib` fails under load. Neither integration failure is a regression from any
one plan: both already failed before `runtime-state-does-not-belong-in-the-repo`
touched anything, verified by running them against that plan's intro commit with
the identity variables cleared and its work swapped out of the tree.

The three are not the same defect, and an earlier draft of this paragraph
flattened them into "two gates that went red and stayed red, which means
whatever each was built to prevent is now unobserved". That is wrong about all
three. What they share is narrower and worse: **each one reports something other
than whether the code is correct.**

- One has stopped being a DISCRIMINATOR. It is red whether the property it
  guards holds or not, so its red carries no information — and the property does
  currently hold, so there is no panel bug to fix.
- One fails BEFORE reaching its own assertion, so the thing it is named for has
  never been checked.
- One fails for a reason that is not about the code at all, which is worse than
  red: the next person reads it as their own regression.

(No sha is cited for that baseline on purpose: `finish.autosquash` collapses a
plan's internal commits, so any of them would be unresolvable by the time anyone
reads this.)

Found while verifying `runtime-state-does-not-belong-in-the-repo`, which needed
a clean baseline to tell its own regressions from the background.

## The three

### `panel_probes_wakers::the_status_loop_probes_the_waker_fingerprint_on_its_own_turns`

ANSWERED while promoting this plan, and the answer is better than either option
the first draft offered. There is no live bug, and the matcher did not drift.

The gate scans `status_tui/mod.rs` for `waker_changed(` with `waker_probe`,
`crate::cli::status::waker_fingerprint(&repo)` and `refresh_pending = true`
within 400 characters after it. `waker_changed` and `waker_fingerprint` do not
exist in production at all any more — they appear ONLY inside the gate's own
source. They were renamed to `attendance_changed` and `attendance_fingerprint`
in `80f01612 [investigate-wait-generation-removal]`, and the behaviour came with
them intact: `status_tui/mod.rs:4462` still asks on the loop's own turns, with
the real fingerprint, and still sets `refresh_pending = true` when it flips. The
variable is even still called `waker_probe`, which is the only reason the gate
matches two of its four clauses.

So the panel is fine and has been. What is NOT fine is that the gate stopped
being able to discriminate, and the earlier draft of this paragraph described
that backwards. It said the rename made the gate silently useless and that it
"reports success for years and red only once". The opposite happened: it went
RED at the rename and has been red ever since. Nothing was silent. What failed
is that a red result was TOLERATED, and a test whose failure is routine can no
longer tell anyone anything — it reports the same red whether the call is there
or not, so it has stopped being a discriminator while still looking like a
guard (codex on fbb84e3).

The missing coverage is real either way, and I measured it rather than inferring
it: with the `attendance_changed` call DELETED, the library suite is 1929
passed / 0 failed, identical to baseline, and the integration harness shows the
same two pre-existing failures and no third.

Stated exactly, because the obvious phrasing overstates it: deleting the call
adds NO failure — the suite's verdict is byte-for-byte what it was. It is not
that a healthy green suite stays green, since this suite is not green; it is
that the only test which should have objected was already failing for an
unrelated reason, so its red says nothing either way. The gate's own doc
predicted the consequence — "deleting that one call therefore restores the whole
bug and passes the entire suite" — and the consequence has arrived by a route
that doc did not anticipate.

**The architectural point, which is the whole reason this is worth a plan.** The
helper is well covered — `attendance_changed` has six assertions at
`mod.rs:6581`. Its CALL SITE has none, and the gate exists only because, in its
own words, "the loop owns a terminal and cannot be driven from a test". A
source-scanning gate is not a test; it is a workaround for an untestable loop,
and a rename turns it from a guard into a standing failure. Matching on symbol
names within a character window makes the verdict depend on nobody renaming
anything and on rustfmt's line-breaking — so the gate goes red for reasons that
have nothing to do with the property, and once red is routine it cannot report
the one case it exists for.

**The fallback the first draft offered does not work, and the repo already said
so.** It proposed making the gate name the symbols through the COMPILER — "a
test that calls `attendance_changed` by path will not compile after a rename".
There already IS such a test, `the_panel_probes_for_a_completed_attendance_
between_filesystem_events` at `mod.rs:6570`, and deleting the production call
leaves it green; that is included in the 1929 above. Its own doc comment, two
lines up, states the reason: "A test that calls `input_signature` itself proves
the signature CAN see it and says nothing about whether the loop ever asks —
which is how the reported failure survived its own regression." Compilation
protects a reference. It cannot show that an idle turn REACHES the helper, uses
the real fingerprint, or acts on the answer. That paragraph is deleted, not
weakened.

**What to build instead, bounded (codex on fbb84e3).** Give the continuing
main-panel turn a common COMPLETION operation, called after event/timeout
processing and before the trailing refresh flush. It owns the attendance check
and the decision to raise or retain `refresh_pending`, with the current time and
the fingerprint observation supplied as inputs or narrow injected dependencies.
The terminal stays an outer adapter. This is a seam around an existing
operation, not a UI framework, and it does not require making unrelated screens
testable.

Then test THAT production operation — not an extracted helper beside it, which
would recreate exactly the problem being fixed — driving it with a controlled
event and inspecting its effects:

- an idle timeout with no filesystem refresh and a DUE fingerprint flip requests
  a refresh;
- unchanged attendance adds no request;
- a request already pending survives;
- a not-yet-due probe performs ZERO observations;
- and a key/spinner turn also probes, so the check cannot quietly become
  conditional on `batch.refresh`.

Preserve the existing rebuild throttle and input-signature gate. Mutation
acceptance starts from GREEN and must fail on each of: deleting the attendance
call, ignoring its result, and moving the check under filesystem-refresh
handling.

A source scanner may remain for outer-loop wiring, but only alongside a real
structural assertion about the invoked call site — compiler references may anchor
its names, never substitute for the assertion.

### `wait_event_sources::peek_starts_no_command_source`

This one looks stale rather than broken. It does
`clank::cli::wait::run(args).await.expect("peek returns")` and fails on
`WaitNoWork` — "wait --peek found no work for this agent" — which is a
deliberate error type, so `--peek` returning `Err` with no work is the contract
and the `expect` encodes an older one.

The test's REAL assertion is the line after it: `--peek` must spawn no source,
checked by a marker file the command would have touched. That assertion never
runs.

**The first draft's fix was an assertion a degenerate case satisfies.** It said
to let "the peek's verdict be either", which in practice becomes
`let _ = run(..).await` — and then a broken fixture, an invalid identity, a
failed fold or any unrelated early error also produces no marker and the test
passes without ever reaching the peek branch (codex on fbb84e3). "Either" is
not a contract.

State and test the INTENDED outcome. The idle fixture deliberately holds no
work, so `items` returns `Ok(Some(empty))` before source derivation, monitor
attachment or watcher setup, and `run` maps that to `WaitNoWork`. Assert that
specific outcome — the typed error at unit level, or `Some(empty)` through the
public `items` API in an integration test — without making a private error type
public solely for a test. Keep whatever public-exit coverage already exists.

**And the marker cannot carry the second half.** Absence of the file does not
distinguish "never spawned" from "spawned and torn down before `touch` ran", so
count starts at the real source-start boundary with a spy. Then demonstrate the
forced-start mutation actually fails; if immediate teardown masks it, that is
the reason for the counter, not a reason for a longer sleep.

### `monitor::tests::an_older_holder_goes_and_the_client_shares_its_replacement`

Load-flaky, not red: it passed five of five in isolation and three of four full
`-p clank --lib` runs, failing the one that coincided with two heavy concurrent
cargo jobs. It came in with `f24b59d2` and its windows are its own — budget
800ms, settle 400ms, the holder standing down at 600ms, leaving about 600ms for
the client to take the lease, bind, publish and complete a handshake while 1,900
other tests run across every core.

A test that fails once in four runs is worse than a red one, because the next
person reads the failure as their own regression and spends an hour proving it is
not. That is not hypothetical — it cost exactly that here.

The fix is not simply wider windows: a margin chosen to survive contention tells
you nothing about the mechanism, and the same edit would hide a real slowdown.
Make the handoff clock injectable, or drive the transition by an event the test
controls rather than by elapsed wall time, so the ordering is asserted and the
duration is not. The existing doc comment on it already derives the windows
arithmetically, which is the right instinct pointed at the wrong quantity.

**One clock domain, or the control is an illusion (codex on fbb84e3).** The
decision deadlines here use `std::time::Instant` while the retry waits use
`tokio::time::sleep`, and the socket helpers carry their own real I/O deadlines.
Pausing Tokio time therefore controls only part of the test. Inject the decision
clock and wait CONSISTENTLY, or coordinate the ownership transition through an
event the test raises; keep the real I/O bounded and keep fixture threads and
tasks owned and stopped.

Specify the sequence rather than a duration: a validated newer greeting; old
ownership held beyond the former retry-count cutoff; an actual lease release;
exactly ONE replacement start; publication; successful compatible attachment.
The busy and expired paths must still start zero children, the final pre-start
deadline guard must remain effective, and the regression this test was written
for — a release that arrives LATE — must survive as logical ordering rather than
disappear along with the 600ms sleep.

Not acceptable: widening production deadlines, disabling them under test, or
offering repeated heavy-load reruns as proof. One bounded load run may confirm
the deterministic assertions still compose with the full suite.

## Acceptance

- `--test it` and `-p clank --lib` are both green with all seven identity
  variables cleared.
- **The panel's probe is asserted on the PRODUCTION turn**, through the common
  completion path with a controlled event, never on a helper beside it. Driven
  for: a due flip on an idle timeout requests a refresh; unchanged attendance
  adds no request; an already-pending request survives; a not-yet-due probe
  makes zero observations; and a key/spinner turn probes too, so the check
  cannot become conditional on `batch.refresh`. The rebuild throttle and the
  input-signature gate still hold.
- Mutation acceptance for the panel starts from GREEN and each of these turns a
  test red: deleting the attendance call, ignoring its result, and moving the
  check under filesystem-refresh handling. A compiler reference to
  `attendance_changed` does NOT count — one already exists and survives the
  deletion.
- No test asserts the old `waker_*` names, which no longer exist. If a source
  scanner remains for outer-loop wiring, it carries a real structural assertion
  about the invoked call site and not merely a typed reference.
- **The peek test asserts the INTENDED outcome**, not "either": the no-work
  result specifically, as the typed error at unit level or `Some(empty)` through
  the public `items` API. No unrelated setup error can turn it green, and no
  private type is made public for the test's sake.
- Zero source starts is counted at the real source-start boundary with a spy,
  not inferred from a missing marker, which cannot tell "never spawned" from
  "spawned and torn down early". The forced-start mutation is shown to fail.
- **The handoff test is deterministic in ONE clock domain.** The decision clock
  and wait are injected consistently, or the ownership transition is driven by an
  event the test raises; real I/O stays bounded and fixture threads and tasks are
  owned and stopped. Asserted as a sequence — validated newer greeting, ownership
  held past the former retry cutoff, actual release, exactly one replacement
  start, publication, compatible attachment — with the late-release regression
  preserved as ordering rather than as a 600ms sleep. Busy and expired paths
  still start zero children and the pre-start deadline guard still bites.
- Production deadlines are not widened and not disabled under test, and repeated
  heavy-load reruns are not offered as proof. One bounded load run may confirm
  the deterministic assertions compose with the full suite.

## Watch out

- `--test it` inherits this session's agent identity and fails a dozen ownership
  checks for that reason alone, so clear all SEVEN of
  `agent_env::SESSION_IDENTITY_VARS` or you will read fourteen failures as a
  regression when only two are real:

  `CLAUDE_CODE_SESSION_ID`, `CODEX_THREAD_ID`, `GROK_AGENT`, `GROK_SESSION_ID`,
  `OPENCODE_SESSION_ID`, `CLANK_AGENT`, `CLANK_BOOTSTRAP_SESSION_ID`.

  An earlier draft listed six and omitted `GROK_AGENT` (codex on fbb84e3) — it
  is `ENV_GROK_MARKER`, so its NAME does not contain "SESSION" and reading the
  list by eye skips it. Derive the list from the constant, not from the shape of
  the names. The measurements already in this plan are unaffected because
  `GROK_AGENT` happened to be unset in this environment, which is luck and not a
  reason to keep the recipe wrong.
- The `error: could not apply ... [foo] intro + edit shared` lines on stderr are
  a rebase fixture's expected output, not a failure.

## Resource contract

Everything here is a testing SEAM around an operation that already runs. Nothing
in this plan adds a background service, and a reviewer should treat any of the
following appearing in the diff as a defect rather than an improvement
(codex on fbb84e3):

- The panel keeps its existing receive cadence and its existing rate limit. No
  new timer, no new watcher, no extra probe per turn.
- Peek starts no command source, no monitor repair, and no local watcher.
- The handoff stays one coalesced, bounded repair per repo, releasing its permit
  on every exit path. A healthy attachment stays cheap.
- No permanent polling loop and no process scanning, anywhere.

## Out of scope

- Any other test. These three are what the suite currently gets wrong about
  itself: one gate that can no longer discriminate, one that fails before its
  own assertion, and one that fails for a reason that is not about the code.
- Reopening the monitor-handoff or runtime-state architectures. Both are
  finished; this plan touches their TESTS and their seams, not their designs.
- Extracting the terminal layer in general, introducing a UI framework, or making
  unrelated screens testable. The panel work is one completion operation on the
  main turn, and nothing more.
- The dead `51fb3c8` review citations in `log.rs`, `status_tui/scroll.rs` and
  `status_tui/mod.rs`. Same class as the squashed-sha problem, four real
  instances, and a different job — noted so it is not lost, not adopted here.
