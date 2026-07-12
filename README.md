# clank

Multi-agent peer review around plans. One agent (the **master**)
writes a plan, reviewers weigh in, the master implements it commit by
commit, reviewers sign off on each commit, and the plan finalizes into
a sealed record. Everything lives in `.clank/` and `git`; there's no
daemon and no server.

## Install

```sh
git clone <this-repo>
cd <repo-dir>
cargo install --path crates/cli
```

That puts `clank` on your `$PATH`. Then wire it into your agents
(Claude Code, Codex CLI, Grok CLI — any or all):

```sh
clank setup
```

This installs the role-split skills (`clank-master`, `clank-reviewer`,
plus `clank-pr-review`) into `~/.claude/`, `~/.codex/`, and
`~/.grok/`, a `/clank` slash command for claude, and tag-merges a Stop
hook entry into claude's `settings.json` and codex's `hooks.json`
(grok has no active hooks — its skill carries the work loop). Re-run
`clank setup` after upgrading the binary; `--force` refreshes skill
files you've locally edited.

Verify with:

```sh
clank doctor
```

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

## Quickstart

In a repo:

```sh
clank init                  # scaffold .clank/ (+ post-rewrite hook)
clank agent add alice --tool claude
clank agent promote alice   # alice is the master
clank agent add bob --tool codex --review commit
```

(Or seed the whole roster from a saved template: `clank init --team
<name>`; save one with `clank team save <name>`.)

Inside each agent's session, bind once:

```sh
clank as alice
```

The master then queues and promotes a plan:

```sh
$EDITOR .clank/drafts/my-feature.md   # write the plan body
clank queue add my-feature            # consumes the draft into the queue
clank queue promote my-feature        # activates it: commits the intro
```

Reviewers' `clank wait` wakes with the intro to review; they write
verdicts:

```sh
clank feedback write --commit <sha> --verdict continue \
    --author bob -m "plan is well-scoped; implementation pending"
```

(`-m` is required, git-style: first line summary, then detail. The
verdict is prepended as the header — don't restate it in the message.)

The master implements in `[my-feature]`-prefixed commits, each
reviewed the same way. When the gate says FINISHED:

```sh
clank finish my-feature -m "<what changed>" -m "<why>"
```

## How agents stay awake

`clank wait` blocks until the calling agent has actionable work
(inferring author + role from the session binding). Each tool keeps
its loop differently — `clank setup`'s skills teach this, and
auto-mode drives it:

- **claude** — never waits inside its Stop hook. The hook nudges the
  agent to keep a background `clank wait` armed; the wait's completion
  wakes the session with the work items.
- **codex** — its Stop hook long-polls `clank wait` in-hook and blocks
  with the items (per-agent `wait_timeout` bounds the poll).
- **grok** — has no active hooks; its skill (and the auto-on launch
  prompt from `clank agent start`) teach it to arm the background
  wait itself.

Auto-mode is per-agent (`clank auto on|off|status`), inheriting a
user-global default (`~/.clank/config.json`'s `"auto"`) when unset —
`clank doctor` shows the effective value with its provenance.

### The wider wait surface

- `clank wait --peek` — non-blocking, side-effect-free "is there work
  right now?" probe.
- `clank wait --for commit|finished|stopped` — OBSERVE a repo (often a
  foreign one via `--repo`) instead of waiting for your own work.
- **Extra wake sources** (`wait_events` in the agent's config, or
  repeatable `--event '<json>'`): `github` entries poll any repo's
  events feed via `gh` (PRs opened/merged, comments, issues — your own
  actions filtered by default), and `command` entries spawn an argv
  whose completion is the wake. This is the building block for a
  "controller" repo whose agents manage other repos — the
  `clank-master` skill documents the pattern.

## Command reference

Run `clank <cmd> --help` for details; the skills carry the depth.

| Command | What it does |
|---|---|
| `init` | Scaffold `.clank/` (+ `--team <name>` to seed a roster) |
| `setup` | Install skills + hook entries into `~/.claude`, `~/.codex`, `~/.grok` |
| `doctor` | Diagnose repo / user / session setup; exits 1 on FAIL |
| `as` | Bind this agent session to a roster label |
| `auto` | Per-agent auto-mode on / off / status |
| `status` | Repo state; `--watch` live, `--tui` full-screen pane, `-j` JSON |
| `wait` | Block for work; `--peek`, `--for`, `--event` (see above) |
| `feedback` | `write` / `read` review feedback via a typed surface |
| `queue` | `add` / `promote` / `remove` / `reprioritise` queued plans |
| `finish` | Finalize a FINISHED plan (`-m` required; autosquash-aware) |
| `unfinish` | Move a finalized plan back to active |
| `agent` | Roster: `add` / `promote` / `remove` / `list` / `set-review` / `start` |
| `team` | Global team templates: `save` / `list` / `show` / `delete` |
| `fork` | Linked worktree with the whole team's sessions forked into it |
| `pr-review` | Multi-agent review loop against a GitHub PR |
| `open` | Agent workspace: self-managed console, or a zellij tab/layout |
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
~/.claude/settings.json             # claude Stop hook tag-merged in
~/.codex/hooks.json                 # codex Stop hook tag-merged in
~/.clank/config.json                # agent library, teams, hooks, defaults
```

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
