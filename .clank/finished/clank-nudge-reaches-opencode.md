# clank-nudge-reaches-opencode
# clank nudge is a blunt instrument

## Problem

`clank nudge` refuses OpenCode and Grok: `refused: opencode's input cannot be read, so whether it is in shell …`. Observed 2026-10-27 in `~/tmp/backup-bip-experiment`: `claude` tried to nudge `qwen` (OpenCode) and couldn't. Before typing, nudge reads the pane's screen to prove the input box is empty and not in shell mode, and it can only read Claude's and Codex's boxes.

Earlier revisions of this plan tried to route OpenCode nudges through the plugin and the monitor. That grew into thousands of lines of control connections, process routing and interrupt barriers, and was abandoned (the work is saved outside the repo, not committed).

The user's ruling: nudge is a hack, a blunt instrument for an agent that's stuck, and it should work like one. Assume the input box doesn't start with `!`. Don't inspect the screen.

## What changes

`clank nudge` does the same three things for every tool, without looking at the pane first:

1. Press the tool's interrupt key in the agent's named pane: Esc for Claude, Codex and OpenCode, and Ctrl+C for Grok, as `pane_input::stop_argv` already chooses.
2. Type the message.
3. Press the tool's submit key, using the existing per-tool submit, which includes Codex's paste-safe Enter.

- **What it keeps:** the pane lock, the named-pane lookup from the-pane-is-named-by-its-agent, the control-byte neutralising of the typed text (`Admitted`) and the nudge history. It no longer refuses because a screen can't be read or reads as shell mode.
- **What's removed from nudge:** the composer read before typing and the Grok/OpenCode refusal. A failed zellij write is still reported as failed, as now.
- **Scope:** only the manual `clank nudge` path. Recovery's automatic typed sends and user-command dispatch keep their screen checks, since they run unattended. If they share a function with nudge, give nudge its own straight path rather than adding a flag to the checked one.
- **Docs:** `nudge.rs`'s module doc and the README say plainly that nudge is blunt. It interrupts and types whatever is in the box, so a person's half-typed draft is sent with it, and a box in shell mode runs the text as a command. That's the trade-off for always working.

## Out of scope

- The plugin, the monitor and recovery.
- Any change to how automatic recovery reaches OpenCode or Grok.

## Acceptance

In-process with a recording terminal; no Zellij, OpenCode or model started:

- For each of Claude, Codex, OpenCode and Grok, a nudge writes exactly the interrupt key, the text, then the submit key, in that order, to the named pane, and never asks for a screen dump.
- Grok's interrupt is Ctrl+C; the others are Esc.
- A failed write is reported, and the history records it.
- Recovery's typed send still refuses an unreadable or shell-mode screen; its existing tests pass unchanged.
- Mutation check: putting the screen check back into the nudge path fails the OpenCode and Grok tests.
- Manual check, not gating: `clank nudge --to qwen -m "…"` in `~/tmp/oc-live` interrupts and prompts the live OpenCode session.
