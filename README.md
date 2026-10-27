# clank

Multi-agent peer review around plans. One agent (the **master**) writes
a plan; reviewers weigh in; the master implements the complete plan in one
commit by default; reviewers sign off; the plan finalizes into a sealed record.

**New here? Read [the clank book](book/index.html)**: the ideas, the three ways to
use clank, and real recordings. Open `book/index.html` in a browser; it works
offline. This README is the reference.

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
          │◀──────────── CONTINUE / REQUEST_CHANGES ──┤  review corrections
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
- Implement a plan in one complete commit unless it specifies internal named
  milestones with a strong justification for their review boundaries. Do not add
  milestones merely to excuse incremental implementation. Reviewers must request
  changes for unjustified milestone plans or partial implementation without such
  milestones. Promotion, finalization and requested review corrections remain
  separate steps; every commit is still followed by stopping for review.
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
`clank-reviewer`, `clank-standby`, plus `clank-github`) into `~/.claude/`,
`~/.codex/`, `~/.grok/`, and `~/.config/opencode/`, a `/clank` slash
command for claude, the clank opencode plugin
(`~/.config/opencode/plugin/clank.js` — session binding + work loop),
and merges its Stop and SessionStart hooks into claude's
`settings.json` and codex's `hooks.json`, replacing any entry that
runs `clank stop-hook`. Grok gets a native adapter at
`$GROK_HOME/hooks/clank.json` (default `~/.grok/hooks/clank.json`): SessionStart
binds the session, Stop requests one ordinary background wait, and SessionEnd
releases that session's binding. Setup removes the retired Clank-owned tool hooks
while preserving unrelated user hooks.
For a custom Grok launch profile, use
`clank setup --grok-agent <label> --repo <repo>` to install its hooks and
skills in that profile's home. Setup probes the selected executable once;
automatic launches require the complete adapter and Grok 1.x at least 1.0.46
advertising the native Stop decision gate. `clank auto off` retains manual operation.
These configuration/capability checks do not prove enabled completion
notifications or a model wake; doctor reports that distinction explicitly.

Managed agent launches inspect and repair missing or older setup-owned integration
for the provider they actually launch. This includes profile-specific HOME and
configuration directories, and an existing global OpenCode plugin loaded alongside
a custom profile. Locally edited integration files are preserved and reported;
launch does not force an overwrite. Preview commands remain read-only. Repair runs
at launch, with no idle polling or setup daemon.

Claude launches also record workspace trust in the selected profile's user
configuration before execution: `projects["<main checkout>"].hasTrustDialogAccepted`
in `~/.claude.json`, or `$CLAUDE_CONFIG_DIR/.claude.json` for a custom profile.
This follows [Claude's documented workspace trust model](https://code.claude.com/docs/en/permissions#project-allow-rules-and-workspace-trust);
linked worktrees share the main checkout's trust. Fresh starts, resumes and forks
use the selected repository as their actual working directory. Configuration is
merged under Claude's native write lock, preserving other settings and tool
permissions. A failed trust write stops launch with a retryable error; previews
do not record trust or consume fork intent.

`clank doctor` follows the configured launch profiles and reports their actual
asset paths. It probes the selected Claude executable for hook mode and the
selected Grok executable for native capabilities. Diagnosis does not install
anything; tools without a configured profile retain default user-scope checks.

Re-run `clank setup` after upgrading the binary; `--force` refreshes
skill files you have locally edited.

## Quickstart

Two agents reviewing each other, from nothing. Every block says whose
shell it belongs to — that distinction is the whole mental model.

**1. You — set up the repo and the roster.**

```sh
cd ~/src/my-project
clank init                                   # scaffold .clank/
clank agent add alice --tool claude          # the first agent is the master
clank agent add bob --tool codex             # later ones review commits
```

(Or seed a whole roster from a saved template: `clank init --team
<name>`; save one with `clank team save <name>`. A template saved as
`default` seeds every bare `clank init` of a repo with no roster.)

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

If the repo is already open in any zellij session on the machine,
`clank open` says where instead (the session, the tab, and how to get
there from where you are) and opens nothing. A tab or session that only
shares the repo's name is not it: the repo opens beside it, under a name
of its own.

(`clank agent start <label>` launches ONE agent — resuming its session
if it has one, or starting a fresh one if it doesn't. `clank open` is
what brings up the whole team.)

**A pane is named by its agent, and that name is its identity.** Each
agent pane is titled with its label alone; the TUI puts the agent's
status glyph in front (`👀 codex`) and changes only the glyph as its
state or role changes. Clank finds an agent's pane by that name, not by
the command it runs, so a pane you start by hand, named for the agent,
is that agent's pane. Rename one by hand and clank believes you.

Everything clank types into an agent (`clank nudge`, an interrupting
event, recovery's nudges and retries, attendance check-ins, user
commands) goes to the one live pane with that name in the repo's
workspace, as the tool the roster gives the agent. No such pane, two of
them, or a pane whose name could be read as two agents means nothing is
typed; once recovery's nudge has met that, `clank status` says which. Whether the tool may be typed into
unattended is decided by the tool alone: Claude and Codex are, Grok never
is, and OpenCode is resumed through its plugin. `clank nudge` types into
all four.

The TUI learns that an agent's pane exited at its next listing of the
panes: when you open the agents panel or an agent's page, when an
agent's status glyph changes, or when the roster changes. A quiet TUI
can show a pane that has gone until one of those happens.

The name only means something inside the repo's **workspace**: the tab
that holds the repo's TUI pane (`clank tui --repo <path>`), in whatever
session that tab is. Two repos in one session can both have a `codex`;
each sees only its own. A repo with agent panes but no TUI pane open has
no workspace, so nothing finds, nudges or reconciles its agents until
its TUI is open again. Looking across sessions, a workspace counts only
when every session answered and exactly one TUI pane was found.

Clank remembers which labels it owns panes for (the runtime record
`pane-owners.json`, kept across TUI restarts): the panes it opened, the
roster's panes it saw, and the pane `clank as` renamed. So an agent
removed from the roster still has its pane recognised and closed. A
pane whose name matches nothing clank owns, such as a shell or an
editor, is never closed; when the record is missing the TUI lists such
panes as `not on roster, not closed` and leaves them for you. A pane
whose name could be read as two agents (`bob` and `💤 bob` both on the
roster) is refused rather than guessed.

**3. Each agent is bound as it starts.**

The pane's tool runs with `CLANK_AGENT=<label>` in its environment,
and clank's SessionStart hook binds that label to the session the tool
just created — so work can be routed to it — and rebinds it when
`/clear` hands the process a new session id. OpenCode has no such
hook, so clank chooses an OpenCode session's id itself, records the
binding, and launches it with `--session <id>`; a forked OpenCode pane,
whose id OpenCode chooses, is asked to bind itself. Nothing to type.
Only a session you started yourself needs `clank as <label>`. Inside the
repo's workspace, `clank as` also renames its own pane to the label, so
clank can find it. It refuses, before binding anything, when another
live pane there already has that name; when the workspace cannot be
determined it binds and says the pane was not renamed; a rename zellij
refuses leaves the binding and exits non-zero. A renamed pane is that
agent's pane like any other.

From then on the session's binding is its identity, for every hook and
every clank command it runs. `CLANK_AGENT` only binds a session that
has no binding yet, and only from the hook of the process clank
launched. A Codex hook under the shared app-server daemon can't count:
it carries whatever the daemon inherited from another pane. Outside
any agent session, `CLANK_AGENT=<label> clank …` names you directly.

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

Implement everything the plan specifies in one commit tagged `[my-feature]`,
then stop for review. Use separate implementation commits only for strongly
justified internal named milestones already specified in the plan. Address any
requested review correction and stop again. When the gate reviewer says FINISHED:

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
pages. Under the queue, one `drafts: N` row lists `.clank/drafts/`; a draft's
page reads it and queues it (`e`) or queues and promotes it (`p`), with the
same checks as `clank queue add` and `clank queue promote`. Unqueue on a queued
plan's page sends it back there. The remote page shows the same count.
Above the log, a SOURCES region lists each agent's configured command, timer
and GitHub event sources, with the icon their events carry and what the monitor
says each is doing: running with its pid, backing off after an exit, following
another host's log, or not hosted. Without a running monitor it says so rather
than show a stale pid. Enter opens a source's page:
- its configuration, read-only;
- its events alone, newest first, each opening its event page;
- for a command source, `r` to restart its producer behind a confirm.

A restart is a request to the monitor. It stops only that source's whole
process group and starts it again, and is refused if the configuration it runs
has changed since the page showed it. When an edited source configuration was
rejected, the page says the monitor still runs the earlier one.
`--watch` gives a live non-fullscreen view; `-j` emits JSON.
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
remote: Space there (or `r` from the log view) is the switch. From a
shell, `clank remote on|off` asks the repo's running `clank tui` to do
the same, and `clank remote status` shows its state and address; on
while on and off while off change nothing. A `clank tui` started
before this existed cannot take these requests until it restarts. In the
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
| `auto` | Per-agent auto-mode: `on` / `off` / `status` |
| `tui` | The full-screen pane: the way you drive a running team (Enter on a github row opens the event page: preview, ack, open-in-browser; Enter on a block's question or its agent's row opens the block page to answer it) |
| `status` | Repo state; `--watch` re-prints it as it changes, `-j` JSON |
| `wait` | Block for work; `--peek`, `--for`, `--event` (see above) |
| `run` | Run a long command under clank, so clank knows it is in hand (see below) |
| `attend` | Tell clank which background work you are waiting on: a subagent by name, or a running task by id and `--pid` (see below) |
| `events` | Event inbox: `list` / `show` (metadata only) / `read` (content, recording delivery) / `ack` / `reply` (see below) |
| `send` | Put a message in an agent's inbox (`--to <label>` or `--master`, `--repo` for another repo; `--interrupt` also tries to interrupt) |
| `nudge` | Interrupt an agent and type a message into its terminal, for when `send` cannot be delivered |
| `user-command` | Deliver a person's site-signed command into the asking agent's terminal as `! <command>`: `trust-key` / `untrust-key` / `keys` / `submit` / `status` (see below) |
| `feedback` | `write` / `read` review feedback via a typed surface |
| `queue` | `add` / `promote` / `remove` / `unqueue` (back to drafts) / `reprioritise` queued plans |
| `finish` | Finalize a FINISHED plan (`-m` required; autosquash-aware) |
| `unfinish` | Move a finalized plan back to active |
| `rereview` | Make every reviewer review a plan's latest commit again |
| `agent` | Roster: `add` / `make-master` / `remove` / `list` / `set-review` / `swap` / `launch` / `start` |
| `team` | Your saved teams (`save` / `list` / `show` / `remove`) and your agent library (`agent add` / `agent remove` / `agent list`), shared across repos |
| `fork` | `create` a linked worktree with the whole team's sessions forked into it (`--fresh` starts them new instead, orientation prompt intact; it seeds a fork being created, and reopening one never re-seeds); `list` / `remove` |
| `open` | The team workspace in zellij: a tab inside a session, attach-or-create outside one |
| `remote` | This repo's remote, through the `clank tui` that owns it: `status` / `on` / `off` |
| `log` | Chronological commit + review timeline (`--oneline`, `-j`); `--diagnostics` inspects history reconstruction issues |
| `html` | Render the event log + status to a static site; `html open` |
| `diff` | Launch the configured editor on a plan / range diff |
| `stash` | Set a plan's commits aside (bare or `push`, like `git stash`) / `pop` restores / `show` / `drop` / `list` |
| `pick` | Copy plans (commits + files) from another branch (`--from`) |
| `purge` | Strip a plan's `.clank/` artifacts from history (`--drop` = all of it) |
| `block` | Questions only the human can answer: an agent's `block ask`, the human's `block answer`, and `block clean` to tidy away delivered ones. The wake carrying an answer is its receipt; the agent acknowledges nothing |
| `config` | Read/write typed config values (repo or `--global`): `review.adhoc_feedback`, `review.plan_feedback`, `review.require_commit_prefix`, `hooks.blocked`, `diff.editor.command`, `diff.wait`, `finish.autosquash`, `recovery.blind_nudge` |
| `export` | Dump the repo's roster config as JSON |

Some commands are hidden from `clank --help` because clank runs them
itself or they exist for one-off upgrades. They still work:

- `stop-hook` and `question-hook` are the tool hooks `clank setup` installs.
- `rewire --from-stdin` is the `post-rewrite` git hook `clank init` installs;
  it remaps feedback after a rebase or amend.
- `monitor start` runs the per-repo watcher that every wait and TUI shares.
  A parked wait starts one when none is running, and an open `clank tui`
  keeps one up: when its monitor retires or exits, the TUI starts the
  successor, retrying with a backoff while another client, a held lease or
  a newer monitor stands in the way. `clank status --watch` only subscribes
  and never starts one. `monitor upgrade` replaces monitors from
  builds that predate self-retirement; run it once after upgrading from one.
- `block migrate` flattens the legacy plan-scoped block layout.
- `open dry <path> [--json]` reports what `clank open` would do for a path
  without opening anything; `open-worktree.sh` reads its JSON.

Older spellings still work, hidden: `block create` is `block ask`, `unblock`
is `block answer`, `agent add --global --tool` and `agent remove --global`
are `team agent add` and `team agent remove`, `open zellij` is `open`, `agent promote` is `agent make-master`, `team
delete` is `team remove`, `attending` is `attend`, `--dry` is `--dry-run`
and `html open --print-path` is `--print`.

### Messages: send, interrupt, nudge

Three levels, for three situations:

- `clank send` writes a durable event to the agent's inbox. It wakes the
  agent until the agent acknowledges it with `clank events ack`.
- `clank send --interrupt` writes the same durable event and also tries to
  interrupt the agent's current turn. If the interrupt cannot be made, the
  event still waits in the inbox.
- `clank nudge` is the recovery route for when the inbox cannot reach the
  agent at all: a wedged hook, a stale binding, a wake that never fires. It
  interrupts the agent's pane and types the message into its terminal,
  reading no inbox and no binding. It is deliberately blunt: it does not
  read the pane first, works for every tool, and submits whatever is
  already in the input along with your message. Nothing waits to be
  acknowledged, but each attempt and its outcome is kept in the agent's
  nudge history.

### User commands

Agents reach actions their permission checks deny but you are happy to do
yourself: pushing a branch, opening a PR. The story site shows such a
command with a Run button. Pressing it does not run anything in the site:
clank types `!<command>` into the terminal of the agent that asked, Claude
Code's bash mode runs it as your own input, and the output lands in that
agent's conversation. Bash mode skips the agent's permission checks, so a
`!` line reaches a prompt only because a person chose it.

**Interface.** Each site holds its own Ed25519 private key; clank holds
only the public keys, each under the site's name, and verifies, never signs.

- `clank user-command trust-key --name <site> [--replace]` reads a public
  key (standard base64 of the 32 raw bytes) from stdin, stores it as
  `user_command.keys.<site>` in `~/.clank/config.json`, and prints its
  blake3 fingerprint. Replacing that name's different key needs
  `--replace`. A key already trusted under another name is refused: one key
  has one name, so which name admitted a request never depends on search
  order (a hand-edited config holding one key twice trusts nothing until it
  is fixed). A name is 1 to 64 of `[A-Za-z0-9_-]`.
- `clank user-command untrust-key --name <site>` removes it and prints its
  fingerprint; `clank user-command keys` lists each name and fingerprint,
  never a key.
- A config from before keys had names (`user_command.site_key`) is read as
  the key named `default`, and the next `trust-key` or `untrust-key` moves
  it into `keys` and drops `site_key`, so untrusting `default` cannot bring
  it back. If `site_key` and a different `keys.default` are both present,
  nothing is trusted until `trust-key --name default --replace` or
  `untrust-key --name default` decides.
- `clank user-command submit` reads one JSON envelope from stdin, never
  argv: `{"request": <base64 of the payload bytes>, "sig": <base64
  signature>}`. The signature is over the bytes `clank user-command v1\n`
  followed by exactly the payload bytes, and is verified before anything is
  parsed from them. The payload is
  `{v: 1, repo, agent, session, tool, id, command, issued_at, expires_at}`:
  `repo` the repo's canonical path, `agent` a roster label, `session` and
  `tool` the agent's current binding (`tool` must be `claude`), `id` 1 to
  64 of `[A-Za-z0-9_-]`, `command` one non-empty line with no control
  characters, and the times unix seconds at most 600 apart (30 s of clock
  skew either side). Every field is signed. It is admitted under whichever
  trusted key verifies it.
- `clank user-command status --repo <r> --agent <a> --id <id>`.

Both `submit` and `status` print `{id, state, reason, admitted_at,
updated_at, key, key_name}` (the fingerprint and name it was admitted
under), never the envelope, the signature or the command. A repeat submit
keeps that name and fingerprint, even signed by another trusted key. A refused `submit` prints
`{"state": "refused", "reason": …}` and exits 1, and files nothing.

**States.** `admitted` → `waiting-for-idle` (with a reason) → `dispatching`
→ `delivered` | `unconfirmed` | `refused` | `failed-not-started`. The last
four are terminal and are never rewritten, whatever later happens to the
key or the clock. `delivered` means the terminal accepted the `!` line and
Enter, not that the command succeeded. `unconfirmed` means something may
have been typed; it is never retried. Every terminal outcome is added to
the agent's nudge history.

**At most once.** The same id with byte-identical payload returns the
existing state (and has the monitor look again at a waiting one); the same
id with any other payload is refused and changes nothing. `dispatching` is
synced to disk before the first byte is written, under the request's lock;
a reader that later finds it with the lock free records `unconfirmed`.

**Where it is typed.** Only the repo's monitor dispatches, on its existing
wakes (turn-ends, record reads, the store's own files changing, a waiting
request's expiry), never by polling a pane. Before any use it verifies the
stored envelope again, within its window, under the key trusted now under
the name it was admitted under, and no other: if that name is gone or holds
a different key, the request is refused with `key changed`, and so is
every admitted, undelivered request it signed. It checks the envelope
names the repo, agent and id it is filed under. It types only when the
agent's latest turn completed, no question, block or presentation is
pending, no hook is held, the session binding still matches, the pane's
launch is the one first found, and one dump reads Claude's input as an
empty prompt. An agent parked in its Stop hook is released first (the turn
ends instead of parking) and looked at again a moment later. Each attempt
holds the request lock, then the pane's write lock, reads the input, and
only then asks the monitor whether the agent is still idle, takes the
label's `binding.lock` and verifies the envelope, key, clock, binding and
launch again before it records `dispatching` and types. The monitor's own
pane writers give way to a reserved pane rather than wait. A person typing between that dump and the write can mix with
the command; the outcome is then `delivered` or `unconfirmed`, never
retried.

**Tools.** Claude Code only. Codex has a shell mode but no verified
rendering of it, and Grok and OpenCode have shell modes but no readable
input line; all three are refused with that reason.

**What it does not protect.** Every site and every agent run as the same
Unix user, so this is not an OS boundary. An agent cannot forge a request,
and a captured envelope authorises exactly one command, to one target,
under one id, once. Names are labels, not isolation: each trusted key is a
separate capability that can sign for any agent in any repo, and guarding
one site's key does nothing for another's. Any of these defeats it:

- reading any trusted site's private key file;
- writing `~/.clank/config.json`, or running `trust-key` with a key of its
  own (under a new name, or `--replace` on an existing one);
- running `untrust-key`, which refuses that site's pending requests (a
  denial, not a forgery);
- modifying or deleting the ledger under `~/.clank/user-commands/`, which
  defeats replay prevention: a genuine envelope whose entry is gone or reset
  can be dispatched again until it expires.

Add deny rules for these to every agent (Claude Code's `permissions.deny`
in `~/.claude/settings.json`), with a `Read` and an `Edit` rule for EACH
site's private-key path; the two below are examples, adjust them to where
each site keeps its key:

```json
{
  "permissions": {
    "deny": [
      "Read(~/.config/clankstack/site-a/site-key*)",
      "Edit(~/.config/clankstack/site-a/site-key*)",
      "Read(~/.config/clankstack/site-b/site-key*)",
      "Edit(~/.config/clankstack/site-b/site-key*)",
      "Edit(~/.clank/config.json)",
      "Write(~/.clank/config.json)",
      "Edit(~/.clank/user-commands/**)",
      "Write(~/.clank/user-commands/**)",
      "Bash(clank user-command trust-key:*)",
      "Bash(clank user-command untrust-key:*)",
      "Bash(rm ~/.clank/user-commands:*)"
    ]
  }
}
```

Deny rules match tool calls, so they cover `Read`/`Edit`/`Write` of those
paths and the obvious Bash spellings, not every Bash path to the same
bytes.

**Ordinary text cannot reach a shell mode.** Every other pane write
(`send --interrupt`, recovery, check-ins, the web chat) goes through one
admission rule: CR becomes LF, Tab a space, other control characters
U+FFFD, and a leading `!` gets `·` before it. It then types only when one
dump reads the input as an ordinary prompt (Claude, Codex). Grok's and
OpenCode's input cannot be read, so their panes are never typed into.
Otherwise it is refused and an event wake stays pending. Codex's
app-server and OpenCode's plugin take the same admitted text. `clank
nudge` neutralises the same characters but skips the screen check.

**Evidence a human records.** User commands are gated on an observation
made in a throwaway pane against the installed harness, and stay refused
until it is committed as `USER_COMMAND_EVIDENCE` in
`crates/cli/src/cli/pane_input.rs`: Claude Code runs a typed `!<line>`
plus Enter as one bash command (quotes, `$`, a line at the chosen
`max_command`), what its bash mode looks like in a `zellij action
dump-screen --ansi` dump, and whether a bash-mode command fires a Stop
hook. Without it every request is refused at dispatch.

### Background work: run and attend

An agent that starts long work in the background (a test suite, a build,
a subagent) would otherwise be woken at each turn-end with work it is
already doing. To attend the work keeps work the agent has already been
shown from waking it again while the work is listed running, and only where
the harness wakes the agent itself when the work completes (Claude). New
work still wakes it.

- `clank attend "<name>"` attends a subagent by its name.
- `clank run --desc "two words" -- <command>` is the way for shell work.
  Clank asks to attend the run, then becomes the command (same pid, stdio
  and exit status), so the task's lifetime is the work's lifetime.
- `clank attend <task-id> --desc "two words" --pid <pid>` attends shell
  work that is already running. The task id is the harness's own, and the
  live `--pid` is what lets clank tell the work is still running.

The request takes effect from the next turn-end, whose list of running
work must show exactly one match. Each takes `--expect` (default 5m): if
the work is still running past that, the agent is checked in on once,
typed into its pane. Nothing is cancelled and nothing times out. The
harness's own notification that the task finished is still what wakes the
agent when it ends. `clank attend --clear` withdraws it.

Status shows a commit warning only when it belongs to HEAD and amending that
commit can repair it, such as introducing another plan while one is active or
introducing several plans in one commit. Historical ignored introductions remain
part of replay's provenance, but do not clutter CLI or TUI status. Warning labels
and wrapped text use the terminal's warning color independently of the state bar.

Use `clank log --diagnostics` to inspect reconstruction evidence, including the
responsible commit and reason, even after later successful work retires that
evidence from current workflow state. This explicit view replaces the timeline;
`--json` emits typed records, and the usual range, `-n` and `--plan` filters apply.
Use `-n 0` for the whole first-parent history. No history rewrite or cache migration
is needed, and ordinary log output remains unchanged. Live operational failures
and HEAD tag corrections keep their existing status surfaces.

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
~/.claude/skills/{clank-master,clank-reviewer,clank-standby,clank-github}/
~/.codex/skills/{clank-master,clank-reviewer,clank-standby,clank-github}/
~/.grok/skills/{clank-master,clank-reviewer,clank-standby,clank-github}/
~/.config/opencode/skills/{clank-master,clank-reviewer,clank-standby,clank-github}/
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
- **grok** — native SessionStart binds the session; Stop checks the native
  background-task snapshot and requests one ordinary `clank wait` using
  `run_terminal_command` and `block_until_ms: 0` when needed. The wait connects
  directly to the shared monitor with the existing agent and session identity.
  The monitor rejects duplicate automatic waits; no tool arguments are rewritten.
  Grok's background completion carries the next work turn. An idle wait adds no
  helper, watcher or polling cadence. Setup migrates existing Clank-owned hooks
  while preserving user hooks. Deterministic launch fixtures verify the wiring;
  authenticated model completion acceptance remains a separate live check. See
  [the integration contract](GROK-INTEGRATION.md).
- **opencode** (2.x) — the clank plugin long-polls in-hook when a
  session's execution succeeds and prompts the items into it; the agent
  never arms anything. One OpenCode service hosts every session and
  directory, and the plugin runs one loop per session, in that session's
  own directory. `clank doctor` says when a bound session has finished a
  turn but the plugin never reported it — usually an OpenCode that did not
  load the plugin. Launch
  repair also upgrades an existing setup-owned global copy, preserving local
  edits through the usual non-force ownership checks. The launch profile carries the model:
  `clank agent add kimi --tool opencode -- --model moonshotai/kimi-k3`.

### An agent's launch

Everything after `--` on `clank agent add` is the tool's own arguments, typed
exactly as you would type them to the tool, quoted once by your shell. `--env
KEY=VAL` and `--cmd <executable>` go before the `--`:

```sh
clank agent add web --tool codex --review standby \
  --env CODEX_HOME=$HOME/.codex-web -- --no-daemon
```

`clank agent launch <name>` shows an agent's launch profile and the command
`clank agent start` would run, even while the agent is running. Given `--cmd`,
`--env` or `-- <args>`, what you write is the agent's whole new launch, with
nothing kept from before; `--clear` goes back to the bare tool. The agent keeps
its role, session and pane, and the change applies from its next start.

clank always starts Codex with `--no-daemon`, adding it when the launch doesn't
already have it. Codex's shared app-server daemon runs every attached session's
hooks and shell commands in the environment of the pane that first started it,
so a session hosted there reads as that pane's agent.

Keep the launch line to the few flags that select a configuration. Structured
tool configuration (MCP servers, feature switches, allow-lists) belongs in the
tool's own config file, or in a per-agent home (`--env CODEX_HOME=…`) when only
one agent should have it.

Auto-mode is per-agent (`clank auto on|off|status`), inheriting a
user-global default (`~/.clank/config.json`'s `"auto"`) when unset —
`clank doctor` shows the effective value with its provenance.

The Stop hook counts how many times in a row it delivers the same items to an
agent's session.
- **What counts:** only continuations it actually sent. Peeks, inspection,
  reconnects and failed sends never count.
- **Duplicates:** a Stop is placed by the agent's own record when the hook is
  invoked (Claude's newest turn, or Codex's newest turn event), never by what
  the agent said. A Stop placed at or before one already counted is not counted
  again. OpenCode keeps no such record, so each continuation counts.
- **Escalation:** from the fourth unchanged delivery the message adds the count,
  and says:
  - ending a turn does not discharge pending work;
  - the user's latest instruction, any explicit pause, and the stated reason for
    a rejected action are followed;
  - a real dependency on the user belongs in `clank block ask <name> -m "…"`.

  A block pauses review work only; messages and events are still delivered.
- **Resets:** the count resets when the items, the session, or the blocks (which
  are among the items) change. A standing block does not reset it. On Claude, a
  turn that Claude records as a person's also resets it; elsewhere a turn of
  unknown origin never does.
- **Status:** `clank status` shows `repeating: <agent> → same items delivered N
  times` while the count is at or past four.
- **Limits:** only the wording changes; every item is still delivered. Grok hands
  work over through its own wait and is not counted.

A Claude agent that asks a question (`AskUserQuestion`) is recorded as asking
from the moment it asks, with or without a browser attached. `clank status`
shows `asking you: <agent> → "<header>"`, and the TUI and the remote status
show the same. While the question is pending, stall notices and failed-turn
retries hold. The question is over when it is answered, dismissed or cancelled
in the terminal, when its turn ends or fails, when a newer prompt begins, or
when its session is no longer the agent's.

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
  - The rest of clank's GitHub surface (`fork create --pr`) still
    shells `gh` directly.

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
  `clank events list` / `show` inspect metadata; `events read` delivers content; the `clank-github` skill
  teaches agents the react-then-ack loop.

Command producers write one JSON object per stdout line, with an `id` unique
within their source, and optional `content`, `kind`, `at` and `requires_reply`.
Replaying an ID preserves its original event. Clank runs each producer in a
process group it owns, and ends that whole group — TERM, a short grace, then
KILL — when its host stops, including on SIGTERM or SIGINT. If the host was
SIGKILLed instead, the next host to take the source ends the old group before
starting its own. A producer need not watch for its parent's death.

The monitor restarts a source only when its own entry changes: editing,
adding or removing another source or a timer leaves it running. A source it
stops — edited, removed, restarted from the TUI, or its agent off the roster —
gets TERM and then its `stop_grace` to finish, with its output still read and
logged, before KILL. A group whose work ends in time is never sent KILL, and
its replacement starts only once it has ended. `stop_grace` is a duration
(`"30s"`, `"2m"`, at most `"1h"`, `"0"` for none) and defaults to `"10s"`:

```json
{"kind": "command", "name": "signal", "command": ["signal-feed"], "stop_grace": "30s"}
```

When the host itself goes, every source's grace runs inside one drain
shared by all of them and is cut short by it: 10 seconds when a monitor loses
its lease, retires a stale build, or is sent SIGTERM or SIGINT, and the same
for a `clank wait` sent either; 1 second when a newer clank asks a monitor to
hand over.

To ask for an answer, configure the source with a separate reply handler:

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
if they have a handler. Messages also support replies as described below;
GitHub, timer and other backends refuse `--reply`.

Clank invokes `reply_command` directly with its configured argv, without shell
interpolation or added arguments. Stdin carries one JSON object, then EOF:

```json
{"version":1,"id":"question/42","reply":"Use option B.","author":"codex","idempotency_key":"<opaque stable key>","acknowledge":true}
```

`acknowledge` is true for the event's one answer (`events ack --reply`) and
false for each further reply (`events reply`, below). The answer's key is the
same as it always was; a further reply's key is its operation ID, so a handler
tells a retry (same key) from another reply (new key). What a further reply
means is the handler's business.

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
`clank events read <selector> --json` includes the saved reply and acceptance.

Any event whose backend takes replies — a message, or a command event with a
saved `reply_command` — also takes any number of further replies:

```sh
clank events reply command-questions-<hash>@1 -m 'Still working on it.'
clank events reply command-questions-<hash>@1 --operation <printed-id> -m 'Still working on it.'
```

`events reply` never answers, completes or reopens the event, before or after
its acknowledgement; a required event still needs its `--reply` answer. Each
invocation without `--operation` is a new reply, even with identical text.
Its intent is stored before it is sent and stays pending if delivery fails or
is interrupted; retry it with its printed operation ID and the exact text,
which resends that reply and never a new one, and sends nothing once it was
accepted. Changed text, or an unknown or pruned operation ID, is refused.
`events read` and its JSON list an event's replies (`replies`) with their
operation IDs and pending or accepted state.

Message replies use concrete agent destinations:

```sh
clank send --repo /destination --to worker --reply-to coordinator --reply-repo /origin -m 'Please investigate'
clank events ack messages@7 --reply 'Investigation complete.'
clank events reply messages@7 -m 'A further finding.'
clank events reply messages@7 --operation <printed-id> -m 'A further finding.'
```

`--reply-to` requires an answer before acknowledgement. Without `--reply-repo`,
the reply agent belongs to the invoking repository, independently of send's
destination `--repo`. The concrete agent and canonical repository are captured;
master changes do not retarget the answer. An unbound person can specify this
endpoint. Ordinary agent messages can receive a reply using their saved sender
metadata, without becoming reply-required; a human message needs an explicit
reply endpoint.

`events ack --reply` stores one immutable acknowledgement reply. The destination
must accept it durably before the original is handled. Retry with identical text;
different text cannot replace that acknowledgement. `events reply` sends further
replies as for any replyable event, each as a new message.

Message delivery is deduplicated across the destination/source commit gap. A
small destination receipt pin survives inbox acknowledgement and body compaction
until source acceptance is saved; cleanup failures leave acceptance intact and
can be retried explicitly. Reply commands use a single nonblocking author lease,
short local writer locks and no idle retry worker or polling. New replies do not
inherit the original message's reply obligation.

Reply-required messages use a version-2 inbox record that older readers cannot
treat as plain-ackable work. Install supporting binaries on every custodian before
using this feature; mixed old/new message-operation writers are unsupported.

Accepted replies appear in the timeline with a `✉` mail badge, their
destination (`→ @agent`, with the repository's path when it is another one) and
subject; the sender is in the event's details. A reply within the same
repository merges with its recipient's inbox copy; only recipient copies count
as pending work or ack targets. External replies carry the `→` sent mark
locally, even if that repository is unavailable, without fetching it or
claiming the recipient woke or acknowledged.
A reply to a command backend is part of the event it answers, not a timeline
entry of its own. The event's row is marked `↩N`, where N counts the replies its
logs retain, followed by `+` when older accepted ones were pruned and by how many
are still pending. Its page lists each reply: its agent, when, pending or
accepted, its operation, and its full text. Once the event itself has aged out
of the log, an accepted reply stays visible as one entry, "↩ reply to an earlier
<source> event", with the source's icon.
In the TUI, open the reply to read its full body, endpoint, operation ID and
original-message link. `clank log --json` preserves those fields as structured
data. Pending reply intents remain visible in event detail and are not shown as
sent timeline entries.

Command-backend callback attempts are **at least once**: a crash after backend acceptance but
before its local record can repeat delivery. The backend must persist and
deduplicate `idempotency_key`, return zero for a previously accepted identical
submission, and refuse conflicting content for that key. The author, key and
text remain the same on retry. Copies sharing the same resolved source key,
producer ID and saved route use one answer transaction and callback; a further
reply belongs to the copy it was sent from and acknowledges none; same-named
sources with different routes remain independent. A recorded acceptance lets
an interrupted fanout finish retiring other copies without another callback.

Replies add no idle worker, watcher or periodic retry. An explicit command
owns delivery and cleanup. A later explicit retry checks any saved callback
owner by kernel process birth identity before ending a surviving process
group; uncertain ownership refuses a new launch and names the record to inspect.
Use updated Clank binaries for all agents, the monitor and the TUI before
enabling required replies; an older client does not enforce this new policy.

Events may request **best-effort interruption** while retaining normal inbox
delivery. For a message:

```sh
clank send --to grok --interrupt -m "Please switch to the new task"
```

Set `"interrupt": true` on a command, GitHub or timer source to request it for
newly accepted events. A command producer line can override its source default
with `"interrupt": true` or `false`. The default is off. The chosen value belongs
to the persisted event; changing configuration or replaying an ID does not change
old events. Baseline history and source-failure notices never interrupt. Timers
request interruption only when an occurrence becomes due, including catch-up.

An interrupting source should describe its events with `"interrupt_message"`, and
a command producer line can override it. After the interruption, Clank submits
that text followed by the selector of each event, the selector that
`clank events read` and `clank events ack` take:

```json
{"kind": "command", "name": "voice", "command": ["voice-feed"],
 "interrupt": true, "interrupt_message": "Interrupted for new voice message"}
```

That produces `Interrupted for new voice message: command-voice-7f9bc9f9@15`.

- **Several events in one interruption:** each description is listed with its own
  selectors, groups joined by `; ` and capped with `+N more`.
- **Events with no usable description** are listed as `New events`. A description
  is usable if it is one line, contains no control characters, is not empty, and
  is at most 200 characters.
- **An unusable description** is never typed into a pane, and never stops its
  source or its events. `clank doctor` and the monitor report it, as they do a
  missing one.
- **The description is recorded with each event,** like `interrupt` itself.
  `clank events show --json` shows it as `interrupt_hint`.

The monitor initiates requests without requiring an armed waiter. One recipient's
pending eligible events coalesce into one request; the sender uses the same ledger
when sending an interrupting message without a monitor. Codex first tries the
bound thread's native control socket, selected from the target launch profile.
It requires an already-loaded thread with a matching repository, rechecks the
binding before control, interrupts the exact active turn and confirms that turn
ended with one read before submitting the inbox hint. It never resumes a thread
or polls for completion. Relative profile paths with unknown launch cwd are
refused. A read-only preflight failure permits verified terminal fallback;
uncertainty after native control forbids a second transport attempt.

The agent's pane first receives Escape, then a brief inbox instruction.
`clank nudge` uses the same interrupt-then-submit sequence with your message and remains independent of
hook and binding health. Interrupting first is what lets an IDLE agent keep the
message: submitting first started a turn that the interrupt then cancelled, and
the interrupt key is inert at an idle prompt for every supported provider. For
a BUSY agent the terminal path is best-effort. The message follows the interrupt
directly, and what happens to input arriving while a turn is still being
cancelled is up to the provider. Claude documents that input submitted while
busy is queued and sent after Escape; for Codex it is not established. If the
interrupt's delivery is not confirmed, no message is submitted. The key may
still have landed, so the failure is not reported as safe to repeat. A message
whose submission is unconfirmed is never resent automatically. Accepted
terminal bytes are not proof that a subprocess stopped or an event was read.

Interruption does not copy event contents into a second prompt, acknowledge work,
supply a required reply or create a wake receipt. Missing/unverified transport
preserves ordinary delivery. There is at most one automatic attempt for each
logical event; replay, reconnect and idle monitor beats do not retry it. A crash
or deadline may leave an unconfirmed attempt, which also does not retry.
`clank events show <selector> --json` includes `interrupt` and `interrupt_attempt`.
The interruption path adds no watcher or idle polling. Upgrade all Clank clients
and the monitor before enabling it. Monitor capability 10 includes this lifecycle;
a newer client requests cooperative retirement of an older host. Refresh long-lived
TUI clients as well so they share the replacement rather than keep local watchers. Claude
uses terminal control. Grok's and OpenCode's panes are never typed into; OpenCode's
server API has cancellation, but Clank has no verified attachable endpoint for a live
TUI. No separate provider process/server is started to control a persisted session. Without a
verified transport, pending work is delivered through the ordinary wait/hook.
An already-armed local wait emits its normal context directly; it needs no detached
interruption helper. Wake receipts and required replies keep their normal meaning.

Event timelines use `?` for an event that has not been woken, `✓` for a wake
that has not been acknowledged, and `✓✓` for acknowledgement. After the mark
comes the event's icon — `✉` for a message, a source's configured `glyph`, or
a default for its kind — then who it is for: `@codex`, or `@claude,codex` when
several agents hold it, and `@?` when that was never recorded. A wake proves successful output and flush to
an agent's delivery adapter or explicit `events read`, not understanding or
completion. Merely opening the timeline, inspecting events, or running a plain `wait`/`wait --peek`
does not mark them woken. Legacy acknowledged events get double ticks; pending
legacy events remain `?` until a real wake or explicit read.

Clank records the first wake for each exact inbox copy in its existing WAL.
Generic Codex, Claude and OpenCode continuations, Claude SessionStart context,
the native Grok background wait, and explicit event reads record
successful delivery after flush and binding validation. Failed output or stale
sessions leave the state unchanged. A crash or storage error after output can conservatively leave `?`;
the event stays owed, and a later real wake can record it. Callback configuration,
callback failure and callback budgets do not control this local evidence.

Merged entries show `✓✓` only when all local recipient copies are acknowledged.
If any pending recipient has no wake evidence, the entry remains `?`; otherwise
it shows `✓`. Detail and structured JSON retain each recipient's own `woken_at`
and acknowledgement. Pre-watch history stays distinct. Accepted outbound-only
replies show `→` for Sent: local custody never claims that an external recipient
woke or acknowledged. Display does not scan the destination repository.

Wake evidence adds no polling, process, idle timer or retry queue. Each actual
wake batches its new stamps under one short lock/read/file-and-directory sync per
source. Repeated wakes append no new stamps. Compaction retains evidence only
while its exact inbox copy is retained, including recurring timer occurrences.

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

A receipt means Clank successfully wrote and flushed a hook wake or an explicit
`events read` containing the event. It does not prove that the provider accepted the continuation or that the
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
or peek catch-up. An explicit `events read` uses the source-host cwd captured on
the accepted command row (or its saved reply route for older rows). Old rows with
neither context skip the callback with a diagnostic rather than guessing a cwd.

The receipt route is captured from the presenting author's current configuration.
Editing a handler changes later presentations; an in-flight batch keeps its captured
argv/cwd/state directory. Adding a handler also covers an existing source backlog;
removing it disables future receipts without retiring events. Source keys,
subscription arguments and immutable saved reply routes do not change. Omitted
handlers add no receipt callback, WAL reread, runtime coordination or child process.
Invocation-only `--event` receipt handlers are refused; configure the source instead.

Use `clank events read <selector>` (or `--json`) inside the bound agent session to
read an event's full content. `events list` and `events show`, including JSON, expose
only allowlisted routing, identity, lifecycle and reply-operation metadata. They
omit message subjects/bodies, command/timer content, watch instructions and reply
text. Reading verifies the current tool/session/agent binding before output and
after its successful flush; `--author` cannot impersonate a different recipient.
The read records the exact copy's first delivery evidence, then dispatches the same
configured command receipt callback as hooks. It never acknowledges or answers.
A failed write/flush or a replaced binding earns no receipt. Ordinary timeline and
raw filesystem views are not instrumented: this is an explicit delivery boundary,
not proof of model cognition or enforcement against direct filesystem access.

Supported presentations are generic Codex, Claude and OpenCode Stop-hook wakes,
Claude SessionStart's actual additional-context output, and explicit `events read`.
Native Grok handoffs record local delivery evidence but do not dispatch backend
receipt callbacks; arbitrary backend/file reads are outside this version. `events list`, `show`, `ack`,
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
session `…`, tab `…`** found another live pane named for the same
agent in the repo's workspace. Close that pane or use it there; a
session has one holder, and a second resume of it is what the tool
itself refuses. An agent with no session yet is refused the same way
(**already has a live pane in this repo's tab**), so a fresh agent is
not started twice. A claude agent whose
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

**`clank open` says sessions did not answer:** before opening, it checks every
Zellij session on the machine for the repo, and a session that did not answer in
time may hold it, so nothing was opened. Retry, or close the unresponsive
session.

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
