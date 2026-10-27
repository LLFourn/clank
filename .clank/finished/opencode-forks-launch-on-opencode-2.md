# opencode-forks-launch-on-opencode-2
# OpenCode forks launch on OpenCode 2

## Problem

`clank fork` launches a forked OpenCode agent as `opencode --session <SOURCE> --fork`. That worked on OpenCode 1.x. OpenCode 2 (2.1.4, installed 2026-10-26) dropped `--fork` from the main `opencode` command, so every OpenCode fork has been broken since the upgrade.

The user's ruling: whatever works and is stable.

## What was checked live (2026-10-27, OpenCode 2.1.4, scratch repo `~/tmp/oc-live`)

- **`opencode mini --session <src> --fork`** (the one command still carrying `--fork`): the child session's stored `directory` was the PARENT's repo, not the fork's worktree. OpenCode runs a session's tools in its stored location, so that fork would have worked in the parent repo. **Rejected.**
- **`opencode api session.fork --param sessionID=<src> -d '{}'`:** returns the child's `Session.Info` (`data.id`) and copies the history. The child had 21 `session_message` rows against the parent's 19; the parent was unchanged.
- **`opencode api session.move --param sessionID=<child> -d '{"directory":"<worktree>"}'`:** the child's stored `directory` became the worktree immediately. The parent's directory was unchanged.
- **`opencode --session <child>`**, run in the worktree: the full interface opened the child in the worktree, on the fork's branch, with the copied history in context.

## What changes

For an OpenCode fork with a live source session, the agent start prepares the child before launching. All of this runs under the label's existing binding lock (`BindingGuard`), so concurrent starts serialize and only one ever forks.

1. **Before the fork request:** record in the agent's fork launch intent (`.clank/agents/<label>/fork.json`) that a fork of `<source>` was requested.
2. **Fork:** `session.fork` on the source. On success, write the returned child id into the same intent immediately, before anything else can fail.
3. **Move:** `session.move` the child to the fork's worktree, then read it back with `session.get` and require its `directory` to be the worktree. A successful move call alone isn't proof of placement.
4. **Bind and launch:** record the binding to the child, as the bind-at-launch plan does, then launch the ordinary `opencode --session <child>`. The orientation prompt is kept and the `clank as` instruction is dropped.

On every later start, the intent decides what happens:

- **The intent holds a child id but the agent isn't bound:** resume preparation from step 3 with that child. It never forks again.
- **The agent is bound to the child:** the ordinary resume path, which also covers a retry after a failed exec.
- **A fork was requested but no child id was recorded** (the reply was lost, or clank died mid-call): refuse, launch nothing, and say the fork may exist. Recovery goes through one new command, run in the fork's repo and scoped to that one agent's intent: `clank fork recover <label> --child <id>` or `clank fork recover <label> --forget`.
  - `--child <id>` adopts a child the operator found, e.g. with `opencode session list` in the source repo, into that agent's intent. The next start then runs steps 3 and 4 on it: move, confirm, bind. The child is never bound before its directory is confirmed. clank refuses an id equal to the source.
  - `--forget` clears that agent's requested marker, so the next start forks afresh. The refusal message says to delete any stray child first.
  - Both refuse when the agent has no OpenCode fork intent, or already has a recorded child or a binding. They never touch other agents' or other tools' intents.
- **A failed fork request** that OpenCode documents as a refusal before acting, e.g. the source session isn't found, clears the requested marker and reports the error.

### The same backend for preparation and launch

The `opencode api` calls run with the profile's executable (`launch.command`, default `opencode`), its `launch.env`, and the fork's worktree as working directory. A profile whose `launch.args` select a backend (`--server <url>`, `--server=<url>` or `--standalone`) is refused before any request, with the reason. Preparation would otherwise talk to a different store than the launched pane, and `--session` would create an empty session under the child id. Other launch args (model, agent) aren't passed to `opencode api`.

### Unchanged

- The no-source and missing-source fallback keeps its current behaviour: a fresh session with the orientation prompt and the bind instruction. A source probe that fails or returns something unknown is not treated as missing.
- Previews and `--print`: no API call and no write.
- Claude, Codex and Grok forks.

Clank adds no socket or HTTP client. It runs the `opencode api` command and parses only the JSON `data` it prints.

## Acceptance

In-process, with the `opencode api` calls injected and the launch captured. No OpenCode, model or Zellij runs.

- **Success:**
  - the calls go fork, then move, then get, in that order;
  - the child id is in the intent before the move;
  - the child is bound before launch, and nothing is bound to the source;
  - it launches `opencode --session <child>` with the orientation prompt and no `clank as` instruction.
- **Move fails after the fork:** nothing is launched. The next start does no fork call: it moves the same child and launches.
- **The binding write fails after a good move:** nothing is launched. The next start reuses the same child, with no fork.
- **`get` reports a directory other than the worktree:** nothing is launched and nothing is bound.
- **Two concurrent starts:** exactly one fork call, and both end on the same child.
- **The fork reply is lost:** the next start makes no fork call, refuses with the recovery instructions, and launches nothing.
- **A failed exec after binding:** the next start resumes the bound child, with no fork.
- **Recovery after a lost reply:** `fork recover <label> --child <id>` for a child whose directory is still the source repo leaves it unbound. The next start makes no fork call, moves and confirms it, then binds and launches that same id. If the move or `get` fails during recovery, nothing is bound or launched. `--forget` clears only that agent's marker. Both refuse on an agent with no intent, a recorded child, a binding, or a child id equal to the source.
- **A returned child id equal to the source:** refused before any move.
- **A profile with `--server`, `--server=<url>` or `--standalone`:** refused before any API call.
- **Preparation's environment:** every API call gets the profile's executable, `launch.env`, and the worktree as working directory.
- **`--print` / preview:** no API call and no write.
- **No-source fallback, and other tools' forks:** unchanged.
- **Mutation checks:**
  - recording the child id only at binding time fails the move-fails test;
  - skipping the move fails the success test;
  - dropping the requested-marker check fails the lost-reply test.
- **Live check, not gating:** `clank fork` an OpenCode agent. The child's `session_v2.directory` is the worktree, the parent is untouched, and the pane opens with the history.
