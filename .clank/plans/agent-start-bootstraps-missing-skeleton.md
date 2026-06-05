# agent-start-bootstraps-missing-skeleton

`clank agent start <label>` fails with "no such agent" when
the agent is declared globally (in `~/.clank/config.json`
default_agents) but doesn't have a per-repo skeleton at
`<repo>/.clank/agents/<label>/config.json` in the current
repo. This breaks `clank open zellij` in fresh repos: the
layout includes all declared agents (correct — global
declaration says they're registered), but
`clank agent start <label>` fails for any agent that hasn't
been bound to a session in this repo yet.

lloyd 2026-06-06 hit this in `~/src/frostsnap`: claude +
codex panes worked (bound previously), ruthless pane
errored because ruthless had never been bound in frostsnap.

## Goal

When `clank agent start <label>` is called for an agent
that's in the merged declaration but has no skeleton in
this repo, spawn the bare tool with a seed prompt asking
the agent to bind itself via `clank as <label>`. After the
first turn, the skeleton exists and subsequent
`clank agent start <label>` calls resume normally.

This is the behavior the POC `open-worktree.sh` script
implemented (bare tool + seed prompt). Bringing it into
`clank agent start` makes `clank open zellij` work in any
repo with declared agents, regardless of which agents have
been previously bound.

## Verified before promotion (2026-06-06)

- `clank agent start` at `crates/cli/src/cli/agent.rs`
  fails with "no such agent" when
  `load_agent_config(&repo, &label)` returns None
  (skeleton file absent). Error from `agent_start::run`'s
  early-return path.
- `load_merged_agents(&repo, home.as_deref())?` already
  returns the agent's declaration entry (label, role, tool,
  launch, initial_prompt). The tool is what the bootstrap
  spawn should use; launch + initial_prompt are also
  respected.
- The POC `open-worktree.sh:71-85` shape:
  ```
  pane name="$label (reviewer)" {
      command "$tool"
      args "Please run `clank as $label` to bind this session as a reviewer agent for this worktree. Reply with the result, then wait for further instructions."
  }
  ```
  That's what `clank agent start` should produce when the
  skeleton doesn't exist.

## Approach

In `cli::agent::start` (or `agent_start::run`):

1. Load merged declaration via `load_merged_agents`.
2. Find the entry for the requested label.
3. If no declaration entry exists → today's "no such agent"
   error (this is still a real error: label was never
   registered).
4. If declaration entry exists AND skeleton exists → today's
   resume behavior (load skeleton, get session, compose
   launch).
5. **NEW**: declaration entry exists BUT skeleton doesn't:
   bootstrap path. Compose a launch with:
   - Tool: from declaration's `tool` field (claude / codex).
   - Args: declaration's `launch.args` (if any) + a final
     positional bootstrap prompt (PINNED verbatim):
     ``"Run `clank as <label>` to bind this session."``
   - NO `--resume` flag (there's no session id yet).
   - Env from declaration's `launch.env`.

The bootstrap prompt INSTRUCTS the agent to run a specific
clank command. This diverges from `DEFAULT_AUTO_PROMPT`'s
"don't instruct the agent to run a clank command" property,
intentionally — there is no other way to create the
session binding. The agent runs the command, prints the
result, ends the turn, and the stop hook takes over.
Unlike the wfw double-trigger concern (where the stop hook
itself runs wfw), `clank as` is a one-time bind that the
stop hook does NOT also perform.

The decision lives in `cli::agent::start`'s arm; the launch
composition reuses `compose_launch` minus the
session-restore suffix.

## Surfaces touched

- `crates/cli/src/cli/agent.rs`:
  - `start` (or whatever the entry fn is): detect
    declaration-without-skeleton case; dispatch to a new
    `compose_bootstrap_launch` helper.
  - `compose_bootstrap_launch(declaration_entry, repo)`:
    builds the bootstrap argv (tool + launch.args + seed
    prompt) and returns a `ComposedLaunch` shape compatible
    with the existing dispatch.
  - The seed prompt constant lives alongside
    `DEFAULT_AUTO_PROMPT` for symmetry. Suggested name:
    `BOOTSTRAP_BIND_PROMPT`.

## Tests

- Unit test for `compose_bootstrap_launch`: declaration with
  tool=claude + launch.args=["--skill", "ruthless"]; assert
  argv is `["claude", "--skill", "ruthless", "<bootstrap-prompt>"]`
  (no --resume, prompt as final positional).
- Integration test in
  `crates/cli/tests/agent_start_integration.rs`:
  - Set up declaration with a registered reviewer label
    `phantom` (via typed `RepoConfigFile`).
  - Do NOT create a skeleton at
    `<repo>/.clank/agents/phantom/config.json`.
  - Spawn `clank agent start phantom --print`.
  - Assert stdout argv ends with the bootstrap prompt
    (e.g. `'claude' '<bootstrap-prompt>'`), not the
    "no such agent" error.
- Negative test: agent NOT in declaration → today's "no
  such agent" error still fires (the existing test
  `agent_start_no_skeleton_errors` or equivalent should
  pass unchanged).

## Out of scope

- Auto-running `clank as <label>` on the agent's behalf.
  The seed-prompt approach lets the agent itself execute
  the bind so the session-id-to-label mapping is owned by
  the actual agent's process. Auto-binding would require
  guessing the session id from the parent process, which
  is fragile.
- Detecting whether the agent's tool is actually claude or
  codex from the seed prompt and tailoring the wording. The
  POC's single prompt works for both tools today; a
  per-tool variant can come later if needed.
- Changing `clank open zellij`'s pane composition.
  `clank agent start <label>` is what each pane invokes;
  fixing it here fixes the zellij surface for free.

## Acceptance

- `clank agent start <label>` for an agent that's in the
  merged declaration but has no skeleton in this repo
  spawns the bare tool with the seed prompt instead of
  erroring.
- `clank agent start <label>` for an agent NOT in the
  declaration continues to error with "no such agent".
- `clank open zellij` in a fresh repo (no per-agent
  skeletons) opens all declared-agent panes successfully;
  each pane greets with the bootstrap prompt.
- After the agent runs `clank as <label>` once, subsequent
  `clank agent start <label>` calls resume the session
  normally (existing behavior preserved).
- `cargo test --workspace` passes.

## Related

- `agent-config-and-start` (FINISHED `18a5e78`): shipped
  `clank agent start` with the strict "must have bound
  session" model. This plan loosens the strict-session
  requirement to allow bootstrap.
- `agent-start-initial-prompt` (FINISHED `fee340d`):
  added `DEFAULT_AUTO_PROMPT` for auto-mode-On resume.
  `BOOTSTRAP_BIND_PROMPT` is a sibling constant — same
  pattern, different trigger.
- `clank-open-zellij-layout-file` (FINISHED `c3f0ac5`):
  shipped `clank open zellij`. This plan fixes the
  fresh-repo case that wasn't surfaced when the layout
  plan shipped.
- POC `open-worktree.sh`: existing shell script that
  already implements the bootstrap-seed-prompt pattern.
  This plan brings that into the official `clank agent
  start` surface.
