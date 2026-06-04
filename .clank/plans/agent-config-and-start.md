# agent-config-and-start
# Add `AgentConfig.launch` profile fields + `clank agent start <name>` command. Unblocks `clank open zellij`.

## Rescope notice (2026-06-05)

The original sketch had five phases (repo-scope config, AgentConfig launch fields, `clank agent add/remove/set-role`, `clank agent start <name>`, gate tighten). Two findings tighten this:

1. **Phase 5 (gate tighten) is already done.** `compute_gate` at `crates/core/src/wait.rs` returns `CommitGateState::Finished` only when every expected reviewer's latest verdict is `Verdict::Finished` (the `all_finished` check at the bottom of the function). `compute_finalize_readiness` at `crates/cli/src/preview.rs` requires `CommitGateState::Finished` for finalize. So "require APPROVE/FINISH from all agents" IS already the existing behavior. No change needed.

2. **Phase 1 + 3 are nice-to-have, not blocking for open-zellij.** The critical path for the user's stated goal (unblock `clank open zellij`) is **Phase 2 (AgentConfig launch field) + Phase 4 (`clank agent start <name>`)**. Repo-scope `agents` field (Phase 1) and `clank agent add` CLI (Phase 3) are downstream improvements that don't block the zellij integration; users can hand-edit `<repo>/.clank/agents/<label>/config.json` to set launch fields in the meantime.

This plan is therefore narrowed to **Phase 2 + Phase 4**. Phase 1 and Phase 3 belong in a follow-up plan named something like `agent-add-cli-and-repo-scope`. Documented here as an explicit deferral so the queued follow-up has a known shape.

## Problem

`AgentConfig` schema at `crates/core/src/agent_config.rs` has four fields today: `auto_mode`, `role`, `wfw_timeout`, `session`. Nothing about *how to launch* the agent — no command override, no args, no profile/skill semantics.

Consequence: any per-agent spawner (the POC `open-worktree.sh`, future `clank open zellij`, scripts, IDE integrations) can only invoke the bare CLI binary (`claude`, `codex`) with a generic seed prompt. If the user wants:

- `ruthless` launched as `claude --skill ruthless`
- `codex-deep` launched as `codex --profile deep`
- `lloyd` launched as plain `claude` with no extras

…there's no per-agent config to express that. The spawner has to hardcode tool-name-to-command mappings or punt to "the user manually configures their session every time."

Same gap blocks a useful invariant: "if `clank agent start <name>` works, every spawner gets the right launch behavior for free." Without that command, every spawner re-implements lookup + dispatch.

## Verified before promotion (audit 2026-06-05)

- **AgentConfig schema**: confirmed four fields (`auto_mode`, `role`, `wfw_timeout`, `session`) at `crates/core/src/agent_config.rs`. `#[serde(default, skip_serializing_if = "Option::is_none")]` on `wfw_timeout` and `session` — same pattern works for `launch`.
- **POC script reference**: `open-worktree.sh` in repo root invokes `command "$MASTER_TOOL"` with a hardcoded seed prompt. No way to thread `--skill` or `--profile` through.
- **Tool detection**: `Session.tool: Tool` enum is `Claude | Codex`. Default for `LaunchConfig.command` (when None) can fall back to the tool's bare name.
- **No `LaunchConfig` exists today**: greenfield struct.
- **Session restore**: claude CLI supports `claude --resume <session-id>`. Codex CLI supports `codex resume <id> --cd <dir>` (per the worktree-workflow-research findings). Both stable. `clank agent start` composes them when a session is bound; runs the bare tool when not.
- **Gate semantics**: verified already requires all-FINISH (see Rescope notice). No change in scope.

## Approach

### Phase A — extend `AgentConfig` with `launch` field

In `crates/core/src/agent_config.rs`:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AgentConfig {
    // ...existing fields...
    /// Override how `clank agent start <name>` invokes the
    /// agent's tool. `None` defaults to the bare tool name
    /// (claude / codex) with no extra args.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch: Option<LaunchConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LaunchConfig {
    /// Override the executable. `None` falls back to the
    /// session-tool's bare name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Args passed to the executable (after session-restore
    /// args if a session is bound).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Env vars merged onto the exec environment.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}
```

- Schema migration: existing configs without `launch` deserialize fine (Option default).
- No CLI surface in THIS plan — users hand-edit `<repo>/.clank/agents/<label>/config.json` to populate fields. The `clank agent add --skill ... --profile ...` sugar belongs in the follow-up plan.

### Phase B — `clank agent start <name>` subcommand

Add to `Command` enum + `cli::agent::AgentCmd`:

```
clank agent start <name>
```

Behavior:

1. **Resolve `<name>`** via `agent_store::load_agent_config(repo, label)`. Error if absent: "no agent `<name>` in this repo — run `clank agent add` (when shipped) or create `.clank/agents/<name>/config.json`."

2. **Determine session-tool**: from `cfg.session.tool` if bound, else error: "agent `<name>` has no bound session. Run `clank as <name>` from inside the agent's CLI to bind."

3. **Compose the command**:
   - executable = `cfg.launch.command.as_deref().unwrap_or(tool.as_str())`
   - args = session-restore prefix (if bound) + `cfg.launch.args`
     - claude bound: `["--resume", <session-id>]`
     - codex bound: `["resume", <session-id>, "--cd", <repo>]`
   - env = process env ∪ `cfg.launch.env`

4. **`exec` into the composed command.** On unix: `std::os::unix::process::CommandExt::exec` replaces the calling process. The agent's CLI is now in this terminal pane.

5. **`--print` flag** (optional convenience): print the composed command + env diff instead of execing. Useful for `clank open zellij` if it wants to construct layout commands rather than exec directly. Implementer's call whether to include in this plan or follow-up.

### Phase C — doctor check

Extend `clank doctor`'s repo-scope agents section with: "If `cfg.launch` is set but the executable isn't on `$PATH`, Warn." Catches typos in `launch.command` before the user tries to start the agent.

## Out of scope

- Repo-scope `<repo>/.clank/config.json` `agents` field. Follow-up plan owns this.
- `clank agent add/remove/set-role` CLI. Same follow-up.
- `clank open zellij` itself. This plan unblocks it; that's a separate plan that consumes `clank agent start`.
- Schema migration for existing configs. The new `launch` field is Optional + Default; existing configs deserialize fine.
- Multi-tool agents (e.g., "fall back to claude if codex is unavailable"). One agent, one tool.
- `clank agent start --print` JSON output for non-shell consumers. Plain-text print is sufficient if included.

## Acceptance

- `AgentConfig` has a new optional `launch: Option<LaunchConfig>` field. Schema unchanged for configs that don't set it.
- `clank agent start <name>` with no `launch` config + bound claude session execs into `claude --resume <session-id>`.
- `clank agent start <name>` with no `launch` config + bound codex session execs into `codex resume <session-id> --cd <repo>`.
- `clank agent start <name>` with `launch = { command: "claude", args: ["--skill", "ruthless"] }` execs into `claude --resume <session-id> --skill ruthless` (session-restore args precede launch args).
- `clank agent start <name>` with no bound session errors out asking the user to `clank as <name>` first.
- `clank agent start <name>` for an unknown name errors with a clear diagnostic.
- `clank doctor` Warns if an agent's `cfg.launch.command` isn't on `$PATH`.
- `cargo test --workspace` passes.

## Tests

Unit tests in `crates/core/src/agent_config.rs`:

- `launch_field_deserializes_when_absent`: roundtrip an AgentConfig JSON without `launch`; assert `cfg.launch.is_none()` and re-serialization doesn't emit `"launch": null`.
- `launch_field_deserializes_when_present`: roundtrip with `{"launch": {"command": "claude", "args": ["--skill", "ruthless"]}}`; assert fields land correctly.
- `launch_config_default_is_empty`: `LaunchConfig::default()` has None command, empty args, empty env.

Integration tests in a new `crates/cli/tests/agent_start_integration.rs`:

- `agent_start_with_bound_claude_session_execs_claude_resume`: cannot directly assert exec (process replacement), so use a `--print` flag (or wrap exec in a testable indirection) to verify the constructed command.
- `agent_start_no_launch_config_uses_bare_tool`: same pattern, assert `claude --resume <id>` is the constructed command.
- `agent_start_with_launch_args_appends_after_session_restore`: `["--skill", "ruthless"]` lands AFTER the `--resume <id>` args.
- `agent_start_unknown_agent_errors`: clear diagnostic, exit non-zero.
- `agent_start_no_session_errors_with_clank_as_hint`: bound-session-required diagnostic.

Doctor tests:

- `doctor_warns_when_launch_command_missing_from_path`: `cfg.launch.command = "definitely-not-installed"` triggers a Warn entry naming the agent + command.

## Open questions (resolve at implementation)

- **`--print` mode**: include in this plan or follow-up? Argues for inclusion: unblocks zellij + provides testability. Argues against: small extra surface. Lean toward **include**.
- **Default args policy**: should `launch.args` be the FULL arg list (caller can include flags before session-restore) or just APPEND after session-restore? Plan body says append; verify against the most common `claude --skill X --resume <id>` shape. If users want `--skill` BEFORE `--resume`, the order matters.
- **What happens when the calling process exits before exec succeeds?** `exec` semantics: the new program replaces the current process; failures (e.g., `command` not found) propagate as the standard exec error and the user sees them. No special handling needed.

## Related history

- `clank-init-seeds-default-agents` (FINISHED): user-scope `default_agents` + init seeding. Reads `~/.clank/config.json` `default_agents`.
- `manage-clank-agents` (FINISHED, trimmed): `clank agent list` + doctor unbound-reviewer warning. The `add/remove/set-role` write commands were deferred — this plan's follow-up owns them.
- `all-reviewers-gate` (FINISHED): gate-tightening confirmed already at the all-FINISH threshold. No change needed here.

## Unblocks

- `clank-open-zellij` (queued as a separate stub): once `clank agent start <name>` lands, the zellij layout just spawns it per pane with no per-agent special-casing.
