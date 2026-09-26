# a-lost-pane-is-marked-in-the-tui

Follows `the-stop-hook-does-not-know-zellij`. Closing a pane does not unbind the agent, and this plan does not put the pane back. It only shows the situation.

When an agent is still bound and its process is still running, but no pane is showing it, the status TUI marks that agent with `!`. The mark means the process outlived its pane. It does not open a pane, reconnect the process, or change the binding.

Zellij stays in the status TUI. The Stop hook still does not learn about panes. The mark is derived when the TUI looks: roster and binding say the agent is here, the process is alive, and the pane listing has no pane for it. No pane id is stored as core state to remember the loss.

## Tests

- A bound, running agent with no pane is rendered with `!`.
- A bound agent who has a pane is not marked.
- A bound agent whose process has exited is not marked with `!` — that is a different fact from a lost pane.
- The Stop hook is unchanged by this plan: it still has no Zellij reference.

## Out of scope

Opening or restoring the pane. Changing what the Stop hook does.