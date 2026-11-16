# Codex background work in the actual TUI

Follow-up to [the instrumentation investigation](codex-integration.md), measured
on 2026-11-13 with **Codex 0.167.2**, macOS arm64. These are opt-in diagnostic
fixtures, not a production launch change. No live agent or global configuration
was changed.

**Answer:** Stop does not enumerate background work. For an unregistered native
shell command, a client connected to the **server actually hosting the TUI's
thread** can discover the running command, receive its exit, and release our
synchronous Stop hook to continue that same agent. This works without `clank run`.
The current `--no-daemon` launch does not expose that external connection.
A private per-agent server is a tested alternative to the shared daemon.
It is not yet a complete passive observer of arbitrary Code Mode cells.

**What clank does with this:** nothing native. That server is a new socket,
which clank's remote gate forbids, and observing undeclared commands would be
background-work detection, which clank does not do. On Codex, clank itself
tells the agent when a `clank run` ends
(`clank_core::harness::Behaviour::reports_background_completion`).
Native background commands never wake a parked Codex agent.

## Answer table

| Question/path | Evidence and identity | Completion/reconnect | Limit and action |
| --- | --- | --- | --- |
| Stop payload | Captured with a live command: `session_id`, `turn_id`, `cwd`, `transcript_path`, `hook_event_name`, `model`, `permission_mode`, `stop_hook_active`, `last_assistant_message`. | It is a turn boundary, not a task snapshot. | No command list, process IDs, tool-call IDs, cell IDs or child inventory. A transcript path is not such an inventory. |
| Native `exec_command` without `clank run` | `PreToolUse` names `Bash`, command, `tool_use_id`, session and turn. Native inventory supplies `itemId`, `processId`, command and cwd; `osPid` was null here. | `item/completed` / `commandExecution` supplies item, concrete thread/turn, exit code and output. Reconnect to the same loaded server recovered completed items in history. | Do not require an OS PID or infer success from disappearance. Subscribe plus reconcile snapshots; never poll for completion. |
| `exec` wrapping `tools.exec_command` | Outer custom call and inner native command have distinct IDs. The structured inner result contains `session_id`; native inventory and events name the inner command. | Same command completion path as direct shell calls. | Do not equate the outer call ID with the native command item ID. |
| Genuinely yielded Code Mode cell | Actual model input says `Script running with cell ID 1`. The initial native terminal snapshot can still be empty while the cell's inner command is awaiting its first yield. | Its inner command emits a native exit. That does **not** establish the cell finished: the agent must collect the cell with `wait`. | No cell inventory/completion method was found in the generated 0.167.2 public app-server schema. Native terminal observation covers shell work within a cell, not arbitrary JavaScript or non-shell work. |
| Subagent | `SubagentStart` provides root `session_id` plus concrete child `agent_id`. `thread/read` supplies the child's `parentThreadId`; spawn history supplies receiver IDs. | `SubagentStop` supplies child ID/transcript and final text; the child's `turn/completed` confirms that turn ended. Reconnected child/parent history is readable. | Child turn completion is not child closure or proof all its descendants/processes ended. Root session ID is shared: route by concrete thread ID and relation. |
| Existing `--no-daemon` TUI | Embedded server with in-memory transport. | No supported external attach endpoint for that embedded instance was found. | Starting another server and loading its rollout is a different owner, not control of the original TUI. |
| Private server + `codex --remote unix:///…` | Actual TUI and independent observer use one disposable server. Observer first validates the loaded thread and cwd. | Native exit → matching hook decision → next actual agent model request succeeded. | Requires an intentional launcher change and ownership record; cannot combine `--remote` with `--no-daemon`. |
| Native input operations | `turn/steer` accepted the parked turn ID in the experiment. | No further agent request followed within two seconds until the hook was released. | Acceptance is not consumption. Interrupt is cancellation; await the terminal event before starting a replacement turn. |

## Why lifecycle hooks alone are insufficient

The versioned [Stop schema and producer][stop] agree with the captured payload.
`PreToolUse` runs before the shell command and therefore does not yet know its
background handle. More surprisingly, the shell output's
[`post_tool_use_response`][context] returns `None` while `process_id` is present.
In the live TUI probes, neither yielding the handle nor the later automatic exit
invoked `PostToolUse`. The foreground context command in the since-removed fork probe did invoke
it, so this is not a broken or untrusted hook fixture.

The [unified-exec exit watcher][exit] emits the completion event separately.
It does not invoke PostToolUse or start a model request. Thus adding a
PostToolUse hook is **not** sufficient to register every unwrapped background
shell process. Native hooks can be small fact relays, but cannot relay facts the
engine never supplies to them.

Subagents differ: the [SubagentStart/SubagentStop schemas][schemas] carry explicit
child identity. The experiment captured both with the parent parked and confirmed
the child's native terminal turn event. Neither that event nor SubagentStop
caused an automatic parent model request in the two-second observation window.
Releasing the parent's own Stop hook did. A future hook relay can use this
supported fact without reading monitor configuration inside the hook. It must
not treat SubagentStop as a successful exit of the entire subtree.

## The actual client boundary

The official [CLI reference](https://learn.chatgpt.com/docs/cli/reference)
documents `--remote`. The versioned CLI [rejects combining it with
`--no-daemon`][cli]. The embedded [in-process server][embedded] replaces socket
transports with channels, and the [TUI connection implementation][connection]
does not offer an external remote connection for that embedded runtime.

The tested arrangement was:

```text
per-agent launch environment + cwd + CODEX_HOME
  └─ private codex app-server --listen unix:///private/agent/control.sock
       ├─ codex --remote unix:///private/agent/control.sock  (real PTY/TUI)
       └─ independent Clank-shaped observer               (JSON-RPC client)
             verified command exit
               → reply to that session/turn's parked hook
                 → hook prints {"decision":"block","reason":"…"}
                   → same TUI thread makes its next model request
```

The fixture drives terminal capability responses only. It does not type a wake
into the terminal. The model is a scripted localhost Responses endpoint; there
is no external inference or account credential. The `bridge` case releases the
hook directly after the native completion, with **no timer or prompt triggering
the handoff**. Other cases deliberately hold the hook for a two-second negative
observation window before the explicit positive control.

The [documented Stop decision](https://learn.chatgpt.com/docs/hooks) continues the
model. The app-server observer does not have magical authority to execute a
blocked hook: it can see the lifecycle and the monitor can answer the hook it
already owns. `turn/steer` is not a substitute for that reply. An idle thread
needs a deliberate `turn/start`; an interrupted turn must first finish cancelling.
These are different policies and must not be retried interchangeably.

## Isolation

Two private servers ran concurrently, each with two commands, distinct temporary
profiles/cwds and harmless environment sentinels. Each command reported its
expected cwd/sentinel. Each observer verified its own concrete thread ID and
unique completion item IDs; one server's completions did not advance the other
model. Wrong-cwd attachment was rejected before subscription.

A fork probe once launched `codex --remote … fork -C DEST SOURCE_THREAD` from
Clank's real fork spec. Production launches Codex with `--no-daemon`, so the
probe measured a launch Clank does not make, and it was removed. One finding
from it stands: Codex refuses permission overrides when forking a remote task
("Permission overrides are not supported when forking a remote task."), because
a remote fork inherits the source's permission profile. A launcher that moves to
a private server must not append them on that path.

The earlier repository record in
`.clank/finished/a-codex-stop-hook-ends-with-its-session.md` documents a daemon
inheriting the first pane's environment and a hook seeing the wrong pane ID.
This experiment establishes the safer invariant: the **owning server** receives
that agent's environment and cwd before it creates the session. Changing only
the connecting TUI's environment cannot fix an already-running shared server.
The fork still needs explicit destination cwd; a shared session-tree identifier
must never substitute for the concrete child thread.

## Discovery after attachment and recovery

For a live owned server, subscribe to the already-loaded thread, buffer concurrent
notifications, then reconcile running terminals and paginated turn/item history.
The experiment also disconnects the monitor across a failing command's exit:
a separate audit subscriber establishes the exit barrier, is discarded, and a
new monitor connection recovers the completed item and exit code from history.
The audit notification is not passed to the recovering monitor.

This establishes same-server reconnect, **not** durable recovery of every event
after a server crash. Native inventory disappearance is ambiguous. Raw rollout
`ExecCommandEnd` is [classified transient][persistence]; a rollout carrying the
original tool call and its running result need not later contain that exit.
Use native history where present and retain a monitor-owned completion/receipt
record. If neither has an authoritative end, report outcome unknown and arrange
result collection; do not invent a success or run the command again.

Code Mode has internal [active-cell state][cells] and dispatch tracking, but those
are not the terminal inventory API. The [cell handler][cell-handler] returns a
running handle; its notification delegate uses [injection into a running
session][cell-delegate], which is different from starting an idle turn. A cell
may finish its wrapper while a yielded child command remains running, or remain
running after one inner command exits. Inferring one lifetime from the other is
wrong. The experiment retains both identities and explicitly collects the cell.

For an embedded TUI that must remain unchanged, there is no demonstrated complete
passive discovery route for arbitrary unregistered shell/cell work. Parsing
rollout tool results can recover handles using a version-specific decoder, but
cannot supply a missing exit fact or a stable cell inventory. Clank's notice
that a `clank run` ended is the limited fallback it ships; it does not solve
this question. For cell-only background work, the smallest honest operational fallback
is to keep/collect the native `exec`/`wait` invocation through completion, rather
than park expecting a passive monitor wake. Fully transparent coverage requires
an upstream supported cell inventory/completion surface (or a maintained engine
extension), not a new requirement to wrap everything in `clank run`.

## Reproduction and scope

The opt-in Rust test uses the existing `clank-test-fixture` binary. No Python,
runtime helper compilation, real model calls or live-team configuration is used.
Owned Codex process groups are killed/reaped on cleanup; command/hook barriers
have 20-second deadlines. Local startup readiness is bounded; the observer does
not repeatedly poll running-task snapshots.

```sh
CLANK_CODEX_PROTOCOL_BINARY=/absolute/path/to/codex \
CLANK_CODEX_PROBE_TRACE_DIR=/tmp/clank-codex-evidence \
cargo test -p clank --test it codex_question_protocol::tui:: -- --ignored --nocapture
```

`installed_codex_tui_background_probe` covers 32 private native/wrapper cases (active model, final-answer/hook-entry boundary,
parked hook, and idle/no-hook; exits 0/7; twice each); two simultaneous two-command
agents; a yielded cell; and immediate bridge, missed-exit reconnect, steering and
interrupt controls. The boundary is triggered by the final-answer event, not an
assertion about the exact instruction at which another process has parked.
A separate test covers a subagent. Each case asserts its negatives: no agent
request in the two-second window after a native exit (or, for the subagent, after
the child's completion) until the Stop hook is released, and no `PostToolUse` for
a yielded or exited background command. Embedded `--no-daemon` cases are not
run: with no external endpoint they cannot see the exit they would follow. Feature settings are explicit:
native uses `code_mode=false, code_mode_only=false`; wrapper/cell uses
`code_mode=true, code_mode_only=true, code_mode_host=true`.

The active-model case holds the second model response until the observer has
received a command-completed event and verified its thread, inventory item ID and
expected exit code. The trace records that event before `release_model_response`;
later collection skips the already-consumed exit. Merely signalling the command
to exit is insufficient: review found that the original diagnostic sometimes
completed the final answer first. Those original active-case traces do not prove
the intended ordering; the corrected eight-case replay replaces that evidence.
Replay just these native/wrapper × success/failure × two repetitions with
`CLANK_CODEX_TUI_CASE=active` and the background-probe test filter.

Corrected replay on 0.167.2 (milliseconds since each case started):

| Tool | Exit | Repeat | Exit received | Model response released | Stop received |
| --- | --- | --- | --- | --- | --- |
| Native | 0 | 0 | 750 | 750 | 768 |
| Native | 0 | 1 | 815 | 815 | 840 |
| Native | 7 | 0 | 746 | 746 | 765 |
| Native | 7 | 1 | 743 | 743 | 759 |
| Wrapped | 0 | 0 | 827 | 827 | 842 |
| Wrapped | 0 | 1 | 780 | 780 | 794 |
| Wrapped | 7 | 0 | 782 | 782 | 811 |
| Wrapped | 7 | 1 | 971 | 971 | 1006 |

Equal millisecond timestamps do not imply simultaneous operations: in all eight
traces the verified exit row precedes the model-release row, and the response
barrier is notified only afterwards. Each exit is consumed once. Replay and
Clippy passed; full local traces were retained under `/tmp/clank-active-order`.

Each case exports timestamped observations, actual agent model inputs and PTY
output. Thread-title inference requests are identified by their metadata and
excluded from agent-continuation counts: a title request initially looked like a
false positive. Two seconds without an agent request is a bounded observation,
not a universal liveness proof. The 0.167.1/0.167.2 engine comparisons in the first
report and this 0.167.2 TUI experiment still establish **no known-good version**,
so they do not prove a recent regression or a removed guarantee.

Validation on the installed 0.167.2 binary: all five protocol diagnostic tests
passed, including the 47-case TUI matrix, fork and subagent tests; the additional
cell-result collection check passed. Final focused TUI checks, formatting,
`cargo clippy -p clank --tests -- -D warnings` and 33 source boundary checks passed.
One immediate bridge trace recorded command completion at 974 ms, hook release
at 975 ms and agent request 3 at 983 ms after the case began. These timings are
illustrative observations, not latency guarantees or test thresholds.

## Upstream question

The demonstrated coverage is native shell work (direct or inside Code Mode) and
subagent turn lifecycle. Arbitrary cell lifetime, server-crash recovery and other
platform/version combinations would need explicit compatibility handling. Clank
does not pursue it (see the answer above).

Prepared upstream reproduction, not posted: run the native/wrapper parked and
idle cases above on 0.167.2. A native `commandExecution` completion arrives with
the correct exit status, but no agent request follows in the observation window;
an explicit Stop continuation does produce one. Ask whether that absence of
automatic continuation is intended, which client owns the scheduling obligation,
and whether Codex will expose supported cell inventory/completion and missing
terminal-end facts to integrations using embedded TUI hooks. Include the captured
hook payloads and native item/turn IDs, not a claim that an upgrade broke a
previously promised behavior.

[stop]: https://github.com/openai/codex/blob/rust-v0.167.2/codex-rs/hooks/src/events/stop.rs
[schemas]: https://github.com/openai/codex/blob/rust-v0.167.2/codex-rs/hooks/src/schema.rs
[context]: https://github.com/openai/codex/blob/rust-v0.167.2/codex-rs/core/src/tools/context.rs#L483
[exit]: https://github.com/openai/codex/blob/rust-v0.167.2/codex-rs/core/src/unified_exec/async_watcher.rs#L155
[cli]: https://github.com/openai/codex/blob/rust-v0.167.2/codex-rs/cli/src/main.rs#L2485
[embedded]: https://github.com/openai/codex/blob/rust-v0.167.2/codex-rs/app-server/src/in_process.rs
[connection]: https://github.com/openai/codex/blob/rust-v0.167.2/codex-rs/tui/src/app_server_connection.rs
[persistence]: https://github.com/openai/codex/blob/rust-v0.167.2/codex-rs/rollout/src/policy.rs#L161
[cells]: https://github.com/openai/codex/blob/rust-v0.167.2/codex-rs/core/src/tools/code_mode/mod.rs
[cell-handler]: https://github.com/openai/codex/blob/rust-v0.167.2/codex-rs/core/src/tools/code_mode/execute_handler.rs
[cell-delegate]: https://github.com/openai/codex/blob/rust-v0.167.2/codex-rs/core/src/tools/code_mode/delegate.rs
