# the-watcher-test-waits-for-quiet

The watcher test assumes nothing is in flight. It should check.

## Why

`cli::status::dirty_and_wake_tests::watcher_wakes_on_tracked_edit_not_on_ignored`
was failing reproducibly when this was queued:

    run 1 (after a rebuild)  ok       1.64s
    run 2 (same binary)      FAILED   0.60s
    run 3 (same binary)      FAILED   0.60s

on `ignored build artifact woke the loop` — the assertion that writing
`target/debug/out.o` produces NO wake.

**It does not reproduce now.** Twelve consecutive warm runs pass, and
the whole-lib suite has run dozens of times today without it failing.
The test was unchanged since: a fixed 250 ms sleep before the drain,
500 ms assertion windows.

So this plan is no longer about a failing test. It is about a test
whose premise — "nothing is in flight" — is established by sleeping
rather than by checking, on a machine whose load decides whether that
sleep is long enough. The recorded failure is evidence the hazard
bites; its absence today is evidence about today's machine, not about
the test.

The setup committed `.gitignore` files through `git`, then slept
250 ms, then drained the channel, then wrote the ignored path and
asserted nothing arrived within 500 ms. If any of the setup's own wakes
were delivered AFTER the drain, they were indistinguishable from a wake
caused by the write.

There is no debouncer — an earlier draft of this plan said there was,
and there is not (codex fdc9c59). Two producers share this channel:

- `RepoStateWatcher` holds a `notify::PollWatcher` that restats every
  `POLL_INTERVAL` — 500 ms (`repo_watch.rs`). A change made at t can
  therefore be REPORTED as late as t+500 ms.
- `watch_diff_paths` uses `notify::recommended_watcher` and sends
  straight from the callback (`status.rs`), so its latency is the
  native backend's, not a window clank chose.

The 200 ms coalescing in `status.rs` belongs to the status CONSUMER
loop, which this test never runs. So the sleep to beat was the 500 ms
poll interval, and the test slept 250 — half of it.

A gate whose verdict depends on machine load is not a gate. The
whole-lib suite runs before every review, and a test that was
dismissed as flaky for weeks is one nobody reads the failure of.

## The build

**The test waits for quiet, not for a duration.** Drain until the
channel has been silent for a settling budget, rather than sleeping a
fixed time and hoping. Every wake-vs-no-wake assertion in that module
gets the same treatment; they share the hazard.

**Quiet is evidence, not a guarantee.** A poller that restats every
500 ms can always deliver something later; observing silence for
longer than a poll cycle is good evidence nothing is in flight, and
that is all it is. The helper should say so, so nobody reads it as a
barrier.

**The budget is justified by the producer.** It comes from
`repo_watch::POLL_INTERVAL` — the one real constant here — with slack
for the native backend, rather than a number chosen to make the test
pass. If the poll interval changes, the budget follows it.

## Tests

- The test fails when the ignore filter is broken — mutate
  `WakeFilter::wakes`, which is what `watch_diff_paths` consults, so
  the mutation reaches the thing the assertion is about. The settle
  must happen BEFORE the tested write, or a broken filter's wake is
  drained away and the mutation survives.
- It passes twenty runs in a row on a warm binary.

## What the load run actually showed

It did not discriminate, and this plan predicted it would. Recorded
here because `clank finish` squashes the cycle, and a finding that
lives only in a commit message does not survive the squash
(ruthless 2293844).

- the OLD fixed sleep: 12/12 warm, and 6/6 under 17 CPU burners.
- the NEW settle: passes both.
- mutating `settle` back to a bare drain: SURVIVES.

So the load run was not the discriminator this plan said it would be,
and no number distinguishes the two versions on this machine. The
historical 2-of-3 failure is evidence the hazard bit once; it could
not be made to bite again.

What IS demonstrated is the thing that could have gone wrong and did
not: mutating `WakeFilter::wakes` to pass everything still FAILS the
test. Settling before the tested write does not drain away the
evidence the assertion needs.

So the justification for this change is premise-establishment — the
test now checks that nothing is in flight instead of assuming it —
and NOT a demonstrated repair. Anyone reading this later should know
the difference, because the test looked fine before the change too.

## Out of scope

- The watcher itself, which is not accused of anything here.
- The other timing-shaped tests in the suite, unless they are in this
  same module and share this hazard.
