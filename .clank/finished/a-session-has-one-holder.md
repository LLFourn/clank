# a-session-has-one-holder

`clank open` after closing a zellij tab often fills the master's pane
with this instead of the master:

> Session 15debee4-1858-4bd6-99fc-c74771ec276b is running as a
> background session (15debee4). Run `claude attach 15debee4` to open
> it, or `claude stop 15debee4` first to resume it here. Add
> --fork-session to branch off a copy instead.

And a codex pane, on opening a new session while the old one still
lives:

> Error: Failed to resume session from
> ~/.codex/sessions/2026/08/27/rollout-…-01a04034-….jsonl: thread/resume
> failed during TUI bootstrap: thread/resume failed: thread
> 01a04034-cebe-7c12-8595-99c9b9b9de9c already has an active writer
> (code -32600)

Neither agent starts. The user reads the message, works out which
process holds the session, and fixes it by hand — in a pane clank was
supposed to have filled.

## A session has one holder, and `agent start` resumes blind

Each agent's binding names a session. `clank agent start` turns that
into `claude --resume <id>` / `codex resume <id>` and spawns it into
the pane without asking whether anything already holds that session.
Two things now can:

- **A process that outlived its pane.** Claude Code keeps a session
  running as a BACKGROUND session when its terminal goes away, and
  closing a zellij tab is exactly that. `claude agents --json` lists
  them (no TTY needed): `kind: "background"`, the full `sessionId`, the
  short `id` that `attach` takes. On this machine right now there are
  dozens, every one a clank agent orphaned by a closed tab.
- **A pane that is still open somewhere else.** `clank open` into a
  new zellij session while the old session still has the agent's pane.
  That pane's process holds the session — codex's "active writer" — and
  a second `resume` of the same id cannot succeed with any tool.

The layouts are correct; the launch is what is wrong. It must find the
holder before it spawns, because the pane is interactive and the
refusal cannot be parsed after the fact.

## Change

`clank agent start` asks who holds the session before composing the
launch:

1. **A live pane in any zellij session runs this agent's launch
   command** (the byte-exact `agent_start_command`, already how the
   reconciler recognises agent panes; `zellij ls` + per-session
   `list-panes` is the same read the reconciler does, one session
   wider). Then the agent is open elsewhere. The pane prints ONE line —
   which session and tab hold it, and that closing that pane (or
   attaching to that session) is the fix — and exits non-zero. Tool
   agnostic; this is the codex case and it is also the claude case when
   the old tab is still open.

   **The launching pane is in that listing too.** In the normal path
   `clank agent start` runs INSIDE the pane zellij just created, and
   that pane's `terminal_command` is the very string being searched
   for. A scan that does not know who is asking finds itself and
   refuses every fresh launch (codex on f125770). So the decision
   takes the caller's identity — `$ZELLIJ_SESSION_NAME` plus
   `$ZELLIJ_PANE_ID`, which `caller_pane_id` already reads — and
   excludes exactly that one pane; pane ids are per session, so the
   pair is the key, not the id. Every OTHER match, in this session or
   another, still counts. When the caller cannot be identified — inside
   zellij (`$ZELLIJ` set) but without a pane id — the scan is skipped
   rather than run blind, because a match might be the caller.
   Outside zellij entirely there is nothing to exclude and every match
   is a genuine other holder.

   **A holder is a running process, not a pane with the right
   command.** zellij keeps a pane open after its process ends, showing
   the exit code, and `list-panes` still reports its
   `terminal_command`. `ZellijPane::exited` is deliberately ignored by
   `find_pane_by_command` — identity there is the command, so the
   reconciler can find and close a dead copy — but this decision is
   about who HOLDS the session, and a dead pane holds nothing. Only
   non-exited, non-plugin panes count. Reusing the identity match as
   written would refuse a launch over the corpse of the pane the user
   just closed (codex on d38a63d), which is the failure this plan
   exists to remove.
2. **Claude lists the session as a background session.** Launch
   `claude attach <short id>` instead of `--resume`. The running
   process is reconnected to the pane with whatever it was doing
   intact; nothing restarts and no context is lost. `attach` is the
   verb, not `stop` then `--resume`: stopping discards in-flight work
   (a background session's parked `clank wait` still wakes it) and pays
   a full restart for nothing.
3. **Neither.** Today's resume argv, byte for byte.

A probe that fails — no zellij, no `claude agents`, a listing shape
clank does not recognise — falls through to today's path. If claude
changes its listing, the fallback is the current behaviour, not a new
failure.

The decision itself is pure: it takes the pane listing, the caller's
identity, and the claude listing as values, and returns the launch to
compose or the holder to name. Every zellij process (`ls`, per-session
`list-panes`) and the `claude agents --json` read stay in
`open_zellij.rs` / the tool's launch module behind typed functions —
the ownership gate already enforces the zellij half.

The seam is `session_restore_args` in `agent.rs`, the pure tail of
`compose_launch`; the probes feed it, they do not spawn. `clank agent
start <label> --print` already prints the composed launch, so the
decision is checkable without starting anything. The attach launch is
`claude attach <short id>` and nothing else: `attach` is a subcommand,
and the profile / launch args belong to the running process, which
already has them.

## Tests

- Claude bound to a session the listing reports as `background`:
  argv is `claude attach <short id>`. Bound to one absent from the
  listing, or with the listing unavailable: today's `--resume` argv.
- The listing is parsed from a fixture of real `claude agents --json`
  output; an `interactive` entry for the same session id is NOT
  background.
- A pane in another session running the agent's launch command: no
  spawn, the message names that session and tab. Same command in a
  different repo's pane (a sibling worktree, a fork) does NOT count —
  the command carries `--repo`, so this is byte matching, not label
  matching.
- Fixtures for the caller: a listing whose ONLY match is the caller's
  own (session, pane) proceeds to launch; the caller's own match plus
  one more refuses and names the other's session and tab, not the
  caller's; the same pane id in a different session is not the
  caller. Inside zellij with no pane id, the scan is skipped and the
  launch proceeds; outside zellij, a lone match anywhere refuses.
- Fixtures for liveness: an EXITED other match does not block, alone
  or beside the caller; a non-exited other match does. The fixture
  panes are real `list-panes --json` shapes with `exited` set, not
  hand-built structs.
- codex/grok/opencode argv is unchanged when nothing holds the session.

## Out of scope

- Cleaning up the orphaned background sessions that already exist
  (`claude rm`); this plan stops creating the need.
- Moving a live pane between zellij sessions (zellij 0.45.0 cannot).
