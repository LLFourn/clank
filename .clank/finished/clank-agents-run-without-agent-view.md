# clank-agents-run-without-agent-view
# Clank's Claude agents run without agent view

## Why

Seen 2026-11-05 on this repo's master. Claude Code 2.1.308's agent view
(`claude agents`, `--bg`, `/background`, the on-demand daemon) moved the
master's running conversation into its daemon when the user interrupted
a turn. Nobody asked for that. The daemon record in
`~/.claude/daemon/roster.json` shows `dispatch.source slash`,
`launch.mode resume, fork: true`, and `~/.claude/history.jsonl` shows no
slash command was typed.

The daemon then started a warm "spare" Claude (29afb9c9) in the repo,
carrying the master's environment, `CLANK_AGENT=claude` included. The
spare's SessionStart hook bound `claude` to it. It never took a turn, so
for an hour:
- the TUI read `claude → unknown: its record could not be read`;
- the real session was never woken, and Codex's REQUEST_CHANGES went
  unseen.

Clank already runs each agent in its own pane, and the TUI and remote
control already provide the views. Agent view adds nothing for a clank
agent and starts Claude processes clank does not own in the agent's
directory. Claude Code has a documented switch for exactly this. The
settings schema describes `disableAgentView`/`CLAUDE_CODE_DISABLE_AGENT_VIEW`
as: "Disable agent view (`claude agents`, `--bg`, /background, the
on-demand daemon) … Equivalent to CLAUDE_CODE_DISABLE_AGENT_VIEW=1". It
is separate from `disableRemoteControl`, and in-session remote control
ran in this conversation for weeks without the daemon.

## What changes

`agent.rs::tool_env_defaults` gives `Tool::Claude`
`CLAUDE_CODE_DISABLE_AGENT_VIEW=1`, beside OpenCode's default and with
a one-line why. Every Claude launch clank composes goes through
`launch_env`, so they all carry it: bound resume, fresh bootstrap, fork,
and the pane's restart. A profile's `launch.env` still overrides it, as
it does every default.

The variable is inherited by every process the agent starts, so a
daemon an agent would start has agent view disabled.

Agents already running are not affected until their next start. Nothing
is restarted.

## Acceptance

- A composed Claude launch (bound, bootstrap and fork) carries
  `CLAUDE_CODE_DISABLE_AGENT_VIEW=1`. Codex, Grok and OpenCode launches
  do not.
- A profile `launch.env` setting the variable to another value wins.
- Mutation check: drop the Claude arm from `tool_env_defaults` and the
  launch test fails.
- README: a sentence saying clank's Claude agents run with agent view
  disabled, and how a profile can turn it back on.

## As landed

721e2e47, as planned:
- `tool_env_defaults` gives `Tool::Claude` `CLAUDE_CODE_DISABLE_AGENT_VIEW=1` (const `DISABLE_AGENT_VIEW`).
- `every_claude_launch_runs_without_agent_view` covers the resume, bootstrap and fork composers across all four tools. Mutation check: dropping the Claude arm fails it.
- `a_profile_can_turn_agent_view_back_on` covers the `launch.env` override.
- `opencode_launches_disable_the_claude_compat_skill_scan` used to assert that a Claude launch carries only `CLANK_AGENT`. It now asserts that the OpenCode default is absent from the Claude, Codex and Grok launches.
- The README paragraph sits beside Codex's `--no-daemon` note.

The switch is documented by Anthropic: env-vars and settings-reference `disableAgentView`. This plan's intro calls it "documented" and that stands. A correction made in chat, which said it wasn't, was wrong.
