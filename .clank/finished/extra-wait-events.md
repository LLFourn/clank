# extra-wait-events
# extra wait events: github + command wake sources for clank wait

`clank wait` currently wakes only on this repo's state. Give each
agent EXTRA wake sources — GitHub events across arbitrary repos and
arbitrary commands — so a "controller" repo's agents can manage other
repos: wake on a new PR, triage it with `gh`, coordinate through the
normal clank review flow.

## Config shape (the important part)

Per-agent `config.json` gains an array of wake sources; each entry is
a serde-tagged sum type in `clank_core::agent_config`:

```json
{
  "wait_events": [
    {
      "kind": "github",
      "repo": "frostsnap/frostsnap",
      "events": ["pr_opened", "pr_merged", "pr_comment",
                 "issue_opened", "issue_closed", "issue_comment"],
      "poll_interval": "60s",
      "include_own_actions": false
    },
    { "kind": "github", "repo": "LLFourn/bdk", "events": ["issue_opened"] },
    { "kind": "command", "name": "signal",
      "command": ["signal-cli", "receive", "--timeout", "0"] }
  ]
}
```

- **`github`**: one entry per watched repo (an agent lists as many as
  it likes — nothing is fixed to one repo). `events` are the six MVP
  sub-kinds above. `include_own_actions` defaults FALSE: events
  actored by the authenticated `gh` login are ignored, or the
  controller agent's own PR comment would wake itself in a loop —
  this default is load-bearing for the whole use case. Extra filters
  (labels, authors, branches) are future config surface; the sum type
  makes them additive.
- **`command`**: an ARGV ARRAY (program + args, no implicit shell —
  callers wanting shell write `["sh", "-c", "…"]`; codex cdcf111).
  `clank wait` spawns it when the wait arms; the command COMPLETING
  is the wake (HTTP long-poll, a custom git remote's poller, a Signal
  message — anything). `name` labels the wake item. One-shot per
  wait: each re-armed wait re-spawns it. The child runs in its own
  PROCESS GROUP and the whole group is killed when the wait exits for
  any other reason — a shell wrapper's children must not outlive the
  wait.

CLI: a repeatable `clank wait --event '<json item>'` taking exactly
the config item shape (typed parse — no second grammar to maintain),
merged after the config-sourced entries. A friendlier shorthand can
come later.

## Wake items

Two new `WaitItem` kinds, emitted alongside (or without) repo work,
wake-worthy, with the usual JSON + one-line human renders:

- `github_event { repo, event, number, title, actor, url }` — enough
  to orient; the agent does its own `gh` work from there.
- `command_event { name, exit_code, output_tail }` — `exit_code` is
  null for a signal-killed child (the human line says "signaled");
  `output_tail` is the LAST 1 KiB of interleaved stdout+stderr,
  streamed into a fixed-size ring as the child runs (never
  `output()`-style unbounded buffering), so cheap pollers hand the
  agent a payload without a second hop and a chatty child can't
  balloon memory.

No lifecycle hooks fire for external items (MVP); `--peek` never
spawns sources; the observer path (`--for`) is untouched.

## Runtime shape

**One supervised async loop** (codex cdcf111): the wait loop's beat
moves from the sync `mpsc::recv_timeout` to a single `tokio::select`
over (a) the repo-watch signal (the existing std-mpsc watcher bridged
by a forwarder task), (b) an external-items channel fed by source
tasks, (c) the heartbeat tick, and (d) the `--timeout` deadline. One
beat drains everything ready: repo items and external items arriving
together merge into ONE emitted result (a single `items` array), and
external items never starve repo-state projection or vice versa.
Source tasks are owned by a supervisor that is cancelled AND joined on
EVERY return path — items, timeout (exit 2), and errors — so no
poller or child process outlives the wait; command children die by
process-group kill.

- **github**: a poll task per entry against
  `gh api repos/<repo>/events` with CONDITIONAL requests (store the
  ETag, send `If-None-Match`, treat 304 as "nothing new"), honoring
  the server: effective interval = max(configured,
  `X-Poll-Interval`). Event ids are identifiers, NOT an ordering
  (codex cdcf111), so the cursor is a bounded SEEN-ID SET: the arm-
  time fetch populates it and emits nothing (delta-from-now); each
  poll paginates until a page holds only seen ids; if pagination
  exhausts GitHub's bounded timeline without meeting a seen id, the
  window was OVERRUN — emit what was fetched and note the possible
  gap on stderr (accepted-loss rule, documented). `gh` supplies auth;
  a missing/failing `gh` degrades that SOURCE to a stderr warning,
  never the wait.
- **`pr_comment` covers all three comment classes** (codex cdcf111):
  `IssueCommentEvent` whose issue is a PR, `PullRequestReviewEvent`
  (submitted reviews, including empty-body approvals), and
  `PullRequestReviewCommentEvent` (inline diff comments); the item's
  `detail` names which class. `issue_comment` is `IssueCommentEvent`
  on a non-PR issue.
- **command**: spawned once per wait (argv, own process group), a
  reader task streams output into the 1 KiB ring, wake on exit with
  code-or-signaled.
- Because the codex in-hook poll IS `clank wait`, codex agents get
  these wakes through the existing hook path with zero extra work;
  claude/grok agents get them through their armed background waits.

## Known limits (accepted, stated in docs)

- The source SET is fixed at arm time. Config edits to `wait_events`
  take effect on the next re-arm (waits re-arm constantly in every
  loop; the per-refold reload covers projection inputs, not
  long-lived source tasks).
- GitHub delivery is polling, not webhooks: worst-case latency is the
  poll interval; the events API also has its own server-side delay.
- A command source that exits instantly re-fires every re-arm; that's
  the author's contract to manage (their command should block until
  the event).

## Milestones

- **M1 config + CLI**: the `WaitEventSource` sum type (serde,
  round-trip tests, unknown-kind fail-loud), agent-config plumbing,
  `--event` JSON parse + merge order.
- **M2 command sources**: spawn/kill lifecycle in the wait loop, the
  `command_event` item, in-process tests (a `sh -c 'sleep …; echo'`
  source waking a parked wait; timeout killing the child; `--peek`
  spawning nothing).
- **M3 github sources**: the events-API poller, pure response→item
  mapper (fixture-tested against real API payload samples for all six
  sub-kinds incl. the merged-vs-closed distinction), id-baseline
  dedup, own-actor default filter, per-entry intervals. Poll runner
  injected so tests never touch the network.
- **M4 docs**: skill/README mention of the controller-repo pattern +
  the self-wake filter default.

## Acceptance

- An agent whose config lists a command source has its parked wait
  wake with `command_event` when the command exits (in-process test);
  on timeout the child's PROCESS GROUP is dead before exit 2 returns
  (test with a `sh -c` wrapper spawning a sleeping grandchild); a
  signal-killed child reports `exit_code: null`; a child spewing far
  more than 1 KiB yields exactly the bounded tail.
- Every source task is cancelled and joined on every return path —
  items, timeout, and error (pinned with an injected slow source).
- The github mapper turns recorded `/events` fixtures into exactly the
  configured sub-kinds' items — including ALL THREE `pr_comment`
  payload classes and the merged-vs-closed PR distinction — using
  seen-set dedup (ids treated as unordered), and drops own-actor
  events unless `include_own_actions`.
- Poller behavior with an injected runner: 304 → no items and no
  mapper run; `X-Poll-Interval` larger than the configured interval
  wins; pagination continues to the seen boundary; timeline overrun
  emits fetched items plus the stderr gap note.
- Multiple sources (two repos + a command) merge into one wait without
  starving repo-state wakes, and a beat with both repo and external
  items emits ONE combined result (injected-runner test).
- `--event` accepts the config item JSON verbatim; malformed input is
  a parse error naming the field.
- Config round-trip: unknown `kind` fails loud (config-file lint
  style), absent `wait_events` means none, existing configs parse
  unchanged.
- fmt/clippy at the 18/6 baseline; tests green; no clank-binary
  spawning (git/sh fixtures fine); no network in tests.
