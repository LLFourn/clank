# clank

Multi-agent peer review around plans. One agent (the **master**) writes
a plan; reviewers weigh in; the master implements it commit by commit;
reviewers sign off on each commit; the plan finalizes into a sealed
record.

Everything lives in `.clank/` and `git`. No daemon, no server, no
database — the review history IS the commit history.

```
        master                                    reviewers
          │                                           │
  write   │  .clank/drafts/my-feature.md              │
  queue   │  clank queue add my-feature               │
 promote  │  clank queue promote my-feature ─────────▶│  woken with the intro
          │                                           │
          │◀──────────── CONTINUE / REQUEST_CHANGES ──┤  clank feedback write
          │                                           │
 implement│  git commit -m "[my-feature] …" ─────────▶│  woken with the commit
          │                                           │
          │◀──────────── CONTINUE / REQUEST_CHANGES ──┤  (repeat per commit)
          │                                           │
          │◀──────────────────────────── FINISHED ────┤  gate reviewer
  finish  │  clank finish my-feature                  │
          ▼                                           │
   .clank/finished/my-feature.md
```

Agents are woken when it is their turn — they do not poll, and you do
not chase them.

## The model

- A **plan** is a markdown file under `.clank/plans/`. Committing it
  (tagged `[<plan>] intro`) opens the review cycle. Each repo accepts at most
  one active plan. Finish or stash it before promoting, restoring or importing
  another run. An overlapping introduction made directly in Git is ignored
  with a visible warning; it does not displace the accepted plan.
- The **master** authors and implements plans. **Reviewers** review —
  at one of four tiers:
  - **commit** — reviews every reviewable commit as it lands.
  - **plan** — reviews at the plan stage (the intro / plan revisions).
  - **final** — reviews when the work is believed complete.
  - **gate** — folds into BOTH the plan- and final-stage reviews.
- Verdicts are `CONTINUE` (good, keep going — with a note on what's
  left), `REQUEST_CHANGES` (fix before proceeding), and `FINISHED`
  (the plan is done). When the gate says FINISHED, the master runs
  `clank finish`, which seals the plan (and, with `finish.autosquash`,
  collapses its commits into one).
- Roles are a property of the repo's **roster** (`.clank/config.json`,
  local-only), not per-session flags. A session clank launches is
  bound to its roster label at session start (and rebound after
  `/clear`); a session you start by hand binds with `clank as <label>`.

## Install

```sh
git clone https://github.com/LLFourn/clank
cd clank
cargo install --path crates/cli --locked
```

That puts `clank` and its `clank-web-cloudflare` host process on your
`$PATH`. The TUI starts that sibling executable when you turn on the
remote. Keep `--locked`: without it
`cargo install` ignores the committed `Cargo.lock` and re-resolves
every dependency to the newest semver-compatible version.

Then wire it into your agents (Claude Code, Codex CLI, Grok CLI,
opencode — any or all):

```sh
clank setup
clank doctor      # verify
```

`setup` installs the role-split skills (`clank-master`,
`clank-reviewer`, plus `clank-pr-review`) into `~/.claude/`,
`~/.codex/`, `~/.grok/`, and `~/.config/opencode/`, a `/clank` slash
command for claude, the clank opencode plugin
(`~/.config/opencode/plugin/clank.js` — session binding + work loop),
and merges its Stop and SessionStart hooks into claude's
`settings.json` and codex's `hooks.json`, replacing any entry that
runs `clank stop-hook`. Grok gets a native adapter at
`$GROK_HOME/hooks/clank.json` (default `~/.grok/hooks/clank.json`): a fast Stop
guard and terminal-only Pre/Post hooks protect one background handoff.
For a custom Grok launch profile, use
`clank setup --grok-agent <label> --repo <repo>` to install its hooks and
skills in that profile's home. Setup probes the selected executable once;
automatic launches require the complete adapter and Grok 1.x at least 1.0.46
advertising native PreToolUse/Stop gates. `clank auto off` retains manual operation.
These configuration/capability checks do not prove enabled completion
notifications or a model wake; doctor reports that distinction explicitly.

Re-run `clank setup` after upgrading the binary; `--force` refreshes
skill files you have locally edited.

## Quickstart

Two agents reviewing each other, from nothing. Every block says whose
shell it belongs to — that distinction is the whole mental model.

**1. You — set up the repo and the roster.**

```sh
cd ~/src/my-project
clank init                                   # scaffold .clank/
clank agent add alice --tool claude
clank agent promote alice                    # alice is the master
clank agent add bob --tool codex --review commit
```

(Or seed a whole roster from a saved template: `clank init --team
<name>`; save one with `clank team save <name>`.)

**2. You — launch the team.**

```sh
clank open
```

This is the step that makes the agents exist. It needs
[zellij](https://zellij.dev): outside a session it starts one for the
repo; inside one it adds a tab. Either way the master sits on the
stage with the reviewers stacked beside it, and each pane is a real
agent CLI started in this repo. The arrangement is a layout composed
from the roster: when the roster changes — an agent added, removed or
promoted — the running `clank tui` applies the recomposed
layout to the live tab (zellij 0.45's `override-layout`), so alt+[ /
alt+] flip the CURRENT roster's arrangement, not the one from when
the session was opened. The layout also hands Alt+arrows to the
agents — codex edits its last message on Alt+Up — so in a clank
session focus moves on Alt+h/j/k/l, in every mode.

(`clank agent start <label>` launches ONE agent — resuming its session
if it has one, or starting a fresh one if it doesn't. `clank open` is
what brings up the whole team.)

**3. Each agent is bound as it starts.**

The pane's tool runs with `CLANK_AGENT=<label>` in its environment,
and clank's SessionStart hook binds that label to the session the tool
just created — so work can be routed to it — and rebinds it when
`/clear` hands the process a new session id. Nothing to type. Only a
session you started yourself needs `clank as <label>` (claude and
codex and native Grok; opencode binds through its plugin).

**4. The master — write and promote a plan.**

```sh
$EDITOR .clank/drafts/my-feature.md   # the plan body
clank queue add my-feature            # consumes the draft into the queue
clank queue promote my-feature        # activates it: commits the intro
```

**5. Reviewers — woken automatically, they write verdicts.**

```sh
clank feedback write --commit <sha> --verdict continue \
    --author bob -m "plan is well-scoped; implementation pending"
```

`-m` is required and git-shaped: summary line, then detail. The
verdict becomes the header — don't restate it in the message.

**6. The master — implement, then finalize.**

Each commit is tagged `[my-feature]` and wakes the reviewers again.
When the gate reviewer says FINISHED:

```sh
clank finish my-feature -m "<what changed>" -m "<why>"
```

The plan moves to `.clank/finished/`, and with `finish.autosquash` its
commits collapse into one.

## Watching it happen

```sh
clank status            # one-shot repo state
clank tui               # full-screen: the way you actually drive it
```

The TUI is the primary interface once a team is running: agent rows
with live state (`?` marks an agent whose pane is not open),
per-agent pages (auto-mode, review tier, swap, reopen a closed pane,
remove), the plan and its verdicts, queued plans, and github event
pages. `--watch` gives a live non-fullscreen view; `-j` emits JSON.
Status JSON carries `active_plan` and `last_finished` as objects or `null`. A
finished run carries `duration_secs`, recorded once from its accepted intro to
finalize using Git author timestamps (clock reversals clamp to zero; squashed
runs have the single commit as both boundaries). Log banners show one name per
run across external events and loaded pages, with its recorded duration.
HEAD tag diagnostics carry `unknown` and `extra_named` as a string or `null`;
`untagged_touched` remains an array of actual files touched by the diff.
The latter is shown when no active plan is selected. These replace the former
`plans` and `finished_plans` arrays; `clank open` inspection also reports one
optional `active_plan` name.

Enter selects whatever the cursor is on; every screen shows its own
keys along the bottom. The `remote` row under the agent list is the
remote: Space there (or `r` from the log view) is the switch. In the
default mode, the TUI starts a separate host process on the port the
repo remembers in `.clank/config.json`, attaches this repo to it,
and sends your browser in. Switching off, or closing the TUI, stops
that owned host. When this repo names an existing host instead, the
switch attaches to it and leaves it running when detached. The glyph
beside the zellij one at the bar's right end shows its state from
any page.

To attach to a separately operated host, see the
[custom host example](crates/web/examples/README.md) for the `web.attach`
configuration and the host-side browser and connector credentials. The
[embedding guide](crates/web/README.md) describes the public service,
protocol, authentication, and route contracts.

The default host's page is behind a door, and the key is one token. Enter on the
row opens the remote page, which prints it; paste it into the page
and you are in. It is minted on first use and kept at user level in
`~/.clank/config.json` under `remote.token`, so it is the same token
for every repo's remote on this machine and it survives restarts —
paste it once per device, not once per session. Backspace on the
token row rotates it, which ends every session it opened.

The links make the paste optional rather than necessary: `o` on the
row opens the browser through a one-time link, so this machine never
pastes, and `p` shows that link as a QR for a phone's camera. Each
is good for one use within five minutes. Sessions last thirty days,
hashed in `~/.clank/remote-sessions.json`, and the page lists them;
Backspace revokes one, and a revoked session's stream ends at once.

A phone reaches the default host through a tunnel. With neither a
tunnel nor an existing host configured, the row reads `not configured`
and the switch refuses. Configure the default tunnel once in
`~/.clank/config.json` under `remote.tunnel`:

- `{"provider": "quick"}` — no account, no domain, nothing to
  install. An ephemeral `*.trycloudflare.com` handed out at start,
  spoken natively. Start here.
- `{"provider": "command", "run": ["ssh", "-R",
  "80:localhost:{port}", "nokey@localhost.run"]}` — any binary that
  forwards this port, with `{port}` substituted. Without a `url` the
  hostname is read from what the child prints on either stream,
  which is how the ssh services announce theirs; add
  `"url_contains": "localhost.run"` when it prints a banner or help
  link first. With a `url` (`"https://clank.example.com"`, a named
  cloudflared tunnel) the hostname is yours and is claimed before
  the child runs.

To attach this repo to a host started elsewhere, put this in the
repo's gitignored `.clank/config.json` instead:

```json
"web": {
  "attach": {
    "endpoint": "wss://host.example/connector",
    "session": "team",
    "credential": "connector-secret-from-the-host",
    "browser_url": "https://host.example/team"
  }
}
```

The TUI connects outward using the same session protocol as the
locally started host. Switching it off detaches this repo and leaves
that host running. The host owns browser login and must give the TUI
the connector credential separately; the TUI's token and session
controls are omitted for this mode. Plain `ws` and `http` are accepted
only for loopback hosts.

ngrok has no provider of its own — it wanted an account before
anything worked, and a reserved domain on top of that — but its agent
is a binary that forwards a port like any other:

```json
"tunnel": { "provider": "command",
            "run": ["ngrok", "http", "--url", "https://you.ngrok.app", "{port}"],
            "url": "https://you.ngrok.app" }
```

The `url` is not decoration. The agent writes its endpoint to its own
terminal UI rather than to stdout, so without it clank would wait for
a line that never comes; naming the domain on both sides claims the
endpoint instead of reading one.

A hostname you own is leased before its connector starts, because a
second connector against it would be routed to by the edge before
any teardown could undo it; one handed out at start is leased the
moment it is known. Either way one TUI holds it at a time and a
second is told whose it is. The URL reaches the row only once the
TUI has reached its own remote through it, and switching the remote
off ends the tunnel with it.

## Command reference

Run `clank <cmd> --help` for details; the skills carry the depth.

| Command | What it does |
|---|---|
| `init` | Scaffold `.clank/` (+ `--team <name>` to seed a roster) |
| `setup` | Install skills, hooks + the opencode plugin into `~/.claude`, `~/.codex`, `~/.grok`, `~/.config/opencode` |
| `doctor` | Diagnose repo / user / session setup; exits 1 on FAIL |
| `as` | Bind this agent session to a roster label |
| `auto` | Per-agent auto-mode on / off / status |
| `tui` | The full-screen pane: the way you drive a running team |
| `status` | Repo state; `--watch` live (Enter on a github row opens the event page: preview, ack, open-in-browser), `-j` JSON |
| `wait` | Block for work; `--peek`, `--for`, `--event` (see above) |
| `events` | Event inbox: `list` / `ack` / `show` (see below) |
| `send` | Put a message in an agent's inbox (`--to <label>` or `--master`, `--repo` for another repo) |
| `feedback` | `write` / `read` review feedback via a typed surface |
| `queue` | `add` / `promote` / `remove` / `reprioritise` queued plans |
| `finish` | Finalize a FINISHED plan (`-m` required; autosquash-aware) |
| `unfinish` | Move a finalized plan back to active |
| `agent` | Roster: `add` / `promote` / `remove` / `list` / `set-review` / `start` |
| `team` | Global team templates: `save` / `list` / `show` / `delete` |
| `fork` | Linked worktree with the whole team's sessions forked into it (`--fresh` starts them new instead, orientation prompt intact; it seeds a fork being created, and reopening one never re-seeds) |
| `pr-review` | Multi-agent review loop against a GitHub PR |
| `open` | The team workspace in zellij: a tab inside a session, attach-or-create outside one |
| `log` | Chronological commit + review timeline (`--oneline`, `-j`) |
| `html` | Render the event log + status to a static site; `html open` |
| `diff` | Launch the configured editor on a plan / range diff |
| `stash` | Set a plan's commits aside (bare pushes, like `git stash`) / `pop` restores / `list` |
| `pick` | Copy plans (commits + files) from another branch (`--from`) |
| `purge` | Strip a plan's `.clank/` artifacts from history (`--drop` = all of it) |
| `block` / `unblock` | Ask the human a blocking question / answer it |
| `config` | Read/write typed config values (repo or `--global`) |
| `export` | Dump the repo's roster config as JSON |
| `rewire` | Remap feedback after rebase/amend (installed as a git hook) |
| `stop-hook` | The per-tool Stop-hook adapter (installed by `setup`) |

## Layout

```
<repo>/.clank/
├── plans/                          # active plan — TRACKED
├── finished/                       # finalized plan records — TRACKED
├── queue/                          # queued plan bodies (NNN-<name>.md)
├── drafts/                         # staging area consumed by `queue add`
├── config.json                     # roster + repo config — local-only
├── agents/<label>/
│   ├── config.json                 # auto-mode, session binding, wait_events
│   └── feedback/<sha>.md           # this agent's review notes
├── cache/                          # local fold cache
└── .gitignore                      # allow-lists the tracked paths
```

Only `plans/`, `finished/`, and the managed `.clank/.gitignore` are
committed. Everything else is per-user state.

```
~/.claude/skills/{clank-master,clank-reviewer,clank-pr-review}/
~/.codex/skills/{clank-master,clank-reviewer,clank-pr-review}/
~/.grok/skills/{clank-master,clank-reviewer,clank-pr-review}/
~/.config/opencode/skills/{clank-master,clank-reviewer,clank-pr-review}/
~/.config/opencode/plugin/clank.js  # opencode binding + work-loop plugin
~/.claude/settings.json             # claude Stop + SessionStart hooks merged in
~/.codex/hooks.json                 # codex Stop + SessionStart hooks merged in
~/.grok/hooks/clank.json             # native lifecycle adapter; custom GROK_HOME supported
~/.clank/config.json                # agent library, teams, hooks, defaults
```

## How agents stay awake

`clank wait` blocks until the calling agent has actionable work
(inferring author + role from the session binding). Each tool keeps
its loop differently — `clank setup`'s skills teach this, and
auto-mode drives it:

- **claude** — with a Claude Code that supports `asyncRewake`
  (2.1.223+), `clank setup` installs the ASYNC loop: the Stop hook
  itself parks the long-poll (no armed background task at all — the
  task manager can't reap what doesn't exist) and work WAKES the
  session as a system reminder; a SessionStart companion mints the
  waiter generation and delivers catch-up work after restarts. On
  older installs the legacy loop remains: the hook nudges the agent
  to keep a background `clank wait` armed and the wait's completion
  wake carries the items. Setup decides ONCE per machine and
  `clank doctor` flags drift.
- **codex** — its Stop hook long-polls `clank wait` in-hook and blocks
  with the items (the poll parks until work, the hook-runner
  ceiling, or the hook's own death). Remote chat uses the local Codex
  app-server control socket to start a turn, steer ongoing work, or
  release a completed turn whose Stop hook is still waiting. If that
  socket or Codex's experimental turn-list endpoint is unavailable,
  remote chat falls back to terminal input with a delay and a
  kitty-protocol Enter press. That fallback can leave a submitted
  message queued behind the held Stop hook; use the remote Stop button
  to release it.
- **grok** — native PreToolUse tags one `clank grok-wait` requested with
  `run_terminal_command` and `block_until_ms: 0`; PostToolUse confirms its
  background task before the existing Clank wait can deliver. Background
  completion carries new work turns; Stop only checks or requests bounded
  repair. Existing ordinary waits require an explicit migration choice.
  There is no new idle helper or polling cadence. Corrected fork preparation,
  setup, capability gates and deterministic lifecycle fixtures are implemented;
  authenticated native payload and model wake/reaction acceptance remain
  unverified. See [the integration contract](GROK-INTEGRATION.md).
- **opencode** — the clank plugin long-polls in-hook on
  `session.idle` and injects the items as a new prompt; the agent
  never arms anything. The launch profile carries the model: after
  `clank agent add kimi --tool opencode`, edit the agent's entry in
  `.clank/config.json` (or `~/.clank/config.json` for `--global`):

  ```json
  "kimi": {
    "tool": "opencode",
    "launch": { "args": ["--model", "moonshotai/kimi-k3"] }
  }
  ```

Auto-mode is per-agent (`clank auto on|off|status`), inheriting a
user-global default (`~/.clank/config.json`'s `"auto"`) when unset —
`clank doctor` shows the effective value with its provenance.

## Extra wake sources

### Scheduled and recurring events

An agent can declare a native timer in `.clank/agents/<label>/config.json`:

```json
{
  "wait_events": [
    {"kind":"timer", "name":"check-deploy",
     "due_at":"2026-10-04T09:00:00+11:00",
     "content":"Check the deployment and report the result."}
  ]
}
```

Timers default to one-shot occurrences, with a nonempty name and an absolute RFC3339
deadline containing an explicit UTC offset. Deadlines must be at or after the Unix
epoch, with whole-second precision; `.000` is accepted, nonzero fractions and leap
seconds are refused. Duplicate timer names in one scope are refused. Equivalent
offset spellings identify the same instant.

Add `"repeat_every":"24h"` to repeat on a fixed elapsed-time interval. The duration
uses positive whole seconds: a bare number, `s`, `m`, or `h`; zero, empty values,
fractions and unsupported units are refused. `due_at` anchors the first occurrence;
restart and acknowledgement never reset that phase. This is not cron or a timezone
calendar: 24 hours remains 24 elapsed hours across daylight-saving changes.

Both one-shot and recurring schedules catch up when Clank next arms after offline
time. Recurrence accepts **one** catch-up event for the elapsed slots, even after
years offline. While that event remains unhandled, later ticks coalesce without
new events or scheduled wakes. Reading it or sending a wake receipt is not completion.
Acknowledgement covers all slots through its recorded durable timestamp, including
ticks that elapsed while work was pending; the next deadline is the first anchored
slot strictly after that boundary. An exact-boundary acknowledgement covers that
slot. This avoids an immediate replay storm after completing a long-running task.

CLI/hook hints and log/TUI detail show the accepted snapshot's count, represented
due range and interval. Web event state and JSON retain the complete `recurrence`
metadata: normalized generation descriptor/ID, `first_slot`, `last_slot`, and `count`.
The original content remains lossless. The count is frozen at acceptance; additional
pending ticks are covered by acknowledgement without rewriting the delivered event.

An armed wait wakes at the deadline with a `timer_event` and a qualified selector.
Handle the task, then run `clank events ack <selector>`. Reading or receiving it does
not acknowledge it. Unhandled accepted events survive removing their definition and
restart; acknowledging an occurrence prevents it firing again with the same name and
deadline. Editing the deadline creates another occurrence; changing only its content
does not replace already-accepted work. Removing a future definition cancels it when
the monitor observes the edit. Invalid edits retain the last accepted host and emit
a diagnostic; a valid empty `wait_events` array removes its future schedules.
For recurrence, changing the anchor or interval creates a new generation. Accepted
old-generation work still needs acknowledgement, which cannot suppress the new
generation. Restoring an identical definition resumes after its proven coverage.
Storage does not grow per missed tick: compaction keeps one coverage record per
generation, pending work and bounded recent display history. Removed generations
keep small coverage tombstones so restoring them cannot replay completed work;
the number of distinct historical definitions can therefore grow storage.

Configured timers belong to their named agent across roster role changes. A timer
does not automatically follow whoever becomes master. An invocation-only
`clank wait --event '<timer JSON>'` has a separate identity for each wait call and
applies only to that invocation. `clank wait --peek` reports accepted work without
arming or ingesting a future or overdue definition.

The existing monitor owns one native deadline set per author; the local fallback
owns one per wait. Each waits until its nearest deadline, with no timer subprocess,
task per timer, clock polling, or additional permanent service. The monitor's existing
repository watcher and heartbeat remain. Local/invocation config edits apply on the
next arm. With no monitor or armed wait, a timer cannot launch an agent at its due
time: it catches up when Clank next arms. Clock movement backwards is rechecked before
acceptance; a forward jump is observed at the currently armed monotonic deadline or
next configuration rearm, without periodic clock checks.

Install a timer-capable binary **before** adding timer definitions, on every client
and monitor host. From this checkout use `cargo install --path crates/cli --locked`,
then ensure `command -v clank` resolves to that installation. Existing configurations
need no migration. Recurrence-capable clients use monitor protocol 9 and the existing
exclusive monitor handoff. Deploy the current binary on every client/monitor host
and retire or restart old monitors and waiters **before** enabling recurring
definitions. Updating the binary does not update a process already running it.
Earlier timer-capable binaries may ignore `repeat_every` and treat recurrence as
one-shot, and old writers cannot preserve the new compacted coverage records.
Mixed-version custody and downgrade are unsupported; disable recurrence and retain
its WAL for a supporting binary before downgrading. This feature does not install
or start an agent provider. Native timer sleeps add no polling; the existing bounded
repository watcher uses `notify::PollWatcher`, and its cadence is unchanged.

- `clank wait --peek` — non-blocking, side-effect-free "is there work
  right now?" probe.
- `clank wait --for commit|finished|stopped` — OBSERVE a repo (often a
  foreign one via `--repo`) instead of waiting for your own work.
- **Extra wake sources** (`wait_events` in the agent's config, or
  repeatable `--event '<json>'`): `github` entries watch any repo
  (PRs opened/updated/merged, comments, issues, branch pushes — your
  own actions filtered by default — and CI conclusions,
  `ci_passed` / `ci_failed`), and `command` entries run an argv that
  streams JSONL events (see the reply protocol below). `"prs":[7,9]` scopes a watch to named PRs —
  every kind that names one, comments and updates and merges and CI
  alike; `pr_opened`, issues and branch pushes are outside it. A watch can carry a `"prompt"`
  — operator instructions stamped onto every wake item it produces,
  telling the agent exactly how to react:

  ```json
  { "kind": "github", "repo": "o/r", "events": ["pr_comment"],
    "prompt": "Triage the comment; reply on the PR, then ack." }
  ```

  A watch can instead select by **object**, which scopes an object's
  events together and lets a filter sit on the events it describes:

  ```json
  { "kind": "github", "repo": "o/r", "watch": [
      {"pr": {"numbers": [603], "events": ["review_submitted", "comment"]}},
      {"pr": {"events": ["opened"]}},
      {"branch": {"names": ["main"], "events": ["push"]}} ] }
  ```

  Clauses UNION — an event any clause admits wakes you once — so two
  clauses is how two events get different filters. `numbers` scopes
  every event beneath it, where the flat `prs` leaves `pr_opened` out;
  the shapes therefore mean different things and a watch uses one or
  the other, never both. `review_submitted` is its own event, never
  filtered by `include_own_actions`: submitting a review is the act a
  team waits on, and the login that drafts review comments is usually
  the one that submits them.

  The prompt is presentation config, never stored in the event log:
  editing it retitles the standing intent for already-logged
  unhandled events (per-kind granularity = split the watch). `"delivery":"realtime"` upgrades
  a github source from polling to push-speed webhook delivery, with
  polling kept as the completeness backstop. This is the building
  block for a "controller" repo whose agents manage other repos — the
  `clank-master` skill documents the pattern.

  What the github sources need (`gh` is optional for polling):
  - **Polling** talks to the REST API directly. The token comes from
    `GH_TOKEN` / `GITHUB_TOKEN` if set — no `gh` needed at all — else
    from one `gh auth token` call. With neither, the source waits
    (loudly, fail-closed) rather than polling unauthenticated.
  - **Realtime** requires the `gh` binary, the `cli/gh-webhook`
    extension, and ADMIN on the watched repo (it creates a webhook).
    When any of that is missing the source falls back to polling with
    one diagnostic naming the reason — nothing is lost, wakes are
    just poll-speed.
  - The rest of clank's GitHub surface (`pr-review`, `fork create --pr`)
    still shells `gh` directly.

  Every ingested github event lands in a per-agent **inbox**
  (`.clank/agents/<label>/events/`, a write-ahead log) before it wakes
  anyone, and stays *unhandled* — re-waking the agent on every arm —
  until acked with `clank events ack`. That makes delivery
  at-least-once: events that fire while no wait is armed (agent
  offline, wait killed, the gap before a re-arm) are caught up on the
  next arm by reconciling GitHub's event feed against the inbox. CI
  conclusions are the exception — they are not in that feed, they are
  read from each watched PR's checks, so an arm sees the CURRENT
  result: a build that went red and was replaced by a green one while
  nothing was armed is not recoverable.
  `clank events list` / `show` inspect it; the `clank-github` skill
  teaches agents the react-then-ack loop.

Command producers write one JSON object per stdout line, with an `id` unique
within their source, and optional `content`, `kind`, `at` and `requires_reply`.
Replaying an ID preserves its original event. To ask for an answer, configure
the source with a separate reply handler:

```json
{
  "kind": "command",
  "name": "questions",
  "command": ["question-backend", "subscribe"],
  "reply_command": ["question-backend", "answer"]
}
```

The producer can emit:

```json
{"id":"question/42","content":"Which option?","requires_reply":true}
```

Use the selector printed by `clank events list` or the wake:

```sh
clank events ack command-questions-<hash>@1 --reply 'Use option B.'
```

Plain acknowledgement, including bulk or TUI acknowledgement, refuses an
event requiring a reply. The TUI shows the CLI recipe; it has no reply editor.
`--reply` takes one event selector and a nonempty answer; the supplied text is
preserved verbatim. Ordinary command events can also receive an explicit reply
if they have a handler. Other backends currently refuse `--reply`.

Clank invokes `reply_command` directly with its configured argv, without shell
interpolation or added arguments. Stdin carries one JSON object, then EOF:

```json
{"version":1,"id":"question/42","reply":"Use option B.","author":"codex","idempotency_key":"<opaque stable key>"}
```

The original producer ID and answer can contain Unicode, slashes or newlines.
The handler receives the producer's saved working directory and resolved
`CLANK_SOURCE_STATE_DIR`. These and its argv are captured with the event, so
editing the source configuration cannot redirect an outstanding reply, and an
invocation-only `--event` source remains answerable after its producer ends.
A missing handler leaves a required event owed, with a diagnostic; configure
the handler before emitting such events.

The event log durably stores an immutable reply intent before delivery. Exit
zero accepts the answer; acceptance and acknowledgement share one durable log
record. Failed launch, nonzero exit, blocked stdin or a hanging handler leaves
the event unhandled. The callback and its process-group cleanup have a total
five-second budget, with bounded diagnostics. Reply commands to the same saved
backend are serialized by one nonblocking lease, keeping its callback count
bounded. A concurrent submission is refused. After failure or interruption,
retry explicitly
with the original answer; different text is refused while that intent exists.
`clank events show <selector> --json` includes the saved reply and acceptance.

Callback attempts are **at least once**: a crash after backend acceptance but
before its local record can repeat delivery. The backend must persist and
deduplicate `idempotency_key`, return zero for a previously accepted identical
submission, and refuse conflicting content for that key. The author, key and
text remain the same on retry. Copies sharing the same resolved source key,
producer ID and saved route use one transaction and callback; same-named
sources with different routes remain independent. A recorded acceptance lets
an interrupted fanout finish retiring other copies without another callback.

Replies add no idle worker, watcher or periodic retry. An explicit command
owns delivery and cleanup. A later explicit retry checks any saved callback
owner by kernel process birth identity before ending a surviving process
group; uncertain ownership refuses a new launch and names the record to inspect.
Use updated Clank binaries for all agents, the monitor and the TUI before
enabling required replies; an older client does not enforce this new policy.

Command sources can opt into **wake receipts**, independently of replies. Add
`receipt_command` to an agent's configured `wait_events` source:

```json
{
  "kind": "command",
  "name": "voice",
  "command": ["voice-backend", "events", "--flag"],
  "receipt_command": ["voice-backend", "receipt", "--flag"]
}
```

A receipt means Clank successfully wrote and flushed a hook wake containing the
event. It does not prove that the provider accepted the continuation or that the
model read or completed the instruction. It never acknowledges an event, submits
an answer, or completes backend work. Receipt handlers should record delivery/read
metadata only; for Anx, this must not invoke `anx ack` or `anx say --done`. Unfinished
work remains owed and can still reach a replacement master.

Clank invokes exactly the configured argv, without added arguments or an implicit
shell. Stdin is one JSON object, followed by EOF:

```json
{"version":1,"kind":"wake","source":"voice","author":"codex","ids":["utt/42"]}
```

`ids` contains original producer IDs, preserving slashes, Unicode, quotes and
newlines; these are not the selectors used for event acknowledgement. A wake batches
and deduplicates IDs per source. Exit zero accepts the receipt batch. Handlers must
consume the request before applying effects, be idempotent per source/producer ID,
and keep their children in Clank's owned process group rather than daemonizing.
Clank creates and supplies the source's resolved `CLANK_SOURCE_STATE_DIR`, without
reading its private contents. The working directory is the current presentation
host's: the monitor's cwd for shared waits, and the local wait's cwd for fallback
or peek catch-up. It is not an immutable historical producer directory.

The receipt route is captured from the presenting author's current configuration.
Editing a handler changes later presentations; an in-flight batch keeps its captured
argv/cwd/state directory. Adding a handler also covers an existing source backlog;
removing it disables future receipts without retiring events. Source keys,
subscription arguments and immutable saved reply routes do not change. Omitted
handlers add no receipt callback, WAL reread, runtime coordination or child process.
Invocation-only `--event` receipt handlers are refused; configure the source instead.

Supported presentations are generic Codex, Claude and OpenCode Stop-hook wakes,
and Claude SessionStart's actual additional-context output. Native Grok handoffs and
arbitrary backend/file reads are outside this version. `events list`, `show`, `ack`,
plain `wait`, and `wait --peek` do not notify. Peek includes pending configured source
rows without starting producers; a SessionStart adapter that emits that result owns
the receipt. Failed/partial output, failed flush, stale bindings, diagnostics, control
messages and source-failure rows never produce a receipt.

Receipts are **best effort**, with no idle polling, persistent dispatch queue,
per-event child or retry worker. Across one wake, receipt work retains at most 256
IDs and 64 KiB of encoded stdin, omitting whole IDs that do not fit. These are receipt
cost budgets; they do not truncate or discard the underlying event or wake. At most
one callback runs at a time per hook, sequentially across sources. A nonblocking
per-source lease skips a busy callback rather than queueing behind it. One monotonic
one-second phase budget covers all routes, including cleanup; the last 200 ms are
reserved for cleanup, and later routes receive no fresh deadline. This bounds
software waiting, not OS syscall latency or uninterruptible kernel activity.

A callback failure never replaces the already emitted wake or its exit result.
Clank isolates callback output, kills/reaps its process group including ordinary
descendants, and retains birth-identity evidence when cleanup is uncertain. Another
callback for that source requires safe recovery, attempted only on a later real
presentation. Crashes, overload and failures can miss receipts, and a later wake can
repeat them. There is no exactly-once or durable at-least-once receipt guarantee,
especially after the event is acknowledged.

Diagnostics are bounded to 4 KiB. Codex/OpenCode use diagnostic stderr; Claude's wake
already occupies stderr, so its latest callback diagnostic replaces
`/tmp/clank-run-<uid>/<repo-tag>.wake-receipt-<author>.diagnostic`. The repo tag is the
first 32 hex characters of BLAKE3 over the canonical repo path bytes. This file is
only diagnostic evidence, never acknowledgement, completion or retry authority.
Update the Clank CLI and restart existing monitors before enabling receipt handlers.
An older monitor's missing execution context skips receipts with a diagnostic rather
than guessing a backend directory. No existing source is opted in automatically.

## Troubleshooting

`clank doctor` is the first stop — per-section OK / WARN / FAIL with
actionable messages. A healthy bound session looks like (trimmed from
a real run):

```
[repo]
  OK   .clank/.gitignore: present at …/.clank/.gitignore
  OK   agent: claude: …/config.json: auto_mode=on (per-agent), session=claude bound to …

[user]
  OK   ~/.claude/skills/clank-master/SKILL.md: matches embedded content
  OK   ~/.claude/settings.json: claude Stop hook installed
  OK   ~/.codex/hooks.json: codex Stop hook installed

[session]
  OK   env: running inside claude (session …)
  OK   identity: resolved to `claude` via session binding (last bound …)
  OK   role: inferred role: master
```

`WARN … drifted from embedded content` after an upgrade means the
binary's embedded skills are newer than the installed files — run
`clank setup --force`.

An agent pane that says **`<label>` is already running in zellij
session `…`, tab `…`** found a live pane elsewhere with the same
agent — usually a tab in another session you opened earlier. Close
that pane or use it there; a session has one holder, and a second
resume of it is what the tool itself refuses. A claude agent whose
tab was closed keeps running as a background session; `clank agent
start` notices and `claude attach`es to it instead of resuming, so the
conversation carries on where it was.

**`clank open` could not determine whether a tab/session exists:** the lookup
failed; that does not prove the tab is absent or the server is dead. The error
names the selected session, query argv, exit/timeout and bounded stderr. Check
the failing shell's Zellij session/socket access against the terminal where you
see the tab. Clank does not create tabs after an unanswered lookup. Session/tab
lookups run during the explicit open operation, with a three-second limit each;
inside-session `open --all` shares one tab query. `--print` previews a layout
without querying the server (outside Zellij it previews a fresh session).

`clank fork create` may open the fork automatically inside Zellij before a later
explicit `clank open --fork …`. If automatic opening fails, the error states the
fork's existing path; the fork remains created. A later query failure likewise
does not undo a tab created by an earlier invocation. Inspect the selected
session before retrying or deleting anything.

**Building takes minutes and `cargo test` seems to hang before any
test runs** — on macOS, that is Gatekeeper. `syspolicyd` assesses
every freshly built executable on its first launch, and a big test
binary on a busy machine is a minute of it, at zero CPU in the
process itself. Add your terminal to System Settings → Privacy &
Security → **Developer Tools** (for Terminal.app:
`sudo spctl developer-mode enable-terminal`); everything it spawns is
then exempt, and a fresh binary starts in milliseconds.

The grant follows the *responsible process*, and a zellij server is
re-parented to launchd — so panes inside zellij are not covered by a
grant to the terminal alone. Add `/opt/homebrew/bin/zellij` (⌘⇧G in
the file picker) as well, then restart the sessions; `clank open`
recreates one.
