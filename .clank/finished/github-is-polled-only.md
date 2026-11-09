# github-is-polled-only
# GitHub watches are polled every 60s by default; the webhook relay is gone

## Why

No GitHub events reached clank from 7 Nov to 10 Nov. PRs #22-#25 were merged by hand because no `ci_passed` ever arrived. Here's how:

1. On 7 Nov at 10:08 a monitor started `gh webhook forward` for `LLFourn/clank`, and was then killed without the forwarder deleting its repo hook.
2. GitHub allows one `cli` forwarding hook per repo. Every later forwarder got "another forwarder holds the webhook slot".
3. The source sat in `degraded` with `"polling": false`, delivering nothing.
4. The TUI's sources row said "monitor not running" although the monitor was up and had published the reason.

LLFourn deleted the orphaned hook (id 708368673) by hand.

GitHub offers no subscription API: there's nothing like a websocket or GraphQL subscription for repo events. Push means webhooks, which need a public endpoint (ruled out by the remote hard gate), or `gh webhook forward`. That's a preview developer extension with a one-hook-per-repo slot and a hook whose lifetime is tied to its process. LLFourn: drop it and poll, default 60s.

CI conclusions are already read by polling: each watched PR head's checks, reduced to one result. Feed events (PRs, comments, reviews) come from the repo events feed, read with ETags. A 304 doesn't count against the rate limit, so 60s polling is cheap.

## Build

1. **Delete the relay.**
   - Remove the `gh webhook forward` forwarder: `github/relay.rs` and its loopback listener, the shared transports and subscriptions, `webhook_event_names`, and the relay health and `degraded` lifecycle states.
   - Remove the realtime cue path in `supervisor.rs`, and everything that only existed to arbitrate the webhook slot.
   - Delete the states, don't leave them unreachable.
   - Configs that still say `"delivery": …` keep loading, and the field stays ignored.
2. **Polling is the only path.**
   - Every GitHub watch polls its feed (with ETag) and its CI reads at the watch's `poll_interval`, default 60s.
   - Keep the existing floor and rate-limit handling:
     - honour GitHub's `X-Poll-Interval` and its rate-limit headers;
     - back off on errors;
     - never stop polling while the source is hosted.
   - A failing poll is reported, with its reason and next retry, never silently dropped.
   - The ingest lease and event log, at-least-once delivery, ack semantics and catch-up on re-arm are unchanged.
3. **Show what's happening.**
   - The TUI's sources row and `clank status` show each GitHub source's real state: polling, the time of the last successful poll, or the last error and its retry.
   - Fix the bug that showed "monitor not running" for a running monitor that had published its source account. Find why `source_status::live` returned `Unavailable` there, and fix that rather than the wording.
4. **A failing source is a bright red error, judged per source kind.** Today every source row's state is drawn dim (`status_tui/render.rs:1170-1173`), so a source that had stopped delivering looked healthy.
   - **One health function.** `source_health(source, record, now) -> Healthy | Failing { why, since }` is shared by the TUI source row and `clank status` text. It's drawn bright red with the reason when failing, and dim otherwise. It appears in the source's own row, with no separate bar alarm (LLFourn).
   - **Each source kind defines its own expected progress.** Health is judged on whether the source *checked*, never on whether events arrived. Quiet repos, 304s, future timers and silent commands are healthy.
     - **GitHub.** Expected progress is a completed poll, 200 or 304, of both the feed and the CI reads. The effective interval is the larger of `poll_interval` (default 60s) and GitHub's `X-Poll-Interval`, plus any backoff currently in force. The source is failing when it has an explicit last error or a rate-limit stop, or when no poll has completed within 3 effective intervals. At startup the clock starts when the source is hosted, so a fresh source isn't failing before its first deadline. A successful poll clears the failure at once.
     - **Timer.** It's healthy while its next fire time is ahead or it fired on time. It's failing only if a due fire wasn't delivered within a grace (for example 2 minutes) after its due time.
     - **Command.** It's healthy while its producer process is running, however quiet. It's failing when the producer has exited unexpectedly or failed to start and isn't being restarted, or after repeated restarts within the existing restart policy.
   - **Recording.** The monitor records each source's last success, last error and the next expected check in the account it already publishes (`source_status`). The health decision reads only that record and `now`.
   - **Fix "monitor not running".** The TUI showed "monitor not running" for a running monitor that had published its source account. Find why `source_status::live` returned `Unavailable` there and fix the cause, with a regression test from the real published account's shape.

5. **`gh` is only a token source (LLFourn).** After this plan, clank runs `gh` in exactly one place: `gh auth token` in `github/session.rs`, and only when neither `GH_TOKEN` nor `GITHUB_TOKEN` is set.
   - `fork.rs`'s `gh_pr_title` (`gh pr view <n> --json title`) moves to the same HTTP session the poller uses (`GET /repos/{owner}/{repo}/pulls/{n}`).
   - It resolves the repo from the source directory's GitHub remote, through `git_io`, as `gh` did by inferring from the working directory.
   - It stays best-effort: any failure falls back to "reviewing PR #N".
   - Add a source gate that fails if any production code spawns `gh` other than `auth token` in `session.rs`.

6. **Docs and skill.**
   - Update the `clank-github` skill text: drop the "Realtime notes" and the webhook relay paragraphs.
   - Update the README and book sections that describe realtime delivery, the forwarder or the `gh webhook` extension.
   - State the 60s default and how to change it (`poll_interval`).

## Tests

No test starts `gh`, the network or a monitor.

- **Polling cadence:**
  - A hosted source polls at its interval with no relay present.
  - A 304 keeps the ETag.
  - An error backs off, is reported, and polling resumes.
- **Config:** a config with `delivery: realtime` loads and polls.
- **TUI:** a running monitor's published account shows the source's polling state, not "monitor not running".
- **Health, on a fake clock:**
  - Healthy waiting: a GitHub source receiving only 304s, a timer before its fire time, and a silent running command all stay healthy.
  - Detection: a GitHub source with no completed poll for 3 effective intervals, including an `X-Poll-Interval` larger than the configured one and a backoff in force, turns failing. A fresh source before its first deadline doesn't.
  - Recovery: an explicit poll error turns the source failing at once, and the next successful poll clears it.
  - A timer that misses its due fire past the grace turns failing. A command whose producer exited and isn't restarting turns failing.
  - The TUI row is red when failing and dim when healthy, and `clank status` text uses the same health function.
- **`source_status::live` regression:** a running monitor's real published account shows the sources' states, not "monitor not running".
- **`gh` only for the token:** the gate passes, and it fails if a `gh pr view` or `gh webhook` spawn is reintroduced. Fork's PR title comes from the HTTP session, against a faked response, and an absent token or network falls back to "reviewing PR #N".
- **No relay left:** a source scan (or the existing ownership gates) confirms no code path spawns `gh webhook`.

## Done when

- **Polling never stops.** A hosted GitHub source checks its feed and CI every effective interval (60s by default), and no state exists in which it stops polling.
- **CI results arrive promptly.** They're read directly from each watched PR's checks, so they reach the agent within about one interval of GitHub reporting them.
- **Feed events are only as timely as GitHub's feed.** PRs, comments and reviews come from GitHub's repository events feed, which GitHub documents as having 30 seconds to 6 hours of latency and bounded history (https://docs.github.com/en/rest/activity/events#list-repository-events). clank's poll adds at most one interval on top. The skill and README say so.
- **The TUI tells you the state.** It shows each source's last successful check. A source that has stopped checking is red in its row.

## As landed

- **Relay removed.** `github/relay.rs` is deleted, along with:
  - the coordinator's cue and relay arm;
  - `classify_webhook` and `webhook_event_at`;
  - the relay-overflow writers;
  - the `Listening` and `Degraded` lifecycles.

  `hyper`, `hyper-util` and `http-body-util` move to dev-dependencies. `Transport::Relay` and `Record::RelayOverflow` stay readable, because old logs contain them. An old `delivery` field loads and is ignored.
- **Polling.** The feed and the CI checks each have their own `Schedule`: retry debt plus a periodic tick of `effective_interval(every, floor)`, where `every` is the longer of `poll_interval` (default 60s) and `X-Poll-Interval`. One half's retries never move the other half's cadence. The monitor publishes `Lifecycle::Polling(PollRecord { since, every, checked, errors: [PollError { half, why, retry_at, … }] })` after each attempt.
- **Health.** One function, `source_status::source_health`, serves the TUI rows and the `clank status` text:
  - GitHub fails on an explicit error, or when no check completes within 3 × (every + backoff), with a startup grace.
  - A timer fails when it's due and undelivered more than 120s past due.
  - A command fails when it's Stopped, or after 3 backoff failures.

  A failing source is bright red in its own row, with each failing half's own retry time. There's no bar alarm.
- **"monitor not running".** The cause was protocol skew. `live_in` now accepts `Published::Other` when the account proves its writer by pid and birth. `Hosted` reads unknown lifecycles leniently, shown as "reported by another clank version". The TUI's idle tick re-reads the account.
- **`gh`.** fork's PR title comes over the HTTP session (`GET /repos/{o}/{r}/pulls/{n}`), with the repo taken from `git_io::origin_url`. The gate `it/gh_spawn_boundary.rs` allows only `gh auth token` in `session.rs`.
- **Docs.** The skill gains "Polling and catch-up", including GitHub's documented 30s–6h feed latency. The README, `book/controller.html` and the `poll_interval` docs are updated.
- **Not covered by a test:** fork's `pr_title` call site and the TUI idle refresh line. `ci_cues_do_not_wait_on_the_own_login` was deleted with the webhook cues it depended on.
