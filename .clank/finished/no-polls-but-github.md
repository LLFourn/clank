# no-polls-but-github

Invariants: I10.

## Why

The user's rule: the only poll clank may run is GitHub's. Production code still has dozens of timed sleeps, for example `wait.rs:591`, `:743` and `:1026`, `agent_store.rs:1669`, `owner_sentinel.rs:40` and `upgrade.rs:1105`. Nobody has classified them. Some are bounded retries or backoff, which are not polls; others may be loops that check state on a timer. No gate stops a new one.

## Build

1. **What counts as a poll.** Every production timed wait is one of these, judged by what ends it and what re-arms it:
   - **Scheduled domain work:** the wait is for a time the user or the domain chose, such as a timer source's `due_at` or `repeat_every`. Allowed.
   - **One-shot deadline:** it bounds a single wait for an event and is not re-armed by its own expiry, such as a handoff budget or a report deadline. Allowed.
   - **Bounded retry:** it retries a failed operation a finite number of times, or within a finite total duration, and then fails or reports. Allowed. A capped *interval* alone is not a bound.
   - **Defensive throttle:** it delays work an event already triggered, so a burst costs one pass. It never wakes without an event. Allowed.
   - **Poll:** it wakes again while the observed state may be unchanged, with no bound on how long it keeps doing so. Forbidden, except GitHub's.
2. **Inventory every site.** Classify every production timed-wait site in the workspace by those rules: `sleep`, `sleep_until`, `interval`, `interval_at`, `timeout`, `timeout_at`, `recv_timeout`, `wait_timeout`, `park_timeout`, and any helper that wraps one, such as the monitor's deadline helpers and `timer_source::clock::sleep_until`. Name the caller that repeats each one, not just the helper.
   - `monitor::ContentionRetry` is classified by the same rule. `look()` re-arms it for as long as contention lasts and only its pause is capped, so it is a poll of another process's lock. macOS gives no notification when a flock is released. Unless a replacement is found that still guarantees progress after contention clears, it is a recorded residual (step 5), not "a bounded retry".
   - Start from fresh sites. Some examples in the earlier draft are stale; `upgrade.rs:1105` is test-only.
3. **Replace each poll** with the event it waits for (`tree_watch`, `fs_notice`, a pipe, a child exit, a socket), or delete it. Each replacement gets two tests:
   - **no lost wake:** the race between checking the state and starting to wait for the event is closed;
   - **no periodic wake:** nothing wakes while the observed state stays unchanged, shown with a counter or a paused clock, not elapsed time.
4. **A site-level gate.** A source gate in the `it` harness lists every production timed-wait site as (file, function, form, ordinal within the function), each with its class and a one-line reason.
   - Adding, removing or reordering a timed wait in any function, a listed one included, changes the inventory and fails the gate, which forces a reclassification.
   - Its limits are stated in its doc: it detects the timed-wait forms and named helpers above syntactically. It does not prove a loop's semantics, so the classification itself is reviewed.
5. **Residual polls are recorded, never kept silently.** If a poll has no event to wait for, list it in the commit with why, keep it in the gate as `Poll` with its reason, name it in I10's `Today`, leave I10 `target`, and queue a follow-up. The user's rule allows only GitHub's poll.
6. **Registry:** I10 becomes `holds` only if no `Poll` remains but GitHub's. Its `Tests` names the gate and the replacements' tests.

## Acceptance

- Every production timed-wait site is in the inventory with a class.
- Mutations:
  - add a new timed wait in an unlisted function;
  - add a second timed wait in an already-listed function;
  - wrap a sleep in a new helper and loop on it.

  Each fails the gate.
- Each replaced poll has its no-lost-wake test and its no-periodic-wake test.
- I10's status matches what the inventory shows.
