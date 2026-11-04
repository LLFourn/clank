# github-watches-are-event-driven
# A GitHub watch listens to webhooks, polls once when it starts listening, and polls on a timer only if asked

## Why

Today a GitHub watch polls by default. `"delivery":"realtime"` is an
opt-in that adds `gh webhook forward` as a faster path, while polling
keeps running on its interval as the "completeness backstop". The user
asked why we still poll when we have live events. Their design:

> When you start the listening you can do a single background poll to
> catch up. Otherwise it should be event based.

Follow-up from the user: "since we have the polling code I don't mind
if we sort of keep it. Basically just merge the two options, it always
does realtime and it always poll on start but you can enable polling
every X mins if we want in the background too."

### What this guarantees, and what it does not

Two different promises, kept apart:

- **Presentation is at-least-once, unchanged.** Once an event is
  accepted into the agent's inbox, it wakes the agent until acked.
- **Observation upstream has a stated gap when no timer is set.**
  - GitHub's repository events feed lags real activity by 30 seconds
    to six hours
    (https://docs.github.com/en/rest/activity/events#list-repository-events).
    So an action taken just before a watch starts listening can be
    absent from that start's single catch-up read.
  - The new webhook won't deliver it either, because the action
    predates the subscription.
  - GitHub does not retry a failed webhook delivery
    (https://docs.github.com/en/webhooks/using-webhooks/handling-failed-webhook-deliveries).

  The user chose to accept this gap rather than add follow-up polls.
  A watch with no `poll_interval` can miss an action that is still
  lagging in the feed when it (re)starts listening. `poll_interval` is
  the optional backstop that closes the gap. It is not required, and
  no follow-up reads are added by default. The README and the skill
  state this limit plainly. A healthy idle watch with no
  `poll_interval` makes zero API calls.

## What changes

1. **Every GitHub watch is realtime.** The `delivery` option and the
   `Delivery` enum go away. `poll_interval` stays, but it means
   something new: an OPTIONAL background poll on that interval, on top
   of the webhook. It is absent by default, and absent means no timer.
   - Old configs that still carry `delivery` parse and ignore it.
     Nothing on disk needs migrating, and no existing agent config is
     refused.
   - `doctor` names an ignored `delivery` once, so it can be cleaned up.
   - An old poll-mode watch that relied on the implicit default
     interval now has no timer. That is the point of the change.
   - A watch's source key must not change because of this. A
     `"delivery":"poll"` watch and its realtime twin keep the same
     log. Check `supervisor.rs`'s `source_key`; delivery is already
     not part of it, and it must stay out.

2. **The listening lifecycle.** For each watch, the ingest-lease
   holder runs the following.
   - Start the relay (`gh webhook forward` into the loopback listener)
     and wait until it is PROVEN ready. The banner gh-webhook prints is
     not that proof: it comes before the hook is active
     (https://github.com/cli/gh-webhook/blob/main/webhook/forward.go).
     Proof is the hook's `ping` delivery arriving at our loopback
     listener, end to end. GitHub sends `ping` when a hook is created,
     and `gh webhook forward` creates a fresh hook each time it starts.
     - No `ping` within a bound (60s) counts as a failed start: kill
       the child, raise the diagnostic, retry with backoff.
     - `ping` itself is never classified as an event.
   - Then run ONE catch-up in the background:
     - reconcile the event feed against the inbox, as the first poll
       does today, including the "may have been missed" warning past
       the feed's retention;
     - read the checks once for every PR in scope.

     The relay starts BEFORE the catch-up, so nothing falls between
     them. The coordinator's dedup already makes an event seen by both
     paths a single entry.
   - From then on, unless `poll_interval` is set, nothing is fetched on
     a timer:
     - feed events arrive only by webhook;
     - checks are read only when a webhook cue says they may have
       moved (`CHECK_CUES` in `coordinator.rs`, plus a PR head change).
   - With `poll_interval` set, the existing poll (feed and checks) also
     runs on that interval, deduped against the webhook as it is today.
   - Relay failure is something production code actually observes:
     - the child exits;
     - or the child reports a forwarding error on its output while it
       stays alive. gh-webhook warns about a failed local forward and
       carries on; we read its stderr and treat such a line as a
       failure. Match the actual strings in forward.go, and pin them
       in a test.

     Either one means: kill the child if it is still alive, raise the
     diagnostic, and restart with backoff. Every proven-ready restart
     is a new start of listening and runs its own catch-up.
   - A quiet repo is never a failure. No restart and no read happens
     because time passed without deliveries.
   - A follower that takes over the lease is also a new start, with a
     relay and a catch-up of its own.

2b. **One forwarder per upstream repo per monitor.**
   - **Why this is needed.** GitHub allows one CLI forwarder per repo
     or org; a second gets "Hook already exists"
     (https://docs.github.com/en/webhooks/testing-and-troubleshooting-webhooks/using-the-github-cli-to-forward-webhooks-for-testing).
     Today each watch's ingest-lease holder runs its own
     `start_relay`, and a per-watch WAL lease doesn't own that
     repo-wide slot. So the transport moves out of the watch and into
     the monitor.
   - **The `RepoTransport`.** The monitor keeps one per upstream repo,
     shared by every agent's watches of that repo. It is refcounted by
     subscribing watches and owns the `gh webhook forward` child, the
     loopback listener and the proven-ready state.
   - **Events.** It subscribes to the UNION of the webhook events its
     watches need. That includes `pull_request` (head changes) and the
     check events in `CHECK_CUES` for any watch with `ci_passed` /
     `ci_failed`, even a CI-only watch.
   - **Fan-out.** Each delivery is fanned out to every subscribed watch
     through that watch's own existing path: filter / `Selection`,
     source key and WAL, dedup, acknowledgement. One watch's filter
     never decides for another.
   - **Joining.**
     - A watch whose events the current subscription already covers is
       subscribed at once. It runs its own catch-up (§2) for its own
       log, starting after the transport is proven ready, or
       immediately if it already is.
     - A watch that needs events outside the current set expands the
       union. The transport restarts the forwarder with the new set,
       and that restart counts as a new start of listening for every
       subscriber, so each runs its catch-up, as with any restart. So
       expansion loses nothing.
   - **Leaving.** A leaving watch is unsubscribed and its fan-out stops.
     The forwarder keeps running for the others; the subscription is
     not narrowed while others remain, so no needless restart. When
     the last subscriber leaves, the transport stops the child and
     releases the slot.
   - **"Hook already exists".** If the forwarder reports this (another
     monitor, another machine, or a person's own `gh webhook forward`
     holds the slot), it is an external slot conflict. It is reported
     as exactly that ("another forwarder holds the webhook slot for
     o/r"), never as missing admin and never as ready.
     - Watches with `poll_interval` keep polling, shown as degraded.
       Others only show the diagnostic.
     - Retry uses backoff with a long ceiling (minutes, not seconds),
       once per transport, not once per watch. So competing watches
       can't cause a restart loop.
   - **Scope.** Sharing is within one monitor, and a monitor serves one
     repo, so the boundary is this: two clank monitors in different
     repos watching the same upstream will conflict. The second sees
     the slot conflict above. The README states that.

3. **No silent loss in intake, and a defined start-up overlap.**
   - **Intake overflow becomes recovery work.** `RelayInbox`'s ingress
     buffer (`RELAY_BUFFER_CAP`, `relay.rs:294`) and the coordinator's
     pre-baseline buffer (`coordinator.rs:198`) evict the oldest
     delivery on overflow. They rely on later polls, and the listener
     still answers 200. Both stay bounded. An eviction now marks the
     watch as needing a reconcile, which is retried until it succeeds
     (§4), and the event log records that the relay overflowed. That
     reconcile is subject to the same feed-lag limit, stated in the
     docs.
   - **Cold start, with no log for this source.** The catch-up read is
     history and is baselined as handled, as today. Any webhook
     delivery received after the relay was proven ready is LIVE: it is
     pending and wakes exactly once, even when the same action also
     appears in the baseline page. `Coordinator::baseline` currently
     seeds the catch-up's actions as handled before draining the
     buffered webhooks, which would suppress such an overlap. Reorder
     so that a live delivery's identity is never pre-seeded as handled.
   - **Warm start, with a log.** The catch-up reconciles the feed
     against the log. Anything not in the log is pending. A webhook
     delivery and its feed copy are one entry, as today.

4. **Unfinished work is retried until it succeeds; idle work is
   nothing.**
   - One catch-up is one LOGICAL reconciliation. It is retried with
     backoff until it completes: a login failure, a network error, or
     a page that fails halfway. A partial pagination failure keeps the
     committed cursor and re-reads from it.
   - A CI read triggered by a cue is likewise pending until it
     succeeds, without waiting for another cue.
   - A cue that arrives while a read is in flight marks the PR dirty,
     and one more read runs after it.
   - Rate-limit reset deadlines are honoured.
   - These are one-shot deadlines that exist only while work is
     pending, and disappear on success. They are not a timer.

5. **Coalesce check cues; never drop them.** Today, cues that arrive
   in quick succession are dropped, and the next poll covers the loss.
   With no polling that would lose conclusions. A cue that arrives
   inside the rate-limit window must schedule ONE trailing read at the
   window's end. That read sees the state after the last cue. Several
   Apps finishing at once still cost one read.

6. **No silent fallback.** If the relay cannot start, the watch raises
   a loud, repeating diagnostic. It keeps polling only if
   `poll_interval` is set, and the diagnostic says so either way. That covers no `gh`, no
   `cli/gh-webhook` extension, no admin on the repo, or the forwarder
   refusing to start. The diagnostic surfaces the way source failures
   already do (`source_status`). It names the reason and the fix,
   e.g. "needs admin on o/r to create a webhook".
   - It retries with backoff, and a retry that succeeds runs the
     catch-up.

7. **The poll scheduler becomes opt-in.** The interval tick in
   `coordinator::poll_loop` runs only when `poll_interval` is set. The
   implicit default interval goes. The catch-up and the timer both
   use the same code that polls once.

8. **Docs.** Rewrite the README's github sections and the clank-github
   skill (`setup_assets/github_events_skill.md`), covering "Realtime
   notes", "Catch-up semantics", "CI conclusions are read" and the
   requirements list:
   - the WEBHOOK path needs `gh`, `cli/gh-webhook` and admin on the
     repo; without them a watch with `poll_interval` keeps polling and
     shows as degraded, never as a working webhook;
   - the feed-lag gap in "What this guarantees", stated plainly;
   - it catches up once each time it starts listening;
   - CI is read when a check cue arrives;
   - `poll_interval` adds an optional background poll, which is also
     how to watch a repo you don't administer.

## Out of scope

- No new socket or listening port. The relay's loopback listener
  exists already.
- No change to inbox, ack or event-identity semantics.

## Acceptance

Tests run in-process with fakes for the forwarder, the fetcher and the
CI reader. They never spawn gh, clank or a monitor.

- **Start:** the relay is proven ready (a `ping` delivery) before the
  catch-up begins. A child that prints its banner but never delivers
  `ping` is a failed start. The
  catch-up runs exactly once: one feed reconcile and one checks read
  per PR in scope. After it, with no webhook, a long run of
  fake time makes no fetch at all.
- **Restart:** a forwarder exit is followed by a restart and exactly
  one more catch-up. An event that fired while the forwarder was down
  is delivered by that catch-up exactly once, even if the webhook
  later delivers it too.
- **Shared transport:**
  - Two agents' watches plus a differently scoped watch of the same
    repo start exactly one forwarder. A delivery reaches each watch
    whose filter admits it, and only those. Each watch logs and acks
    independently.
  - When either watch leaves, the other still receives deliveries and
    the forwarder is not restarted. When the last one leaves, the child
    stops.
  - A joining watch that needs a new event (a CI-only watch joining an
    issues-only one) restarts the forwarder with the union, including
    `pull_request` and the check cues. Every subscriber then runs one
    catch-up.
  - A forwarder that reports "Hook already exists" is never ready, and
    the diagnostic names an external slot conflict, not admin. It
    retries once per transport with a long backoff, never once per
    watch. A watch with `poll_interval` keeps polling meanwhile.
- **Relay health:**
  - A forwarding-error line from a child that stays alive triggers a
    restart and a catch-up.
  - A healthy relay that receives nothing for a long fake-time span is
    never restarted and makes no read.
- **Intake:** overflowing the ingress buffer marks a reconcile, which
  runs, and the event log records it.
- **Overlap:**
  - Cold start: an action delivered live after readiness that also
    appears in the baseline page is pending and wakes exactly once.
    Pre-watch history in the same page is baselined.
  - Warm start: an action missing from the log is pending.
- **Retry:**
  - A catch-up whose first attempt fails (login, network, half a page)
    completes on retry with no webhook arriving, and keeps the
    committed cursor.
  - A final CI cue whose read fails is re-read until it succeeds, with
    no further cue.
  - A cue during an in-flight read gives one more read.
  - These pending retries are the only fetches in an otherwise idle
    span; healthy idle makes none.
- **Feed lag:** an action that is absent from the catch-up's feed read
  and predates the hook is not delivered when `poll_interval` is
  absent, and is delivered by the first timer poll when it is set.
  This pins the stated gap.
- **Cues:**
  - N check cues inside one rate window give exactly one trailing
    read, after the last cue.
  - A conclusion that changes between the first and the last cue is
    reported.
  - Mutation: restore drop-on-cooldown, and this test goes red.
- **Relay failure:**
  - Without `poll_interval`: a relay that can't start raises the
    diagnostic, naming the cause, and the fetcher is never called.
    After the retry succeeds, the catch-up runs once.
  - With `poll_interval`: the diagnostic is raised and polling
    continues on the interval.
- **Optional timer:** with `poll_interval` set, fake time past N
  intervals gives N polls on top of the catch-up. With it absent,
  there are none.
- **Old configs:** an agent config carrying `"delivery":"poll"` loads,
  and its source key equals the key of the same watch without it.
  `doctor` reports the field.
- **Mutations:**
  - Restore the implicit default interval, and the "no fetch without
    a webhook" test goes red.
  - Start the catch-up before the relay is ready, and the ordering
    test goes red.
- `cargo test -p clank --lib` (the github modules, then the whole
  lib), the `it` gates for the files touched, clippy, and fmt.

## As landed

As planned, with these specifics:

- **Readiness is the hook's `ping`.** It was verified live against
  LLFourn/clank before landing: the ping arrives about 2s after
  `gh webhook forward` starts, and the hook is deleted when the
  forwarder stops.
- **Failure strings** are pinned in `the_gh_webhook_strings_are_pinned`:
  - `warning: error forwarding event:` comes from gh-webhook's
    `forward.go`;
  - "Hook already exists" and `you do not have access to this feature`
    come from `create_webhook.go`'s error path, and are GitHub's API
    responses;
  - `unknown command "webhook"` is gh's own error, taken from
    knowledge, not fetched.
- **Backoff.** A held slot backs off from 2 to 30 minutes, other
  failures from 5 seconds to 5 minutes, once per transport.
- **`RelayInbox` keeps control state apart from the lossy action
  queue:**
  - one slot for the latest health;
  - a separate catch-up debt;
  - a coalesced check cue that can't be evicted and isn't held behind
    the own-login gate.

  An intake overflow owes both a feed reconcile and a checks read.
  The overflow is logged as a new `relay_overflow` WAL record, which
  older binaries skip.
- **Retries are owed until they succeed.** Feed retries also honour
  GitHub's X-Poll-Interval floor. CI dirtiness is one flag per watch,
  because a checks read covers every PR in scope.
- **Upgrade.** `delivery` is no longer serialized, so every GitHub
  watch's config fingerprint changes once. A monitor still on the old
  binary reports "an edit was rejected" until it restarts.
- **Docs.** `book/controller.html` and the master skill were updated
  too, because they described `"delivery":"realtime"`.
