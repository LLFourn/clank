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
  (tagged `[<plan>] intro`) opens the review cycle.
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
  local-only), not per-session flags. Agents bind their session to a
  roster label once with `clank as <label>`.

## Install

```sh
git clone https://github.com/LLFourn/clank
cd clank
cargo install --path crates/cli --locked
```

That puts `clank` on your `$PATH`. Keep `--locked`: without it
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
and tag-merges a Stop hook entry into claude's `settings.json` and
codex's `hooks.json` (grok has no active hooks — its skill carries the
work loop).

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
agent CLI started in this repo.

(`clank agent start <label>` launches ONE agent — resuming its session
if it has one, or bootstrapping it with a seed prompt to bind if it
doesn't. `clank open` is what brings up the whole team.)

**3. Each agent — bind once, in its own pane.**

```sh
clank as alice        # in alice's pane
clank as bob          # in bob's pane
```

The binding is what tells clank which roster entry this session is, so
work can be routed to it. The installed skills teach the agents to do
this themselves on first turn.

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
clank status --tui      # full-screen: the way you actually drive it
```

The TUI is the primary interface once a team is running: agent rows
with live state, per-agent pages (auto-mode, review tier, swap,
reopen a closed pane, remove), the plan and its verdicts, queued
plans, and github event pages. `--watch` gives a live non-fullscreen view; `-j` emits JSON.

Enter selects whatever the cursor is on; every screen shows its own
keys along the bottom.

## Command reference

Run `clank <cmd> --help` for details; the skills carry the depth.

| Command | What it does |
|---|---|
| `init` | Scaffold `.clank/` (+ `--team <name>` to seed a roster) |
| `setup` | Install skills, hooks + the opencode plugin into `~/.claude`, `~/.codex`, `~/.grok`, `~/.config/opencode` |
| `doctor` | Diagnose repo / user / session setup; exits 1 on FAIL |
| `as` | Bind this agent session to a roster label |
| `auto` | Per-agent auto-mode on / off / status |
| `status` | Repo state; `--watch` live, `--tui` full-screen pane (Enter on a github row opens the event page: preview, ack, open-in-browser), `-j` JSON |
| `wait` | Block for work; `--peek`, `--for`, `--event` (see above) |
| `events` | Github event inbox: `list` / `ack` / `show` (see below) |
| `feedback` | `write` / `read` review feedback via a typed surface |
| `queue` | `add` / `promote` / `remove` / `reprioritise` queued plans |
| `finish` | Finalize a FINISHED plan (`-m` required; autosquash-aware) |
| `unfinish` | Move a finalized plan back to active |
| `agent` | Roster: `add` / `promote` / `remove` / `list` / `set-review` / `start` |
| `team` | Global team templates: `save` / `list` / `show` / `delete` |
| `fork` | Linked worktree with the whole team's sessions forked into it |
| `pr-review` | Multi-agent review loop against a GitHub PR |
| `open` | The team workspace in zellij: a tab inside a session, attach-or-create outside one |
| `log` | Chronological commit + review timeline (`--oneline`, `-j`) |
| `html` | Render the event log + status to a static site; `html open` |
| `diff` | Launch the configured editor on a plan / range diff |
| `stash` | Set a plan's commits aside / restore (`push --to-queue` re-queues) |
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
├── plans/                          # active plans — TRACKED
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
~/.claude/settings.json             # claude Stop hook tag-merged in
~/.codex/hooks.json                 # codex Stop hook tag-merged in
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
  ceiling, or the hook's own death).
- **grok** — has no active hooks; its skill (and the auto-on launch
  prompt from `clank agent start`) teach it to arm the background
  wait itself.
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

- `clank wait --peek` — non-blocking, side-effect-free "is there work
  right now?" probe.
- `clank wait --for commit|finished|stopped` — OBSERVE a repo (often a
  foreign one via `--repo`) instead of waiting for your own work.
- **Extra wake sources** (`wait_events` in the agent's config, or
  repeatable `--event '<json>'`): `github` entries watch any repo
  (PRs opened/updated/merged, comments, issues, branch pushes — your
  own actions filtered by default), and `command` entries spawn an
  argv whose completion is the wake. A watch can carry a `"prompt"`
  — operator instructions stamped onto every wake item it produces,
  telling the agent exactly how to react:

  ```json
  { "kind": "github", "repo": "o/r", "events": ["pr_comment"],
    "prompt": "Triage the comment; reply on the PR, then ack." }
  ```

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
  next arm by reconciling GitHub's event feed against the inbox.
  `clank events list` / `show` inspect it; the `clank-github` skill
  teaches agents the react-then-ack loop.

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
