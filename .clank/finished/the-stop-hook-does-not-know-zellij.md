# the-stop-hook-does-not-know-zellij

A pane is a view of the config. Closing it does not unbind the agent, and the process keeps running. The Stop hook currently does the opposite: it treats a missing Zellij pane as the agent dying and clears the binding.

That check does not belong there. Clank works without Zellij installed; Zellij is a convenience for putting agents on screen, not part of the core model. The hook is core. It must not name Zellij, read a pane id, or decide liveness from a pane listing.

## What is wrong

`stop_hook.rs` asks `own_pane_is_gone`, lists panes, parses `ZELLIJ_PANE_ID` out of the process table, writes `session.pane`, and releases the label when that pane is absent. Codex made this worse: its Stop hook runs in a long-lived app-server daemon whose inherited pane id is not the agent's pane, so a live session was unbound at every turn.

The hook's release is the whole defect this plan removes. What the status TUI shows about a still-running agent with no pane is a follow-up, `a-lost-pane-is-marked-in-the-tui`. This plan does not open a pane, reconnect a process, or draw that mark.

## The rule

`crates/cli/src/cli/stop_hook.rs` contains no reference to Zellij and no logic about panes. No `open_zellij`, no `ZELLIJ`, no pane id, no `session.pane`. A source gate fails the build if one appears, the same way `git_boundary` fails a `git` call outside the git layer.

Closing a pane leaves the binding as it is. The process keeps running and keeps being that agent. Nothing in the hook clears a binding because a pane is gone, and nothing in the hook binds a label because a pane appeared. A turn with Zellij not installed behaves the same as a turn with it installed.

## Tests

- The source gate on `stop_hook.rs`.
- A stop with no Zellij environment completes the turn and does not clear a binding.
- A missing pane is not something the hook observes: the binding is unchanged after the hook runs.

## Out of scope

Restoring a pane, reconnecting a process, and marking a still-running agent that has lost its pane. That mark is `a-lost-pane-is-marked-in-the-tui`. The turn-end decision itself — yield, arm, fence, claim — stays; only the pane question leaves the hook.