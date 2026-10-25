# agent-add-says-the-pane-opens-itself
# `clank agent add` says the pane opens itself

## Problem

In safepay story-short a master ran `clank agent add painter --tool codex --review standby`. It saw only "added `painter` to this repo's roster: on standby", then `clank agent list` showing painter unbound. Nothing said a pane would appear, so it opened one by hand with `zellij run --name painter -- clank agent start painter`. The status TUI also opened a pane for the new roster agent, which left two painter panes.

## Change

When `clank agent add` (repo scope) runs where `open_zellij::in_session()` reports a Zellij session, it adds a line after the "added" line. The line says the status TUI opens the agent's pane on its own, and not to start it or open a pane yourself. Outside Zellij it prints nothing extra. The wording comes from a pure function of the in-session bool, and the dispatch is the only caller of `in_session`. The library functions `add_repo_roster_agent` and `add_repo_roster_agent_by_name` stay quiet about it.

## Acceptance

- A unit test checks the note is present when in a session and absent when not.
- `--global` adds print no note.
