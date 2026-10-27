# an-opencode-agent-is-bound-at-launch
# An OpenCode agent is bound at launch

## Problem

Claude, Codex and Grok panes that clank opens bind themselves: their SessionStart hooks run `clank as` with the harness's own session id, so the model never sees the binding step. OpenCode has no such hook, so clank launches it with `--prompt "<repo> · <label> — Run \`clank as <label>\` to bind this session."` and relies on the model to do it.

Observed 2026-10-27 in `~/tmp/oc-live` with Qwen 3.8 27B: the model spent about four minutes and six tool calls before running `clank as qwen`. It thought for over a minute, read `clank --help`, `clank as --help` (which omits OpenCode, see clank-as-help-names-every-tool), `clank status`, dumped its environment, and listed the roster. A weaker or slower model may never get there, and every OpenCode launch spends a first turn on clank's bookkeeping.

## What changes

clank binds an OpenCode agent itself, before the model runs, by choosing the session id:

- `opencode --session <id>` continues the session `<id>`, or creates it if it doesn't exist (OpenCode 2.1.4's `--session, -s` flag). When clank launches a fresh OpenCode agent, it generates a new session id in OpenCode's own format, records the binding for the label, as `clank as` would, and passes `--session <id>`. Verify the format and the create-if-missing behaviour against the installed 2.1.4 statically, with sources and date; nothing is run.
- Resuming an agent whose binding already exists passes its recorded session id (`--session <bound id>`), so a relaunch continues the same session instead of starting a new one.
- The launch prompt no longer asks the model to run `clank as`. If there is no work to hand it, there's no `--prompt` at all, so OpenCode starts idle and the plugin's bootstrap/first-sighting wait picks up work as it does for a bound idle session. Check that the 2.x plugin arms for a session that has never had a turn; if it doesn't, that's part of this plan.
- The reservation goes through the existing locked agent-config mutation path, with a recheck. If two starts both read "unbound", one wins and the other sees its id instead of replacing it. Unrelated config, auto-mode and tool-change handling are preserved. A retry after a failed exec reuses the recorded id rather than minting another. `--print` and previews stay side-effect free and record nothing.
- Configured initial prompts or orientation still go to the agent. Only the bookkeeping `clank as` prompt is removed.
- If the binding can't be recorded (lock contention, a conflicting live binding), the launch refuses with the reason rather than starting an unbound agent.
- `clank as` still works by hand for a session started outside clank.

**Forks are out of scope.** `clank fork` launches OpenCode with `--session <SOURCE> --fork`, which makes a child session whose id OpenCode chooses. Binding the `--session` argument would bind the fork to its parent. Forks, and their no-source or missing-transcript fallback, keep their current binding behaviour and their bind instruction unchanged, as a documented exception. A later plan can prepare the child id natively.

The sweep covers ordinary launches only: README, the OpenCode lines in the skills, `agent add`'s "the TUI opens the pane" note, and anything that tells an OpenCode agent to bind itself.

## Out of scope

- Grok, Claude and Codex launches, which already bind through their hooks.
- Teaching models to follow instructions more directly; the point is that they don't have to.

## Acceptance

In-process, with the launch command captured rather than executed (no OpenCode, model or Zellij started):

- A fresh OpenCode agent's launch command contains `--session <id>` with an id in OpenCode's format, and no `clank as` prompt. The agent's config shows the binding to that id before the command would run.
- A relaunch of an already-bound OpenCode agent passes the recorded id.
- When the binding can't be recorded, nothing is launched and the reason is reported.
- The plugin lifecycle harness: a session that is bound but has never taken a turn arms its wait from the real `session.created` envelope, not from an inbox or execution event.
- Two concurrent starts of the same unbound agent end with one recorded id, used by both launch commands; neither replaces the other.
- `--print` records nothing.
- A fork's captured launch and its fallback are unchanged: still `--session <SOURCE> --fork` with the bind instruction, and nothing is pre-bound to the source id.
- Manual check, not gating: `clank open` on a repo with an OpenCode agent shows it bound in `clank status` with no model turn spent.
