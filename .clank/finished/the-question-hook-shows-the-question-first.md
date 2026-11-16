# the-question-hook-shows-the-question-first

## Why

The question hook spawns or attaches a monitor before the person sees the question, so monitor startup can delay the prompt. It also prints "could not be routed" for PermissionRequest events, which carry no question-state observation.

Review of `wire-changes-bump-the-protocol` set the contract for fixing this:

- `question_hook::run` submits PreToolUse `Asked` evidence before contacting the question broker.
- Moving that submission after the broker's reply could delay `Asked` until after the person answers, since a PermissionRequest can park.
- Deleting it would remove the evidence that keeps recovery from nudging into an open question.

## Build

Today `question_hook::run` first calls `attend::submit`, which uses `attach_or_spawn_with` and so can start a monitor, waiting up to `ATTACH_BUDGET`. Only then does it contact the question broker. Claude shows nothing until the hook process exits, so monitor startup delays the prompt.

1. **Filter before routing.** `evidence()` matches the event first. Only `PreToolUse`, `PostToolUse` and `PostToolUseFailure` with a `tool_use_id`, for `AskUserQuestion`, go on to repo and identity resolution. `PermissionRequest` and every other event return `None` before any lookup, so they print no routing diagnostic. A routing failure for an event that does carry evidence still prints one.
2. **Evidence never waits in front of the prompt.** The hook hands the observation to a detached child, `clank question-hook --deliver` (hidden), with the `Control::Questions` request on its stdin. The child runs in its own session and process group with no inherited stdout, and the hook does not wait for it. The child calls `attend::submit` as today, starting a monitor if none runs, and reports a failure to its own stderr log only.
   - The hook then contacts the broker at once and prints its reply, or `{}` with no broker: the native UI path.
   - The spawn goes through the existing process layer (`proc_run`), within `process_boundary`.
3. **Ordering is already safe.** `question_state::apply` is order-independent for one key. A `Result` that lands before its `Asked` puts the key in `returned`, and the later `Asked` is then not pending. So a detached `Asked` that arrives after the person answered cannot leave a phantom open question. A test pins this.
4. The hook still reads no monitor-owned state or config. The child carries only the request the hook built from its own stdin and binding.

## Acceptance

- With no monitor running and a broker present, the hook prints the broker's reply without waiting for monitor startup. Test it with a broker fixture and a spawn seam that records the child but never completes.
- The `--deliver` path, given the request, submits it through `attend::submit`. Test it in-process.
- `Asked` evidence reaches question state while the question is open. Recovery's question gate then holds the agent (existing recovery tests over question state).
- `Result` before `Asked` for one key leaves nothing pending.
- `PermissionRequest`, with or without a resolvable binding, prints no routing diagnostic. A `PreToolUse` whose binding cannot be resolved still does.
