# clank

Multi-agent peer review around plans. One agent writes a plan, others
review it, the first agent implements it, reviewers sign off
commit-by-commit (CONTINUE / FINISHED), finalize. Everything lives in
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
- **reviewers** — everyone else. Reads the plan, continues (or finishes,
  or requests changes) on each commit master makes.

Both roles are just regular agent sessions running `clank`. The role of
the calling agent is inferred from the local `.clank/config.json` roster
plus the agent's label.

### Per repo: initialize

In a fresh repo:

```sh
clank init
```

That writes `.clank/.gitignore`, creates a local `.clank/config.json`
roster when needed, and sets up claude's per-repo edit permissions for
`.clank/agents/**` so the agent doesn't prompt every time it writes
local state. It does not bind a session or choose a master; do that with
`clank as`, `clank agent add`, and `clank agent promote`.

### Per session: bind your agent

The first thing an agent does in a session:

```sh
clank as alice          # bind this session as `alice`
clank auto on           # enable the Stop hook
```

(`clank init` does both interactively if you'd rather.)

Now `clank wait`, `clank feedback write`, etc. all infer `--author alice`
from the session.

### Master: write a plan

```sh
# Author the plan file at .clank/plans/my-feature.md
$EDITOR .clank/plans/my-feature.md
git add .clank/plans/my-feature.md
git commit -m "[my-feature] intro"
```

Reviewer wait fires on the new commit.

### Reviewers: review

A reviewer waiting on work (`clank wait`, or the Stop hook auto-mode)
gets told there's a commit to review. They write feedback via:

```sh
echo "CONTINUE

Looks good." | clank feedback write \
    --plan my-feature \
    --commit abc1234... \
    --verdict continue \
    --author alice
```

Or `REQUEST_CHANGES` with notes. The header (`CONTINUE` /
`REQUEST_CHANGES`) is validated against the `--verdict` flag. (`CONTINUE`
means good-but-more-to-do; `FINISHED` means the plan is done.)

### Master: address feedback

If anyone requested changes, master's wait says so. Edit the plan or
code, commit, and the cycle repeats. Once everyone has CONTINUE'd the
latest reviewable commit, master implements:

```sh
# code changes under [my-feature] commit prefix
git commit -m "[my-feature] implement"
```

When the implementation commit is FINISHED by all participants, master
finalizes:

```sh
clank finish my-feature
```

This seals the approval snapshot under `.clank/finished/my-feature/`
and commits the finalize.

## The Stop hook (auto-mode)

`clank setup` installs a Stop hook into your claude / codex config that
runs whenever the agent would end a turn. Two modes:

When enabled, the hook long-polls `clank wait`. The agent's turn
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

Roles are a property of the repo's **agent roster**, not a per-agent
flag. An agent is master / commit-reviewer / gate-reviewer because of
its entry in the roster. Build and change the roster with:

```sh
clank agent add <agent> --tool claude            # define one inline
clank agent add <agent> [--review commit|gate]    # add by name from the global library
clank agent promote <agent>                       # make <agent> the master (demotes the old one)
clank agent remove <agent>                         # drop from the roster
```

`clank auto on|off` toggles an agent's auto-mode (Stop-hook behavior),
which is independent per-agent state. (It still accepts a legacy
`--role` flag, but that's now a no-op — roles are roster-derived.)

## Layout

```
<repo>/.clank/
├── plans/                          # active plans (one .md per plan) — TRACKED
├── finished/                       # finalized plans — TRACKED
├── config.json                     # local roster + repo config
├── agents/<label>/
│   ├── config.json                 # auto-mode + session binding
│   └── feedback/<plan>/<sha>.md    # this agent's review notes
├── cache/                          # local fold cache
└── .gitignore                      # allow-lists plans/, finished/, and itself
```

Only `plans/`, `finished/`, and the managed `.clank/.gitignore` are
committed. Everything else under `.clank/` is per-user state — agents on
the same machine share it via the filesystem; multi-machine
collaboration just shares the plans and finalized plan records.

```
~/.claude/
├── skills/clank/SKILL.md           # written by `clank setup`
└── settings.json                   # Stop hook tag-merged in

~/.codex/
├── skills/clank/SKILL.md
└── hooks.json
```

## Useful commands

```sh
clank status                       # current plan + gate state
clank status --all                  # every plan including finished
clank doctor                        # diagnose setup across all scopes
clank wait                           # block for work for this agent
clank feedback write ...            # write a review
clank finish <plan>                 # finalize a finished plan
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
  OK   ~/.claude/settings.json: claude Stop hook installed
  OK   ~/.codex/hooks.json: codex Stop hook installed

[session]
  OK   env: running inside claude (session …)
  OK   identity: resolved to `alice` via session binding (last bound …)
  OK   role: inferred role: master
```
