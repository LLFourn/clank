# clank-as-help-names-every-tool
# clank as --help names every tool

## Problem

`clank as --help` says it "Reads the tool's session id from the environment (CLAUDE_CODE_SESSION_ID / CODEX_THREAD_ID)" (`crates/cli/src/cli/command.rs:304`). `agent_env.rs` also reads `GROK_SESSION_ID` and `OPENCODE_SESSION_ID`. Observed 2026-10-27 in `~/tmp/oc-live`: an OpenCode agent (Qwen 3.8), told to run `clank as qwen`, read the help, concluded clank might not support OpenCode, and spent its turn investigating its environment instead of binding.

## What changes

The help text names every harness clank binds and the variable it reads for each: Claude Code `CLAUDE_CODE_SESSION_ID`, Codex `CODEX_THREAD_ID`, Grok `GROK_SESSION_ID`, OpenCode `OPENCODE_SESSION_ID` (set by OpenCode itself in its bash tool's shells). Derive the list from `agent_env`'s table rather than a second hand-written copy, if the help can be built from it; otherwise add a test that every tool in that table appears in the help.

Sweep the same claim elsewhere: README, the clank skills' `clank as` text, and the launch prompt that tells an agent to run `clank as`.

## Acceptance

- `clank as --help` lists all four variables (test through `cli::command`).
- A test fails if a tool is added to `agent_env`'s session table without appearing in the help.
