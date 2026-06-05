# agent-start-initial-prompt
# Add a configurable `launch.initial_prompt` for `clank agent start`. Default to a stop-hook-firing prompt when the agent's `auto_mode == On`; no prompt when off.

## Problem

lloyd 2026-06-05: "It doesn't ping them to start the stop hook. Is there any way, when you resume you could pass an initial prompt to claude and codex 'resuming session' or something like that."

Today `clank agent start <label>` composes:

- claude: `claude [launch.args] --resume <id>`
- codex:  `codex [launch.args] resume <id> --cd <repo>`

Both tools resume their session interactively but wait for the user to type something. The stop hook only fires after a turn ENDS, so the work loop sits idle until the user manually pokes the agent. When a tab opens via `clank open zellij`, the panes resume their sessions but nothing happens until the user clicks each one and types.

Both CLIs accept a trailing prompt positional argument (verified 2026-06-05):

- `claude [options] [command] [prompt]` (`claude --help`)
- `codex resume [OPTIONS] [SESSION_ID] [PROMPT]` (`codex resume --help`)

If we append a one-line prompt after the resume args, the agent processes that prompt, ends a turn, and the stop hook fires `clank wfw` — which kicks the work loop without manual intervention.

lloyd directive: "Sure we can make it configurable but we should have a sane default that starts the stop hook *IF* auto is on for that agent."

## Verified before promotion (2026-06-05)

- `LaunchConfig` at `crates/core/src/agent_config.rs` carries `command: Option<String>`, `args: Vec<String>`, `env: BTreeMap<String, String>`. Adding `initial_prompt: Option<String>` follows the same shape; `RepoConfigFile.extra` flatten catchall preserves it on round-trip per `typed-config-dogfood`.
- `compose_launch` at `crates/cli/src/cli/agent.rs:212-232` takes `(repo, session, launch)`. Auto_mode lives on the full `AgentConfig`, which the caller `agent_start` ALREADY loads via `load_agent_config`. The auto_mode just isn't threaded into `compose_launch` today.
- `session_restore_args` at `:237-247` returns the suffix that goes AFTER `launch.args`. To inject a prompt AFTER session-restore, the composition becomes `[launch.args] + session_restore + [maybe initial_prompt]`. For codex, the `--cd <repo>` option moves to be BEFORE the prompt so the prompt is the final positional arg.
- `AgentConfig.auto_mode` is `AutoMode::Off | AutoMode::On`. Per-agent in `<repo>/.clank/agents/<label>/config.json`.
- `clank agent add` has `--launch-cmd`, `--launch-arg`, `--launch-env`. Adding `--launch-initial-prompt <STRING>` follows the same pattern.

## Approach

### Phase 1: Schema — `launch.initial_prompt`

- Add `initial_prompt: Option<String>` to `LaunchConfig` (skip serialization when None).
- Add `--launch-initial-prompt <STRING>` to `clank agent add` (and any other writer that touches launch fields).

### Phase 2: Composition — append after session-restore

In `compose_launch`:
- Resolve effective prompt:
  - If `launch.initial_prompt` is set: use it verbatim.
  - Else if `agent_config.auto_mode == AutoMode::On`: use a default prompt that drives the work loop.
  - Else: no prompt (current behavior preserved).
- Append the resolved prompt (when present) AFTER session-restore args.

For codex, position the `--cd <repo>` option before the prompt positional so the prompt is the final argv slot. Both clap (codex) and commander.js (claude) parse options independently of positional args, so option order is flexible — putting `--cd` between `resume <id>` and the prompt is the safe shape.

### Phase 3: Default prompt content

When auto_mode is On and no explicit `initial_prompt` is configured, default to:

> `Resumed. Acknowledge and wait for the stop hook to drive the next turn.`

Rationale:
- Short enough that the agent processes it in one turn (target: a one-line reply, then turn ends).
- **Does NOT instruct the agent to run `clank wfw` itself.** The stop hook is the orchestrator. If the prompt told the agent to run wfw, the agent would: (a) run wfw and process the work in one turn, (b) end turn, (c) the stop hook would fire wfw a SECOND time. Avoidable double-trigger. Cleaner architecture: prompt just triggers the turn-end; stop hook drives the loop.
- Explicit about the resume context so the agent doesn't try to enumerate plans or make up work.
- Generic — applies equally to master + reviewer roles.

Pin at promotion-time: alternative phrasings reviewers may want. The architectural property to preserve: **prompt triggers turn-end; stop hook drives the work loop. Don't make the agent run wfw itself.**

### Phase 4: Threading auto_mode through to `compose_launch`

`compose_launch`'s signature grows to `(repo, session, launch, auto_mode)`. The caller (`agent_start::run`) already has the full `AgentConfig`; pass `cfg.auto_mode` alongside `cfg.launch.as_ref()`.

Pinned: pass `auto_mode: AutoMode` directly (NOT `&AgentConfig`). compose_launch is a pure projection — keeping dependencies explicit at the signature beats passing the whole struct "in case future fields need threading." If a second field eventually needs to flow through, refactor at that point. YAGNI now.

### Phase 5: Tests

Unit tests in `cli::agent::tests`:
1. `compose_launch_appends_initial_prompt_when_set`: launch has `initial_prompt: "custom"`; argv ends with `"custom"`.
2. `compose_launch_uses_default_prompt_when_auto_on_and_no_explicit`: auto_mode=On, no initial_prompt → argv ends with the default prompt.
3. `compose_launch_omits_prompt_when_auto_off`: auto_mode=Off, no initial_prompt → argv has no trailing prompt (current behavior preserved).
4. `compose_launch_explicit_prompt_wins_over_auto_default`: auto_mode=On AND explicit `initial_prompt` set → explicit wins.
5. `compose_launch_codex_prompt_after_cd`: codex composition has `--cd <repo>` BEFORE the prompt so the prompt is the final arg (positional). Or after — doesn't matter for codex's parser, but the test pins the shape.

Integration test:
6. `agent_start_initial_prompt_lands_in_composed_print`: via `clank agent start <label> --print`, configure an agent with auto_mode=On + tool=claude; assert stdout ends with the default prompt single-quoted.

### Out of scope

- Prompts that vary per-role (master vs reviewer) without explicit configuration. v1: single default for all auto_mode=On agents; users override per-agent via `--launch-initial-prompt`.
- Stdin-based prompt injection. CLI positional arg is the path of least resistance; stdin would require process spawn handling changes.
- Re-running with a different prompt after an existing session has resumed. Out of band.

## Acceptance

- `LaunchConfig.initial_prompt: Option<String>` is a typed field; round-trips through `RepoConfigFile` / `UserConfigFile` via existing serde flatten catchall.
- `clank agent add --launch-initial-prompt "..."` writes the field correctly (verified by reading the declaration back).
- `clank agent start <label> --print` for an agent with auto_mode=On (no explicit initial_prompt) shows the default prompt as the trailing positional arg.
- `clank agent start <label> --print` for an agent with auto_mode=Off (no explicit initial_prompt) shows NO trailing prompt — current behavior preserved exactly.
- An explicit `initial_prompt` wins over the auto-default regardless of auto_mode value.
- When the spawned tool exits the prompt's turn, the stop hook fires `clank wfw` (acceptance is testable manually via real tool — the wiring lives elsewhere; this plan just lands the argv shape).
- `cargo test --workspace` passes.

## Related

- `agent-config-and-start` (FINISHED `18a5e78`): shipped `clank agent start` + the `launch.args` extension mechanism this plan adds a sibling field to.
- `clank-open-zellij-layout-file` (in review at `841cdf2`): consumes `clank agent start` per pane. The initial-prompt change lands in the launch path so layouts automatically benefit without any KDL changes.
