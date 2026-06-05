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
that is registered in the merged declaration but **not yet
bound to a session in this repo**, spawn the bare tool with
a seed prompt asking the agent to bind itself via
`clank as <label>`. "Not yet bound" covers both:
- **Missing skeleton**: no
  `<repo>/.clank/agents/<label>/config.json` (the case
  lloyd hit in frostsnap, where ruthless was declared
  globally but never bound there).
- **Session-less skeleton**: skeleton EXISTS with
  `session: None` (the fresh-init path: `clank init`
  seeds default_agents this way).

After the first turn, `clank as` binds the session, and
subsequent `clank agent start <label>` calls resume
normally.

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
4. If declaration entry exists AND skeleton exists AND
   `skeleton.session.is_some()` → today's resume behavior
   (compose launch with `--resume <session-id>`).
5. **NEW (BOOTSTRAP)**: declaration entry exists but EITHER
   (a) skeleton is missing OR (b) skeleton exists with
   `session: None`. Both are "agent registered, never bound
   in this repo." Both lead to the bootstrap path. Codex
   eef6853 catch: `clank init` already seeds default_agents
   as session-less skeletons (`init.rs:47-61`, `:94-98`), so
   case (b) is the freshly-init'd-repo path and (a) is the
   declared-globally-but-never-init'd path. Existing test
   `agent_start_no_bound_session_errors_with_clank_as_hint`
   at `agent_start_integration.rs:311-345` needs to flip
   from asserting-error to asserting-bootstrap-prompt.

   Bootstrap launch composition:
   - **Tool resolution** (PINNED per codex eef6853):
     priority order — declaration's `launch.command` (if set,
     used verbatim); else declaration's `tool` field (claude
     / codex); else **ERROR** with an actionable hint
     (codex 5428550 catch: today's `clank agent add` refuses
     duplicate labels and there is no `set-tool` subcommand,
     so the hint must point at a path the user can actually
     take). The pinned message:
     ``agent <label> has no bootstrap tool. Either edit
     <config-path>.json to add `"tool": "claude"` (or
     "codex") under this agent, or remove and re-register:
     `clank agent remove <label> && clank agent add <label>
     --tool <claude|codex>`.``
     Don't fall back to label-as-tool-name — that masks
     misconfiguration.
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

Unit tests on `compose_bootstrap_launch`:

- `bootstrap_uses_tool_from_declaration`: declaration with
  tool=claude + launch.args=["--skill", "ruthless"]; assert
  argv is `["claude", "--skill", "ruthless", "<bootstrap-prompt>"]`
  (no --resume, prompt as final positional).
- `bootstrap_prefers_launch_command_over_tool`: declaration
  with launch.command="my-claude-wrapper" + tool=claude;
  assert argv[0] is "my-claude-wrapper", not "claude".
- `bootstrap_errors_when_no_tool_or_command`: declaration
  with tool=None and launch.command=None; assert the error
  message names the agent label AND mentions
  `--tool <claude|codex>` so the user knows the fix.

Integration tests in `crates/cli/tests/agent_start_integration.rs`:

- `agent_start_bootstraps_when_skeleton_missing`:
  declaration registers `phantom` (via typed `RepoConfigFile`);
  no skeleton at `<repo>/.clank/agents/phantom/config.json`;
  spawn `clank agent start phantom --print`; assert stdout
  argv ends with `<bootstrap-prompt>` and exit 0.
- `agent_start_bootstraps_when_skeleton_exists_but_session_none`
  (codex eef6853 catch — the fresh-init case): declaration
  registers `phantom`; skeleton EXISTS with `session: None`;
  spawn `clank agent start phantom --print`; assert same
  bootstrap argv shape AND exit 0. This is the path that
  `clank init` produces, so this test mirrors the user-
  visible fresh-init flow.
- **UPDATE existing test**
  `agent_start_no_bound_session_errors_with_clank_as_hint`
  at `agent_start_integration.rs:311-345` (codex a8164a1
  catch — original update broke the negative path).
  Today's setup writes ONLY an unbound skeleton with no
  repo/user declaration entry, so `load_merged_agents`
  legacy-synthesizes a declaration with `tool: None` from
  `config.rs:502-506` (the tool is inferred from
  `cfg.session`, which is None → tool=None). With tool=None
  the bootstrap path now correctly ERRORS with the
  no-bootstrap-tool hint. Keep this test, but rename to
  `agent_start_session_none_with_no_tool_errors_with_hint`
  and assert the no-bootstrap-tool error message (label name
  + tool-fix hint), NOT the legacy "no clank as" wording.
  This continues to defend the legacy-skeleton-without-tool
  negative path.

Negative tests (existing behavior preserved):

- Agent NOT in declaration → today's "no such agent" error
  still fires.
- Declaration entry's launch has no command AND tool=None →
  bootstrap errors with the named-agent + tool-fix hint.
  (The renamed
  `agent_start_session_none_with_no_tool_errors_with_hint`
  above covers this for the legacy-skeleton synthesis
  variant.)

## Out of scope

- **`clank as <label>` failure mode** (ruthless 3ab2def pin):
  if the bootstrap agent runs `clank as` and it fails
  (session-id detection bug, daemon error, etc.), the agent's
  reply surfaces the failure to the user. The next
  `clank agent start <label>` re-attempts bootstrap because
  the skeleton/session state remains unchanged. No automatic
  retry / fallback logic in v1.
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

All bootstrap criteria are gated on the declaration
resolving a bootstrap program (`launch.command` set OR
`tool` set). Without a resolvable program, the no-bootstrap-
tool error path applies — codex a8164a1 catch: the prior
acceptance unconditionally asserted "always bootstraps,"
which contradicted the no-tool negative path.

Bootstrap path (resolvable program present):
- **(a) Missing-skeleton case**: `clank agent start <label>`
  for a declared agent with a resolvable bootstrap program
  and no `<repo>/.clank/agents/<label>/config.json` spawns
  the bare tool with the seed prompt instead of erroring.
- **(b) Session-none case** (codex 5428550 catch — must be
  in acceptance too): same for a declared agent with a
  resolvable bootstrap program and skeleton-with-`session:
  None`. Implementation cannot satisfy this while leaving
  fresh-init repos with declared `tool` broken.

Negative paths preserved:
- `clank agent start <label>` for an agent NOT in the
  declaration continues to error with "no such agent".
- `clank agent start <label>` for a declared agent WITHOUT
  a resolvable bootstrap program (no `launch.command` AND
  `tool: None`) surfaces the actionable no-bootstrap-tool
  error hint with the edit-or-remove-and-re-add path.
  Includes the legacy-skeleton-without-tool synthesis case
  via `config.rs:502-506`.

End-to-end:
- `clank open zellij` in any repo where some declared
  agents are unbound (either case (a) or (b)) with a
  resolvable bootstrap program opens those panes
  successfully; unbound panes greet with the bootstrap
  prompt; bound panes resume normally. Panes for declared
  agents WITHOUT a resolvable program surface the error
  rather than bootstrapping silently.
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
