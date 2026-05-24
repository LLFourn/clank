# clank

Multi-agent peer review around plans. One agent writes a plan, others
review it, the first agent implements it, reviewers approve the
implementation commit-by-commit, finalize. Everything lives in
`.clank/` and `git`; there's no daemon and no server.

## Install

```sh
git clone <this-repo>
cd <repo-dir>
cargo install --path crates/cli
```

That puts `clank` on your `$PATH`.

Then wire it into your agents (Claude Code, Codex CLI, or both):

```sh
clank setup
```

This writes three skill/command files into `~/.claude/` and `~/.codex/`
and tag-merges a Stop hook entry into each agent's settings file. Re-run
after upgrading `clank` to refresh.

Verify with:

```sh
clank doctor
```

## The workflow

Two roles:

- **master** — author of a plan. Writes it, implements it, finalizes it.
- **reviewers** — everyone else. Reads the plan, approves or requests
  changes on each commit master makes.

Both roles are just regular agent sessions running `clank`. The role of
the calling agent is inferred from `.clank/config.json` (which names the
master) plus the agent's label.

### Per repo: initialize

In a fresh repo:

```sh
clank init
```

That writes `.clank/.gitignore`, sets up claude's per-repo edit
permissions for `.clank/agents/**` (so the agent doesn't prompt every
time it writes feedback), and — if you're running inside an agent —
prompts you to bind this agent's label and optionally claim master.

### Per session: bind your agent

The first thing an agent does in a session:

```sh
clank as alice          # bind this session as `alice`
clank auto on           # enable the Stop hook
```

(`clank init` does both interactively if you'd rather.)

Now `clank wfw`, `clank feedback write`, etc. all infer `--author alice`
from the session.

### Master: write a plan

```sh
# Author the plan file at .clank/plans/my-feature.md
$EDITOR .clank/plans/my-feature.md
git add .clank/plans/my-feature.md
git commit -m "[my-feature] intro"
```

Reviewer wfw fires on the new commit.

### Reviewers: review

A reviewer waiting on work (`clank wfw`, or the Stop hook auto-mode)
gets told there's a commit to review. They write feedback via:

```sh
echo "APPROVE

Looks good." | clank feedback write \
    --plan my-feature \
    --commit abc1234... \
    --verdict approve \
    --author alice
```

Or `REQUEST_CHANGES` with notes. The header (`APPROVE` /
`REQUEST_CHANGES`) is validated against the `--verdict` flag.

### Master: address feedback

If anyone requested changes, master's wfw says so. Edit the plan or
code, commit, and the cycle repeats. Once everyone has approved the
latest reviewable commit, master implements:

```sh
# code changes under [my-feature] commit prefix
git commit -m "[my-feature] implement"
```

When the implementation commit is approved by all participants, master
finalizes:

```sh
clank finish my-feature
```

This seals the approval snapshot under `.clank/finished/my-feature/`
and commits the finalize.

## The Stop hook (auto-mode)

`clank setup` installs a Stop hook into your claude / codex config that
runs whenever the agent would end a turn. Two modes:

When enabled, the hook long-polls `clank wfw`. The agent's turn
doesn't end until work arrives or the timeout expires. When there's
no work and no active plans, the hook exits silently.

Toggle per-agent per-repo:

```sh
clank auto on              # enable
clank auto off             # disable
clank auto status          # show current state
```

## Multi-agent in one repo

Each agent gets its own label. Bind once per session:

```sh
# in claude
clank as alice

# in codex (different session)
clank as bob
```

Identity is keyed by the agent's session id (`CLAUDE_CODE_SESSION_ID` /
`CODEX_THREAD_ID`). Two same-tool sessions in the same repo can bind to
different labels.

Role is a per-user preference:

```sh
clank auto on --role master      # default this agent to master view
clank auto on --role reviewers   # default to reviewers
```

Each agent's role is independent; setting yours doesn't make any
repo-wide assertion. Gate state is computed from who's actually
participated (written feedback), not from role claims.

## Layout

```
<repo>/.clank/
├── plans/                          # active plans (one .md per plan) — TRACKED
├── finished/                       # finalized plans — TRACKED
├── agents/<label>/
│   ├── config.json                 # auto-mode + role + session binding
│   └── feedback/<plan>/<sha>.md    # this agent's review notes
├── cache/                          # local fold cache
└── .gitignore                      # blanket-ignores everything except plans/ and finished/
```

Only `plans/` and `finished/` are committed. Everything else under
`.clank/` is per-user state — agents on the same machine share it via
the filesystem; multi-machine collaboration just shares the plans + the
finalized seal.

```
~/.claude/
├── skills/clank/SKILL.md           # written by `clank setup`
└── settings.json                   # Stop hook tag-merged in

~/.codex/
├── skills/clank/SKILL.md
├── commands/clank.md
└── hooks.json
```

## Useful commands

```sh
clank status                       # current plan + gate state
clank status --all                  # every plan including finished
clank doctor                        # diagnose setup across all scopes
clank wfw                           # block for work for this agent
clank feedback write ...            # write a review
clank finish <plan>                 # finalize an approved plan
clank purge <plan>                  # strip a plan's artifacts from git history
```

## Troubleshooting

`clank doctor` is the first stop. It reports per-section OK / WARN /
FAIL with actionable messages — missing skill files, missing hook
entries, gitignore drift, unbound sessions, etc.

For a fully bound session with the hook installed, you should see:

```
[repo]
  OK   .clank/.gitignore: present
  OK   .claude/settings.local.json: all three agent edit-permission rules present
  OK   agent: alice: …, auto_mode=on, session=claude bound to …

[user]
  OK   ~/.claude/skills/clank/SKILL.md: matches embedded content
  OK   ~/.codex/skills/clank/SKILL.md: matches embedded content
  OK   ~/.codex/commands/clank.md: matches embedded content
  OK   ~/.claude/settings.json: claude Stop hook installed
  OK   ~/.codex/hooks.json: codex Stop hook installed

[session]
  OK   env: running inside claude (session …)
  OK   identity: resolved to `alice` via session binding (last bound …)
  OK   role: inferred role: master
```
