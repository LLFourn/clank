# the-remote-terminal-is-never-silently-stale
# The web Terminal mirrors the real pane or says it cannot: a dead or stalled stream is detected, shown as stale, and resynchronised

## Why

Reported by Life's assistent for Lloyd, 2026-10-17 (screenshot: `~/src/life/notes/attachments/clank-frozen-console-2026-10-17.jpg`). The web Terminal for secg-search / codex showed a Codex "Giving this request a little extra thought" overlay at 10:19, with no stale or disconnected indication. By Lloyd's report, the real pane had already moved well past that screen.

Established so far (2026-10-17 10:25):
- secg-search's panes live in zellij session `clank-fs-war-room` (terminal_4 claude, terminal_6 codex, terminal_7 grok). Their `zellij subscribe --format json --ansi` child (pid 96150) has run since 09:46:58, under secg-search's `clank tui` (pid 29615). The child did not die and was not restarted around 10:19.
- A read-only `subscribe -p terminal_6` at 10:25 got a current initial frame. The real pane by then showed a different screen ("Sorry can you continue? …").

Structural gaps, independent of which layer stalled:
1. **Subscription death.** `web::pane_poll` restarts the subscription only when pane membership changes (`TableChange::Membership`). A subscribe child that exits, or stops emitting, is never noticed, and the reader thread just ends.
2. **No freshness signal.** Nothing end to end tells a stalled stream apart from an idle pane. The page shows the last retained viewport as current; there is no liveness or age shown, and no resync trigger from the browser's side.
3. **Lag.** `feed` broadcasts at capacity 2, and resyncs a lagging connector from retained state (`bridge.rs`, `service.rs`). That is by design, but unverified for this failure.

## The freshness contract

**Two separate facts.**
- **Transport liveness:** each hop (browser, host, bridge) is connected. Heartbeats prove it, and prove nothing else.
- **Live:** the screen a browser shows for a pane matched zellij's pane at a moment that browser can bound. Only a confirmation the browser itself requested proves it (below). Nothing else does: a subscription frame, a sample result alone, a timer, the child process existing, a replay, a reconnect, a heartbeat.

**Per-pane state in `Feed`:**
- the retained viewport;
- a content `revision`, which increases whenever the retained content changes, whether by an accepted subscription frame or a published sample. An equal sample leaves it unchanged;
- the subscription generation the content came from.

**Measured age.** Each pane frame carries `age_ms`: the time since the bridge last took or confirmed that content. Each hop that holds a frame (the bridge's queue, the host's retained `BrowserFrames` replayed by `service::pump`, the browser) adds its own residence time on its own monotonic clock. Transit, socket and pipe delay are not measured, so age is a lower bound. It drives only the "not verified for N s" text.

### Confirmation: the one way a pane becomes live

**The request.** A browser that is demanding panes (see Demand) sends `verify { id, panes }`. It records the request's local monotonic send time `t0` under that `id`.

**The cutoff.** The host forwards each request to the bridge with its `id` and the browser's connection identity, unchanged. The bridge stamps each request's arrival. A sample answers a request only if the sample was **acquired after that request arrived**: its short-lived subscribe was started after the request's arrival stamp. A request arriving while a sample is in flight never borrows that sample. It waits for the next sample, which is shared with every other waiting request, so a late request costs no extra work.

**The answer.** When an eligible sample completes, the bridge answers each waiting request with `confirmed { id, pane, revision }`, for each pane whose sample matched the retained viewport at that revision, or whose sample was published as the new content and revision. The host routes each answer back to the connection whose `id` it is.

**Live in the browser.** The browser treats a pane as live until `t0 + FRESH` (30 s), where `t0` is the send time it recorded for that `id`, and only while its shown revision equals the confirmed revision. All delivery time in both directions counts against the window. A response that arrives after `t0 + FRESH` confirms nothing. A newer subscription frame raises the revision and ends live, until a confirmation of the new revision.

### Demand: one protocol

**`verify` is the demand.** A browser sends `verify { id, panes }` every 15 s while all of these hold:
- its Terminal view is showing;
- `document.visibilityState` is `visible`;
- it has a connection.

When they stop holding, it sends `verify { id, panes: [] }`. There is no separate demand message.

**Expiry.** The bridge keeps each connection's latest `verify` as its demand, and drops it 30 s after it arrived. So a hidden tab, a Chat or Team view, or an abandoned socket stops demanding within 30 s. The demanded set is the union across connections, while each request keeps its own `id` for answering.

### Budget: one schedule

**Normal sampling.** Per repo, at most one sample is in flight. A new one starts only when both:
- at least one request is waiting that no in-flight or completed sample can answer;
- at least 15 s have passed since the last sample started.

So with continuous demand, sampling is one short-lived subscribe per 15 s per repo, covering every demanded pane, active or quiet. With no demand, nothing samples.

**Stall check.** A sample whose content differs from the retained viewport at its starting revision might be a healthy change whose subscription frame is still in flight. So one confirming sample runs 2 s later. This is the only exception to the 15 s spacing, at most one per regular sample, and it stays inside single flight. The confirmation applies, using the ordering rule below, only if the generation and revision have not moved. If it also differs, the subscription is stalled. Then, atomically: the confirmation's content is published, the revision increases, and the subscription restarts in generation `G + 1`. The stall check also answers the requests that were waiting.

**Ordering a sample.** A sample records at its start the generation `G`, and each pane's id and revision `r`. Its result applies to a pane only if those are unchanged on arrival; otherwise it is discarded for that pane. A late sample from before a newer frame, membership change or replacement subscription therefore neither publishes content nor restarts anything.

## Stream identity and order

**Generations.** Each long-lived subscription has a generation number, and its reader tags every frame with it. `Feed` accepts frames only from the current generation, so a late frame from a retired subscription is dropped. A pane replaced with a new id is a new pane, and starts at "awaiting first frame".

**Sequences.** Bridge sequence numbers are per bridge connection, as today. A gap, or a host `Lagged`, triggers the existing bridge replay, which carries each pane's age and revision and never confers live. A gap never needs an upstream sample.

**A snapshot** establishes the retained viewports, revisions and ages at that instant, never live.

**What the page shows for a pane:**
- **awaiting first frame:** no viewport yet;
- **live:** as defined under Confirmation;
- **unverified:** anything else with a viewport. The retained screen is shown marked "not verified for N s", never as current;
- **disconnected:** the transport is down, as today;
- **closed:** the pane is closed, as today.

Unverified clears only on a timely confirmation. Disconnected clears on reconnect, after which the pane is unverified until it is confirmed.

## What to build

**Supervision.** In the bridge's pane subscription supervisor:
- a subscribe child that exits or reaches EOF is restarted with backoff (1 s, doubling to 60 s), in a new generation;
- its old child and reader are stopped and joined;
- its panes keep their ages, and become unverified once past `FRESH`.

**Freshness and verification.** Per-pane revision and generation in `Feed`, `age_ms` on every frame, and the confirmation, demand and budget rules above, with request ids routed through the host.

**The page.** It tracks age using its own monotonic clock and renders the five states. On `visibilitychange` or `pageshow` it requests a resync: a bridge replay plus an immediate `verify`. That `verify` waits for the next eligible sample under the cutoff rule.

**Root cause.** Whatever the deterministic fixtures, or Lloyd's reload check, show was the original stall gets fixed at its layer. The plan's As built records it.

**Stated limits.** The page says that zellij frames are a viewport sample, not a byte stream.

**Never as part of this:** pressing retry, model-switch or dismiss in an agent, interrupting an agent, or resending input.

## Acceptance

Tests use deterministic fixtures with a fake subscription, a fake sampler and the real `Feed`. The browser checks drive the real page reducer.

- **A quiet healthy pane:** no updates; under demand, each sample matches and confirms it, so it stays live, and no subscription is restarted.
- **A changing healthy pane:** a differing first sample is followed by a live frame before confirmation, so the confirmation is discarded. No restart, and the live content stands.
- **A late-joining request:** sample A is acquired, then request B arrives, then A completes. A answers no part of B. B is answered by the next sample, acquired after B, and that sample is shared.
- **Routing:** two viewers' requests with different ids are each answered under their own id, and each times live from its own `t0`.
- **The budget holds:** under continuous demand, samples start at most every 15 s, plus at most one confirming sample 2 s after a mismatch.
- **A delayed old sample:** one started under generation `G` or revision `r`, arriving after a newer live frame or a replacement subscription, neither replaces content nor restarts anything.
- **Delayed in transit:** a confirmation held in a fake connected transport past `FRESH` arrives and does not make the pane live. A timely confirmation for the same pane and revision does.
- **Old subscription output:** a frame of old content delivered after newer source state exists updates the content, but does not make the pane live. The next timely confirmation, from a sample of the newer state, does.
- **Host cache replay, paused clock:** the host retains a pane frame and repo-to-host progress stalls. A browser that connects five minutes later receives it at least five minutes old, and shows it unverified.
- **An alive but stalled child:** the fake child stays up, but stops forwarding while the fake source changes, and heartbeats continue. The page goes unverified within `FRESH` of its last timely confirmation. The next sample and its confirming sample both differ: the subscription is restarted in a new generation, the confirming sample's viewport is published, and the pane reads live only once a request answered by that sample confirms its new revision.
- **Retained-stale replay:** a bridge replay or reconnect of an unverified pane keeps it unverified.
- **A late frame from a retired generation** is dropped.
- **A dead child:** EOF restarts it with backoff, and old children and readers are joined.
- **Connector lag or reconnect:** replay restores the content and ages, without resetting freshness.
- **Visibility resume** triggers one coalesced resync.
- **Cost:**
  - no samples run with no terminal demand, including with a connected bridge and a browser on Chat or hidden;
  - demand expires 30 s after a viewer stops renewing;
  - two viewers of the same pane cause one sampling schedule;
  - all demanded panes, active or quiet, share one sample per 15 s window per repo.

## What is proven about the original incident

The subscribe child was alive across the freeze (pid 96150, 09:46:58 onward), and zellij delivered current frames when sampled read-only afterwards. Which downstream layer stalled is not proven. The fixtures make every candidate layer's failure visible and recoverable. Lloyd's reload check would be useful evidence, but it is not a condition for finishing.

## As built

**One change from the planned routing.** A `verify` request is answered by a `confirmed` frame, not by its HTTP reply, and the browser keys its requests with a per-load tab id. The reason: the repo connector (`clank_web::bridge::run_connector`) handles one request at a time, inline with outgoing updates, so a reply held open until a sample completed would stall every other action and the screen updates themselves. Instead, `/verify` is registered and acknowledged at once (204). The answer is broadcast as `confirmed { tab, id, pane_id, revision }`, and each page keeps only answers carrying its own tab and request id. The ids are random and not secret. There is no new socket and no new route type beyond the POST.

- **`web/freshness.rs` (new):** the pure contract, over injected instants.
  - Demand is per tab, expiring after 30 s, and an empty `verify` ends it.
  - A sample answers only requests that arrived at or before its start.
  - Regular samples start at least 15 s apart, single flight, and only with a request waiting.
  - One confirming sample runs 2 s after a mismatch. If the generation and revision are unchanged and it also differs, the stall is declared, and the waiters are answered with the new revision.
  - A sample's result applies only if the generation and revision are unchanged.
  - A sample is abandoned after 5 s (`SAMPLE_TIMEOUT`) or on failure, and its waiters wait on.
- **`web/feed.rs`:**
  - `Retained` gains `generation`, per-pane `revisions` (bumped only when content changes) and `taken`.
  - Pane frames carry `revision` and `age_ms`.
  - `event_in` drops frames from a retired generation.
  - New: `next_generation`, `freshness_view`, `publish_sampled`, and `confirmed`, which broadcasts and is never retained.
  - `taken` is refreshed on confirmation.
- **`web/mod.rs`:** `pane_poll` is the per-repo supervisor.
  - It runs a 1 s tick on tokio's clock.
  - It re-lists the panes every `PANE_POLL`, as before.
  - A subscription whose reader finished is restarted after 1 s, doubling to 60 s, in a new generation through `resubscribe`.
  - Verify requests from the channel go to `Freshness`.
  - Samples run on `Blocking` through `Panes::sample`. For zellij that is `sample_initial`: a short-lived `subscribe` read for the initial frames until all arrive or `SAMPLE_TIMEOUT`, after which the child is killed and its reader joined.
  - A stall publishes the confirming sample and resubscribes; confirmations are broadcast.
- **`web/repo_session.rs`:** `BrowserRequest::Verify` is forwarded to the supervisor, which replies 204, or 409 when no supervisor exists.
- **`clank_web`:**
  - `request.rs` adds `Verify { tab, id, panes }`.
  - `service.rs` adds the `POST verify` route, and its replay sends every retained frame through `replay::aged`.
  - `replay.rs`:
    - pane frames the host retains are stamped with the host's monotonic clock, on both the merge and the snapshot path;
    - `aged` adds the time held to `age_ms` and drops the stamp;
    - `confirmed` frames are live-only.
- **`page.html`:**
  - a per-load `TAB`;
  - `verify` every 15 s while the agent's Terminal view is shown, the document is visible, the narrow Timeline is closed and the socket is connected, an empty `verify` when that stops, and an immediate one on `visibilitychange`, `pageshow` and reconnect;
  - each request's send time is kept on `performance.now()`;
  - a pane reads `live` only while its shown revision has a matching `confirmed` for one of this tab's requests, until that request's send time plus 30 s;
  - a status line shows live, "not verified for N s" (the measured lower-bound age), awaiting first frame, or disconnected; the closed overlay is unchanged;
  - a disconnect clears every live window.
- **Tests:**
  - `web::freshness` (10), including an answered request not being sampled again until the next: no demand, no sample; a matching sample confirms the request it postdates; a late-joining request waits for the next shared sample; a newer frame voids a sample; a changing healthy pane is not stalled; two differing samples prove a stall and answer the waiters; a hung sample is abandoned; demand lapses; two viewers share one sample with their own ids.
  - `web::tests::supervisor` (5, paused tokio clock, test-owned child processes):
    - an alive but stalled subscription is proven by two samples and replaced, with the content published and resubscribed;
    - a quiet healthy pane: one sample per window and no restart;
    - a changing healthy pane is not restarted;
    - a dead subscription is restarted with doubling backoff;
    - nothing is sampled without demand.
  - `web::feed`: a frame from a retired generation is dropped.
  - `clank_web::replay`: a pane frame held five minutes replays 300 s older on an injected clock, and confirmations are not retained.
  - `tests/browser/check.js` (the real page; fake `performance.now`):
    - a frame alone is not live;
    - the page demands its visible pane;
    - a confirmation after 30 s confirms nothing;
    - another tab's answer is ignored;
    - a timely confirmation of the shown revision is live;
    - a newer revision is not live until confirmed;
    - a replayed frame shows its age;
    - returning to the page asks at once;
    - a lost connection reads disconnected;
    - no page errors.
- **Mutations, each caught:**
  - the page accepting any tab's answer, or treating a frame as live;
  - no restart on a stall;
  - never detecting a dead subscription;
  - no backoff;
  - dropping the generation check;
  - dropping the sample cutoff;
  - sampling with nothing waiting. This one survived until `an_answered_request_is_not_sampled_again_until_the_next` was added.

  A redundant late-confirmation guard in the page survived its mutant, because `liveUntil = sent + FRESH` already expires it, and was removed.
- **Not covered by a test:** the `service::replay` → `replay::aged` call is exercised only through `aged_at`'s unit test, and the zellij `sample_initial` is exercised only through the fake `Panes::sample`.
- **What is proven about the original incident:** unchanged from above. One plausible layer was found in reading, though not proven. `run_connector` sends the next update only after the host acknowledges the previous one (`in_flight`), so a lost acknowledgement would stop all updates while the connection stays up: a silent freeze of exactly this shape. With this change such a stall can no longer read as current: its panes go unverified within 30 s. Its own repair is left for a follow-up once evidence points there.

### Revision after codex on 40839cf

1. **Atomic transitions.**
   - `Feed::event_at` checks the generation inside the same `publish` lock that applies the frame, so a retired reader can never write.
   - `Feed::commit_sample` replaces the separate validate-then-publish steps. Under one lock it runs `Freshness::done` against the current state, publishes any stall content, retires the stalled generation (`generation += 1`), and builds `confirmed` frames, but only for revisions still current.
   - The supervisor then starts the replacement with `subscribe_in(.., generation)` outside the lock.
   - `publish_sampled` and `confirmed` are removed.
2. **Demand and the bounded confirmation.**
   - The confirming sample is consumed when it starts, and covers only panes still demanded and live; withdrawn or expired demand cancels it.
   - `abandon` (failure) and `timed_out` drop a confirming attempt, so its waiters go back to the regular 15 s budget.
   - In the page, `demandedPane` also requires `#views` to be visible and the shown agent's Terminal view to be `.on`, so a plan, queue or stash page ends demand with an empty `verify`.
3. **Recovery.**
   - `freshness::Retained` carries `live` pane membership (open, not exited, not closed), and sampling admits live panes with no viewport yet.
   - A missing viewport differs from any sample, so a stream that never delivered its first frame is repaired through the same two-sample rule.
   - On resume (`visibilitychange` or `pageshow`), the page reconnects its socket (`resync`), so the host replays every retained screen and the page catches up on missed content, then verifies at once.
4. **Ingress stamping.** The host stamps pane frames as it ingests a connector snapshot (`bridge.rs`, `replay::stamp_snapshot`), as well as on merge. Existing stamps survive unrelated updates, because `stamped` never overwrites one.

**New tests:**
- `web::freshness`:
  - demand withdrawn, or expired, between the first and confirming samples;
  - a failed or timed-out confirming sample not being retried;
  - a pane with no first frame sampled and repaired.
- `web::feed`:
  - a sample overtaken just before its commit publishes nothing;
  - a stall commit retires the stalled reader.
- `clank_web::bridge`: a snapshot pane held by the host replays five minutes older, through the real accept loop and `service::replay`, on the test host clock.
- `check.js`:
  - returning to the page reconnects, and a replayed screen replaces the stale one;
  - a plan page over the Terminal ends demand.
- `web::feed`: a frame landing while the confirming sample runs voids the stall.

**Mutations, each caught:**
- no ingress stamp;
- no retired-generation check in `event_at`;
- no current-revision check in `done` (survived until the confirming-sample interleaving test above was added);
- a confirmation ignoring current demand;
- a failed confirmation kept;
- viewports instead of live membership;
- the page still demanding under a plan page;
- resume without a reconnect (first reported as surviving because a thrown wait aborted the browser case; the wait now records a FAIL).
