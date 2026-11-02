# opencode-takes-no-model-flag
# The README stops telling OpenCode agents to pass --model

OpenCode 2.x's interactive command rejects `--model` ("Unrecognized flag:
--model in command opencode", 2.1.4). Only `opencode run` still takes it.
The README's opencode bullet says "The launch profile carries the model"
and recommends `clank agent add kimi --tool opencode -- --model
moonshotai/kimi-k3`. Following it broke Lloyd's kimi agent.

Change: replace that sentence with what is true. The TUI takes no
`--model`. A session records its model (`opencode session export` shows
`info.model`), so resuming one with `--session` keeps that model. For an
agent that must start NEW sessions on its own model, the profile is
`--standalone` plus `--env OPENCODE_CONFIG_CONTENT={"model":"…"}`.
Verified live on 2.1.4:
- On the shared background service, the variable is ignored: a new
  session opened on the default model.
- With `--standalone`, a new session opened on Kimi K3.
- `--standalone --session <existing id>` opened kimi's real session with
  its full history, so the session store is the same.

The cost goes in the README too: `clank fork` refuses a `--standalone`
profile (`opencode_fork::validate_profile`).

The example becomes:
`clank agent add kimi --tool opencode --env
'OPENCODE_CONFIG_CONTENT={"model":"moonshotai/kimi-k3"}' -- --standalone`.

Acceptance: the README carries no `--model` advice for opencode. The
`cli::command` README tests and `book_is_true` pass.
