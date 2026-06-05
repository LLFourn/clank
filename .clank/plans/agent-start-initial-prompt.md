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

- **`LaunchConfig` is SHARED between `clank agent start` and `clank diff --editor`** (codex 527eaec catch). Its docstring at `crates/core/src/agent_config.rs:57-78` explicitly documents two consumers (agent start + diff editor) and warns: *"If future consumers need fields beyond `command/args/env`, split into a sibling struct rather than overloading this one."* Putting `initial_prompt` on `LaunchConfig` would pollute `Config.diff.editor` with a field that has no meaning for editor launches. **The right home for the prompt is on `DefaultAgent` itself** (peer to `launch`), not inside `LaunchConfig`.
- **`DefaultAgent`** at `crates/cli/src/cli/config.rs` already carries `{label, role, tool, launch}` and serializes via the `agents` array in `RepoConfigFile` / `UserConfigFile`. Adding `initial_prompt: Option<String>` is a sibling field; round-trips through the existing typed-config-dogfood infrastructure.
- `compose_launch` at `crates/cli/src/cli/agent.rs:212-232` takes `(repo, session, launch)`. Auto_mode lives on the full `AgentConfig`, which the caller `agent_start` ALREADY loads via `load_agent_config`. The prompt resolution (declaration-level explicit OR auto-mode default) happens in the CALLER; `compose_launch` accepts a pre-resolved `Option<&str>`.
- `session_restore_args` at `:237-247` returns the suffix that goes AFTER `launch.args`. To inject a prompt AFTER session-restore, the composition becomes `[launch.args] + session_restore + [maybe initial_prompt]`. For codex, the `--cd <repo>` option moves to be BEFORE the prompt so the prompt is the final positional arg.
- `AgentConfig.auto_mode` is `AutoMode::Off | AutoMode::On`. Per-agent in `<repo>/.clank/agents/<label>/config.json`.
- `clank agent add` has `--launch-cmd`, `--launch-arg`, `--launch-env`. The new flag is `--initial-prompt <STRING>` (NOT `--launch-initial-prompt`) since `initial_prompt` is on `DefaultAgent`, not inside the launch profile.

## Approach

### Phase 1: Schema — `DefaultAgent.initial_prompt`

- Add `initial_prompt: Option<String>` to `DefaultAgent` (sibling to `launch`), skip serialization when None.
- `LaunchConfig` stays unchanged (its docstring directive honored — no overloading of the shared editor/agent profile).
- Add `--initial-prompt <STRING>` to `clank agent add` (NOT `--launch-*` — the field isn't inside launch).

### Phase 2: Composition — append after session-restore

The prompt is **resolved by the caller** (`agent_start::run`), NOT inside `compose_launch`. compose_launch takes a pre-resolved `Option<&str>` and just appends it after session-restore.

Caller logic in `agent_start::run`:

```rust
let resolved_prompt: Option<String> = default_agent
    .initial_prompt
    .clone()
    .or_else(|| {
        if agent_config.auto_mode == AutoMode::On {
            Some(DEFAULT_AUTO_PROMPT.to_string())
        } else {
            None
        }
    });
let composed = compose_launch(repo, session, launch, resolved_prompt.as_deref());
```

compose_launch's new signature:

```rust
fn compose_launch(
    repo: &Path,
    session: &Session,
    launch: Option<&LaunchConfig>,
    initial_prompt: Option<&str>,
) -> ComposedLaunch;
```

When `initial_prompt` is `Some(s)`, append `s` after session_restore. For codex, position the `--cd <repo>` option before the prompt positional so the prompt is the final argv slot. Both clap (codex) and commander.js (claude) parse options independently of positional args, so option order is flexible — putting `--cd` between `resume <id>` and the prompt is the safe shape.

This split keeps compose_launch a pure projection that doesn't know about `DefaultAgent` or `AgentConfig`. The "where does the prompt come from" policy lives in the caller alongside the rest of agent_start's resolution logic.

### Phase 3: Default prompt content

When auto_mode is On and no explicit `initial_prompt` is configured, default to:

> `Resumed. Acknowledge and wait for the stop hook to drive the next turn.`

Rationale:
- Short enough that the agent processes it in one turn (target: a one-line reply, then turn ends).
- **Does NOT instruct the agent to run `clank wfw` itself.** The stop hook is the orchestrator. If the prompt told the agent to run wfw, the agent would: (a) run wfw and process the work in one turn, (b) end turn, (c) the stop hook would fire wfw a SECOND time. Avoidable double-trigger. Cleaner architecture: prompt just triggers the turn-end; stop hook drives the loop.
- Explicit about the resume context so the agent doesn't try to enumerate plans or make up work.
- Generic — applies equally to master + reviewer roles.

Pin at promotion-time: alternative phrasings reviewers may want. The architectural property to preserve: **prompt triggers turn-end; stop hook drives the work loop. Don't make the agent run wfw itself.**

### Phase 4: (folded into Phase 2)

Originally this phase was about threading `auto_mode` into `compose_launch`. After the codex 527eaec catch, prompt resolution moved to the CALLER and `compose_launch` just takes `Option<&str>`. So compose_launch doesn't need to know about `auto_mode` at all — the caller resolves the prompt based on auto_mode + declaration field, then passes the resolved string.

Net signature delta on compose_launch: one new param (`initial_prompt: Option<&str>`), zero `AgentConfig`/`AutoMode` knowledge. Simpler than the original plan.

### Phase 5: Tests

Unit tests in `cli::agent::tests` (test `compose_launch` directly — pure projection):
1. `compose_launch_appends_initial_prompt_when_set`: pass `Some("custom")`; argv ends with `"custom"`.
2. `compose_launch_omits_prompt_when_none`: pass `None`; argv has no trailing prompt (current behavior preserved).
3. `compose_launch_codex_prompt_is_final_positional_after_cd`: codex composition has `--cd <repo>` BEFORE the prompt so the prompt is the final arg.

Unit tests for the caller's prompt-resolution policy in `cli::agent::tests` (test the resolution helper, not compose_launch):
4. `resolve_initial_prompt_uses_declaration_field_when_set`: DefaultAgent.initial_prompt = Some("foo"), auto_mode = Off → "foo".
5. `resolve_initial_prompt_uses_default_when_auto_on_and_declaration_unset`: DefaultAgent.initial_prompt = None, auto_mode = On → DEFAULT_AUTO_PROMPT.
6. `resolve_initial_prompt_returns_none_when_auto_off_and_declaration_unset`: DefaultAgent.initial_prompt = None, auto_mode = Off → None.
7. `resolve_initial_prompt_declaration_wins_over_auto_default`: DefaultAgent.initial_prompt = Some("custom"), auto_mode = On → "custom".

Integration test:
8. `agent_start_initial_prompt_lands_in_composed_print`: via `clank agent start <label> --print`, configure an agent with auto_mode=On + tool=claude; assert stdout ends with the default prompt single-quoted.

### Out of scope

- Prompts that vary per-role (master vs reviewer) without explicit configuration. v1: single default for all auto_mode=On agents; users override per-agent via `--initial-prompt`.
- Stdin-based prompt injection. CLI positional arg is the path of least resistance; stdin would require process spawn handling changes.
- Re-running with a different prompt after an existing session has resumed. Out of band.

## Acceptance

- `DefaultAgent.initial_prompt: Option<String>` is a typed field; round-trips through `RepoConfigFile` / `UserConfigFile` via existing serde infrastructure.
- `LaunchConfig` schema UNCHANGED — its docstring directive ("split rather than overload") is honored. `Config.diff.editor` cannot accept `initial_prompt` (it's not on LaunchConfig).
- `clank agent add --initial-prompt "..."` writes the field correctly (verified by reading the declaration back).
- `clank agent start <label> --print` for an agent with auto_mode=On (no explicit initial_prompt) shows the default prompt as the trailing positional arg.
- `clank agent start <label> --print` for an agent with auto_mode=Off (no explicit initial_prompt) shows NO trailing prompt — current behavior preserved exactly.
- An explicit `initial_prompt` on the declaration wins over the auto-default regardless of auto_mode value.
- When the spawned tool exits the prompt's turn, the stop hook fires `clank wfw` (acceptance is testable manually via real tool — the wiring lives elsewhere; this plan just lands the argv shape).
- `cargo test --workspace` passes.

## Related

- `agent-config-and-start` (FINISHED `18a5e78`): shipped `clank agent start` + the `launch.args` extension mechanism this plan adds a sibling field to.
- `clank-open-zellij-layout-file` (in review at `841cdf2`): consumes `clank agent start` per pane. The initial-prompt change lands in the launch path so layouts automatically benefit without any KDL changes.
