# Grok launch and wake integration

Investigation date: 2026-10-03. This document records the evidence and the
implementation contract for `grok-wakes-through-a-supported-lifecycle-integration`.
Bounded ACP fork preparation is implemented and tested, including an offline
copy through installed Grok 1.0.46 using the production preparation owner.
The native lifecycle handoff core is implemented and covered by deterministic
fixtures, including delivery through the real Clank wait/inbox path. Native
setup, profile/home selection, bounded capability probes, launch gates, doctor
snapshots, manual-wait migration and guarded skill/resume instructions are now
implemented. These changes have not been installed into the user's provider
configuration or Safepay. Fresh/fork/resume launch acceptance now runs through
a deterministic provider executing the actual installed commands, hooks and
waiters for ten completion cycles each. Authenticated native payloads, enabled
completion notifications and model wake/reaction remain unverified.

## What happened in Safepay

The recorded failure was an ownership change during a fork, rather than evidence
that the monitor failed to deliver an eligible review.

- The original Grok session was `01a0fc72-8c9b-7893-848b-90154b0b7276`.
- A forked child, `01a0fc98-68b2-7f02-ab49-5afdafdec8ac`, was created at
  `2026-10-02T12:30:41.853786Z`. Its summary names the original session as
  parent, but records `/Users/llfourn/src/safepay` as its working directory.
  It should have belonged to the `changeless-hub-research` worktree.
- The original repository's Grok binding changed to the child at
  `2026-10-02T12:30:41.980635Z`.
- The original background wait ended at about `12:30:42Z` with:

  ```text
  wait for grok cannot continue: this grok session
  01a0fc72-8c9b-7893-848b-90154b0b7276 does not own the label
  (grok session 01a0fc98-68b2-7f02-ab49-5afdafdec8ac);
  run `clank as grok` inside the intended session
  ```

- Grok received that completion and explicitly declined to take back the label.
  The child subsequently used an explicit `cd` to bind inside the worktree.
  Later original-session records show successful background-wait completions
  and re-arming, after the binding was restored.

The recorded fork invocation used `--resume <parent> --fork-session --trust`
and an orientation prompt. It supplied no `--cwd`. A captured original-session
environment contains its explicit `GROK_SESSION_ID`, so newest-session discovery
is not the demonstrated cause of this wait's rejection.

This reconstructs the binding collision and resulting stopped waiter. It does
not identify the exact hook/process that wrote the first child binding, prove
which historical Grok binary ran, or establish whether every missed-wake report
has this cause. Session history, binding output and terminal output were examined
read-only. No Safepay work was acknowledged, no agent was restarted, and no
project or Grok configuration was changed.

## Verified upstream support

The installed executable reports `grok 1.0.46 (2765805b9442) [stable]`.
Public source was inspected at commit
`2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8`. Installed behavior and that source
revision are separate evidence; the GitHub releases page has no published
releases from which to infer a newest installed release.

1. Grok now has an active Stop gate. Native payloads use `sessionId`,
   `stopHookActive` and `backgroundTasks`; imported/SDK hooks can have different
   compatibility envelopes. A block reason can continue the model in the same
   turn. This contradicts Clank's old passive-only description.
2. The gate permits eight continuations per turn, then ends the turn without
   consulting hooks. Timeout, crash and malformed output fail open. Native
   SessionStart is a notification; its stdout cannot supply a wake prompt.
   An indefinite Clank work loop cannot rely on repeated Stop blocks alone.
3. Background-command completion can start another model turn. Safepay's
   records demonstrate that channel working. The native Stop payload describes
   outstanding background tasks, including shell commands. This is the useful
   boundary for enforcing arming without parking the Stop hook indefinitely.
4. Interactive `--fork-session` restores the parent's directory when the source
   session is found outside the requested directory. Upstream
   `effective_fork_new_cwd` explicitly prefers the recovered parent directory.
   Merely adding `--cwd <worktree>` is therefore not a sufficient fork fix.
5. Grok's supported ACP stdio endpoint exposes `_x.ai/session/fork`, with
   independent `sourceCwd` and `newCwd`. The leading underscore is required;
   `x.ai/session/fork` returned method-not-found in the installed binary.

Credential-free probes ran `grok agent --no-leader stdio` in a temporary home,
with the auto-updater disabled, no inherited agent identity, bounded reads and
deadlines, and process-group cleanup. Initialize advertised blocking Stop
support. An empty fabricated session was successfully copied through the fork
method to a different directory with a preselected child UUID. The response and
persisted child summary agreed on ID, target cwd and parent; the source summary
was unchanged. The successful request completed in about 0.43 seconds.

The fixture contained only `summary.json`, an empty `chat_history.jsonl`, and an
empty `updates.jsonl`. Summary fields were `info: {id, cwd}`, timestamps,
`session_summary`, zero message counts, `current_model_id`, and
`chat_format_version: 1`. After ACP initialize with protocol version 1, the
request was:

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "method": "_x.ai/session/fork",
  "params": {
    "sourceSessionId": "<source UUID>",
    "sourceCwd": "<temporary source directory>",
    "newCwd": "<temporary child directory>",
    "newSessionId": "<distinct child UUID>",
    "sessionKind": "fork"
  }
}
```

This proves installed cross-directory copy support, not an authenticated model
continuation or copying every form of real history. `session/new` required
authentication, so it was not used. No credentials or live history were copied.

Primary references, pinned where implementation behavior matters:

- [Native hooks and limits](https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-pager/docs/user-guide/10-hooks.md)
- [ACP agent mode](https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-pager/docs/user-guide/15-agent-mode.md)
- [Background tasks](https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-pager/docs/user-guide/20-background-tasks.md)
- [Initialize capability advertisement](https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-shell/src/agent/mvp_agent/acp_agent.rs)
- [Interactive startup directory selection](https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-pager/src/app/session_startup.rs)
- [Fork request, response and copy implementation](https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-shell/src/session/fork.rs)
- [Stop gate implementation](https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-shell/src/session/acp_session_impl/stop_gate.rs)
- [Typed background terminal result](https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-tools/src/implementations/grok_build/bash/mod.rs)
- [Native tool hook envelopes and result delivery](https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-shell/src/session/acp_session_impl/hook_dispatch.rs)
- [Preparation preserves rewritten arguments for execution](https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-shell/src/session/acp_session_impl/tool_calls.rs)

## Design decision

There are two boundaries to fix: creation of the correctly located session, and
ownership of the wait at the end of a turn. Treat both as contracts shared by
fresh launch, fork launch and resume. Adding another orientation sentence or
another retry at each launch site would leave those boundaries unenforced.

Use ACP only for bounded fork preparation. Keep Grok's supported background
completion as the continuation channel. Native PreToolUse reserves an owned
handoff; PostToolUse confirms background registration before delivery can run;
a fast Stop guard checks or requests bounded repair. The guard does not itself
long-poll, create a detached waiter, or repeatedly inject work.
Do not adopt the private leader socket framing as a continuation API, keep an
extra ACP process alive while idle, or type work blindly into terminal panes.

The adapter remains to be validated against installed native model payloads. Its
bounded repair still involves a model tool call; it cannot guarantee a wake
after Grok overrides the gate or crashes. Such failure must be visible and
diagnosable, with pending work intact. Do not claim this is an infallible idle
subscription. If the provider cannot reliably report/maintain the background
handoff, stop at that failed contract and seek supported continuation support;
do not accumulate timers, restarts or prompt patches to conceal it.

## Launch, identity and preparation contract

1. Resolve repository, label, provider profile and effective auto-mode once for
   the launch intent. Roles belong to the current roster, never to launch argv
   or a captured hook result. The launch environment removes inherited session
   identity and carries only the intended label and launch context.
2. A fresh Grok session starts explicitly in the resolved repository. A resumed
   session must already belong there. A fork must be prepared as a new session
   in the child repository **before** any interactive binding can occur. A
   prompt telling the model to `cd` later cannot satisfy this contract.
3. Persist a fork intent with source ID/cwd, target cwd and a stable, distinct
   child ID. Keep the source binding unchanged. Preparation reads that intent
   without consuming it and invokes the configured Grok binary through its
   supported ACP endpoint. No terminal, model request or Clank binding runs in
   the helper. Strip Clank label/session context from its environment.
4. Initialize ACP, verify protocol/required behavior, send the explicit fork
   request, and validate the response identity, target cwd and parent. Stop and
   reap the helper before the interactive launch. Use bounded time, bounded
   stdout/stderr, bounded JSON frames and process-group cleanup, including on
   cancellation. Do not assume a successful initialize advertises every private
   extension: method support must be demonstrated or reported unsupported.
5. Retry an interrupted intent by inspecting the **exact** intended child. A
   matching persisted child can be reused. A mismatch, partial copy or existing
   unrelated ID is an error; never overwrite it or silently generate another
   child. If necessary, persist an explicit prepared state after successful
   validation. Grok's fork-copy method itself is not assumed idempotent.
6. Launch `grok --resume <prepared child> --cwd <child repo>` through the normal
   interactive path. Bind only its actual session ID and actual cwd. Native
   SessionStart parsing must handle the native envelope and must not bind an
   unrelated/subagent/preparation session by guessing from newest mtime.
7. Consume the launch intent only at a defined successful binding boundary;
   `--print` must not copy a session or consume an intent. Printing may describe
   preparation plus launch, but must not imply a bare interactive fork command
   implements the operation. Auto-open and explicit open use this same path.
8. Unsupported versions, malformed responses, deadlines and copy errors leave
   the worktree and intent available for retry. Report which preparation failed
   and that the worktree exists. Do not fall back to same-directory interactive
   fork or discard history by silently starting a fresh session.

Missing-parent behavior must be explicit and consistent with existing fresh
bootstrap policy. A source proven absent before preparation may use that policy
with a clear history-unavailable diagnostic; a source disappearing during copy
is a preparation error, not permission to silently switch behavior.

## Wait and continuation contract

The ownership tuple is `(canonical repository, label, Grok, session ID)`.
The model requests a native background terminal tool; Grok owns its process and
completion-to-turn delivery; Clank's wait owns the monitor subscription; the
monitor resolves work and the current roster role. A unique generation and
provider tool-call ID identify one handoff within that tuple. No role is stored
in the handoff. Native SessionStart binds the actual Grok session and cancels
the previous generation; its passive stdout is never treated as a wake.

The automatic command is `clank grok-wait`, with `block_until_ms: 0` and no
positive timeout. The supported legacy background flags are accepted too.
PreToolUse recognizes the native terminal tool and this exact simple command,
reserves the generation, and rewrites only its `command` field to:

```text
exec <this Clank executable> grok-wait <repository/label key> <generation>
```

The actual executable path is shell-quoted. The command must fit Grok's 1000-byte
task-command bound. Compound shell commands, unrelated tasks and ordinary
manual `clank wait` calls are not promoted to automatic ownership. Unknown
terminal tool names/background parameter layouts are unsupported, not guessed.
A tagged command cannot be replayed under a different provider tool-call ID.
The native cwd must also match the resolved repository: an argv override cannot
relocate a live session or authorize binding in another worktree.

The worker takes a nonblocking lifetime lease and binds a local Unix datagram
socket. It validates its actual session, current binding, roster membership and
effective auto-mode. It does **not poll the work future** until PostToolUse has
confirmed all of these fields from the native result:

```text
type = BackgroundTaskStarted
task_type = bash
status = running
task_id = nonempty
command = the exact rewritten command
promptId, toolUseId, toolName = the reserving call
```

Input/result truncation invalidates confirmation. A foreground result, dispatch
failure or permission denial marks the attempt failed and wakes its worker to
exit. Background registration has one 30-second startup deadline; there is no
registration polling and no additional idle timer after it succeeds.

Confirmation is persisted before sending a nonblocking IPC wake. The worker
reads after binding and before receiving, so confirmation before startup is a
latch and confirmation afterward is a wake. A missing/full socket does not make
a hook or config write block. Once confirmed, the **same process** runs Clank's
existing wait; there is no nested wait child or permanent ACP helper. Eligible
work ends that process, and Grok's background completion supplies the next turn.
Binding, effective auto-mode and generation are checked again before writing
work to stdout. The completed-work record is evidence that Clank selected work
for this task, not proof that Grok has received or acted on it.

| State or trigger | Required behavior |
| --- | --- |
| First launch/fork/resume, auto on | Bind in the correct repo, handle immediately pending work, then reach the same Stop guard. No resume-only arming policy. |
| Stop, correctly owned handoff running or starting | Allow after typed PostToolUse confirmation plus the live worker lease, or an exact matching running shell task in the Stop snapshot during process startup. Keep provider registration distinct from monitor subscription. |
| Stop, auto on, no handoff | Cancel an incomplete attempt and request the canonical background tool. At most two repair blocks per `promptId`; then allow with a visible diagnostic. An instruction is never proof of arming. |
| Background completion with work | Grok starts a new turn containing the wait output. React using the current role, acknowledge/write feedback through the usual commands, then guard the next idle transition. |
| Work arrives while no wait is armed | Durable work remains pending. The next wait derives it immediately. Never synthesize acknowledgement to suppress repeated delivery. |
| Session binding replaced | The config mutation cancels/notifies the old automatic slot; generation/session checks fence it before delivery. Its completion cannot restore the old binding. |
| Role changes during wait | Re-derive role at delivery, using the existing monitor contract. No hook-carried role or second role-specific event copy. |
| Auto off | Cancel only the automatic handoff; allow Stop without repair. Manual waits remain a separate explicit action. |
| User interrupts, provider cancels/fails, pane dies | SessionEnd, StopCancelled and StopFailure cancel/notify the slot. Provider-owned process teardown releases the lease/subscription. Work remains durable. Resume restores the same guarded lifecycle. No automatic restart. |
| Guard missing, incompatible payload, repeated repair failure | Report unsupported/unarmed status and a concrete manual recovery command. Do not claim healthy auto mode or retry forever. |

Identify the handoff precisely. Unrelated shell tasks, monitors, subagents and
scheduled tasks do not satisfy the guard. Use a short canonical Clank command
with explicit repo/label/session context or a typed handoff marker that can be
validated; do not match an arbitrary substring containing `clank wait`.
Account for Grok's capped command metadata and reject truncated/ambiguous proof.
Existing manual waits must not be relabeled as automatic solely by their label.

The production seam covers task-start and task-completion races. Without the
PostToolUse handshake an immediately pending wait could finish before Grok
registers the process as background, return inline, and leave no later wake.
Holding delivery until the typed result removes that race. A delivered record
correlated with the same Stop `promptId` means its completion is already owed
through the background channel; that Stop may allow. The next turn requires
its own handoff. A waiter does not wait for Stop permission: Grok skips Stop
hooks at its eight-continuation cap, which would strand a Stop-gated worker.

The runtime namespace has one fixed record, metadata lock, lifetime lease and
socket path per repository/label. Records contain the full session/generation;
the path does not grow per turn or historical session. A replaced worker must
release its lease before another can own the slot. All transitions are atomic
under nonblocking metadata locks. Busy/corrupt state is diagnosed, never treated
as successful registration. File reads are bounded, do not follow symlinks and
require regular files in the vetted uid runtime directory outside the repo.

Auto-off and binding replacement notify the slot at the existing config write
boundary. Native cancellation and resume use the same control channel. Config
edits outside those APIs, including the global default, are also noticed on the
existing wait's monitor notes/local beats, without a new watcher or timer.
Auto policy shares the binding check's config read rather than adding another
agent-config read to those idle beats.
Only automatic waits carry this policy. Cancelling a local wait future aborts
its source tasks, so command producers are not detached and left running.

Normal operation does not spend Stop continuations: the task is armed, so Stop
allows immediately. Repairs consume the provider's finite allowance. Never
block simply because `stopHookActive` is true or false, and never use exit 2 as
an unbounded idle mechanism. Distinguish allowed completion, cancelled turn and
failed turn; session-end notifications cannot serve as continuation gates.

## Setup, diagnostics and compatibility

Install one native Grok lifecycle adapter with explicit tool identity. Parse
camelCase fields at that boundary and normalize them into Clank types. Do not
route it through a Claude-only parser and infer the provider. Avoid installing
a second active Clank adapter via Grok's imported Claude settings; preserve
unrelated user hooks. Setup is idempotent and must explain which adapter owns
binding and Stop enforcement.
Tool-event entries should match only verified native terminal tool names, so
ordinary file/read/edit calls do not spawn Clank hooks. Resolve the configured
`GROK_HOME`/profile hook location explicitly; installing a default-home file does
not prove a custom-home provider loaded it. Preserve other user hooks.
During migration, identify an existing ordinary/manual background wait and
require an explicit choice to stop it or keep manual operation; do not quietly
start a second automatic waiter or relabel/cancel a manual task.

Probe capabilities only during explicit setup/launch/doctor, using the configured
binary/profile and bounded calls. Report installed version, adapter presence and
verified features separately. The registration proof is now native PostToolUse,
not merely a Stop snapshot: a missing snapshot alone does not imply no tasks or
unsupported enforcement when typed confirmation and the live lease exist.
Missing native identity/turn/call fields or the typed background result is
unsupported. Also verify that the configured provider actually enables
completion notifications; a running task alone cannot establish that setting.
Preserve custom initial
prompts and explicit auto-off; an empty custom prompt is not a substitute for
disabling automatic lifecycle enforcement.

The installer publishes all nine native events in one atomic `hooks/clank.json`
replacement: SessionStart, Stop, SessionEnd, StopCancelled, StopFailure,
PreToolUse, PostToolUse, PostToolUseFailure and PermissionDenied. Each command is
`clank stop-hook --tool grok` with a five-second timeout. Tool events match only
`^(run_terminal_command|bash|Bash)$`. The shared merger removes owned entries
while preserving unrelated hooks, wrapper metadata and top-level settings.
Malformed JSON/event arrays fail before publication. Dry run skips the Grok
probe (initialize can create provider cache directories) and writes nothing;
idempotent native installation does not rewrite an unchanged file.

Default setup honors ambient GROK_HOME/HOME; `--grok-agent <label> --repo <repo>`
uses the registered launch executable, args and environment, resolving the actual
bound/fork tool ahead of a stale roster tool. All Grok skills follow that same
home. The default-home manifest keeps its existing ownership provenance; custom
homes carry the projected skill inventory and their own setup manifest. A real
automatic Grok launch checks this exact home's adapter and capabilities before
exec (and before fork copying). Previews do not probe/install/copy. Auto-off
bypasses the automatic gate, retaining manual operation and custom prompts.

Capability probing reuses the bounded ACP owner: only initialize, a ten-second
budget, one MiB output bound, four KiB stderr tail, updater disabled, identity
scrubbed, process-group cancellation and bounded reaping. Fork preparation keeps
its separate thirty-second budget. No provider session or model is started by
this handshake; Grok may initialize empty session-cache directories. The response's `_meta.agentVersion` and
`agentCapabilities._meta["x.ai/hooks"].blockingEvents` must advertise `pre_tool_use`
and `stop` on Grok 1.x >=1.0.46, the inspected/tested baseline. Older/unknown/future
major versions are unsupported candidates rather than optimistic version guesses.
The default missing executable does not prevent installing other tools; setup
reports the unavailable capability and automatic Grok launch rejects it.

Initialize does **not** advertise whether background completions invoke the
model. Doctor separates complete adapter configuration, advertised gates, a
read-only phase/lease snapshot and unverified continuation evidence. A native
Registered record proves provider task registration; its waiter lease does not
prove the work future has subscribed to the shared monitor. The existing wait
now emits one debug event, `monitor subscription registered`, only after the
monitor accepts that subscription. Enable `RUST_LOG=clank::cli::wait=debug` for
that evidence; no logger call or diagnostic cadence is added while idle. Identical
configured providers share one explicit doctor probe; no probe is added to an
idle hook. Only version tokens and boolean gates leave the capability parser;
account/model metadata and raw provider errors do not become diagnostic output.
A registered task and held waiter lease are stronger than an offered command,
but still cannot prove Grok receipt or model reaction. A Delivered record means
selection for stdout, not that stdout reached Grok. Runtime diagnosis creates no
files, acquires no persistent ownership, sends no control messages and scans no
processes; it uses the monitor's existing read-only namespace trust policy.
Control sockets use the monitor's checked owner-only permission boundary even
when an existing runtime directory is readable by others.

On first resume the default prompt reaches Stop before arming; role skills teach
that same guarded command. A Stop snapshot reporting a running native shell task
whose parsed executable/subcommand is ordinary `clank wait` records positive
manual-task evidence and requests an explicit choice: terminate that specific
provider task and reach Stop again, or choose auto-off and retain manual
operation. PreToolUse refuses an automatic offer while that evidence remains.
Only a later native task snapshot with no matching manual wait clears it;
an absent snapshot does not. Clank never relabels or cancels a manual wait and
still bounds repairs to two per prompt. This is native snapshot evidence, not a
process scan or proof that an arbitrary custom/compound wrapper is a manual wait.

Expose distinct diagnoses: unbound, bound but no handoff, provider task starting,
owned native handoff registered, completion selected, replacement fenced, unsupported
adapter, and failed preparation. An existing tab, PID, successful callback or
launch prompt proves none of the later stages. If a diagnostic needs monitor
state, request it once over existing IPC; do not scrape process lists or poll
session directories. Recovery for a stopped, bound session is an explicit
background wait or reopening through the corrected launch path, never automatic
rebinding over another live session.

## Implementation milestones and exit criteria

1. **Evidence and contract (this milestone).** Review the directory collision,
   active hook limits, successful offline ACP fork, and the two ownership
   boundaries. No runtime repair is claimed.
2. **Correct session creation.** Introduce one typed, bounded Grok preparation
   owner; durable intent/retry semantics; correct cwd and binding across fresh,
   fork, auto-open, explicit open, print and resume paths. Test with a fake
   executable through production launch/preparation seams, including interrupted
   preparation and unsupported methods. Assert source binding is never written.
   Implemented: stable intent, explicit cross-directory ACP copy, child resume,
   binding-time consumption, read-only preview, fresh cwd and resume validation.
   A copied child is reused only after a recorded successful response; a crash
   between copying and recording leaves an unconfirmed child and fails closed.
   Explicitly creating a new fork with `clank fork create --fresh <new-name>`
   is available when the operator chooses to start without inherited history.
3. **Guard the idle handoff.** Validate installed native payload and task-start/
   completion semantics, then implement normalization, exact handoff identity,
   duplicate prevention, cancellation and native setup. Remove model-only
   prompt/skill assumptions once their replacement is exercised. If validation
   fails, revise this decision explicitly before building a workaround.
   Implemented core: native envelopes, Pre/Post registration handshake, owned
   `grok-wait`, finite Stop repair, actual-provider binding and cancellation.
   Fixtures exercise the production state/IPC/wait seams. Native hook setup,
   capability/doctor surfaces and new skill/launch instructions are implemented.
   Installed authenticated native payload validation remains required before
   claiming live wake reliability.
4. **Verify end to end.** Production-seam fixtures must cover first pending work,
   idle then work, more than eight successive work turns, first fork turn,
   replacement, role change, auto off, cancellation, malformed/truncated
   metadata, missing adapter, timeouts, and work surviving interruption. Assert
   one continuation channel and no acknowledgement during delivery. Run only
   relevant agent/fork/hook/setup/monitor and ownership/cost checks.

The launch acceptance fixture lives in
`crates/cli/tests/it/grok_launch_acceptance.rs`, with its deterministic transport
in `fixtures/grok_provider.py`. It uses the existing compiled Clank binary and
integration harness; a new test executable, terminal tab or model is unnecessary.
Empty HOME plus a custom GROK_HOME containing spaces exercise real setup and
shell quoting. Host and profile foreign identity variables are deliberately
supplied; actual exec and ACP identities must be scrubbed/pinned correctly.

All three entry paths execute SessionStart binding, an initial missing-handoff
Stop repair, PreToolUse rewrite, duplicate-offer rejection, a real waiter process,
the typed PostToolUse registration and a genuine CLI event acknowledgement only
after receipt. The first event is already pending before launch; a bounded pipe
sample before registration proves it cannot complete inline. Nine later events
arrive only after the accepted-monitor-subscription debug event, exercising idle
notification delivery. The fixture explicitly transports each background
completion into a new fake provider turn, with ten turns per launch (thirty
cycles total), then closes through SessionEnd. This tests the completion-to-turn
wiring under the declared transport contract, **not a real Grok model turn**.

Assertions cover correct cwd/session and native binding, read-only previews,
consumption of only the intended fork intent, unchanged source binding/summary,
pending events surviving until explicit reaction, fresh generation per turn,
one fixed runtime slot and one shared monitor reused across idle turns. ACP
initializations occur only during setup/launch (two per direct/resume, three
for a history fork), and each helper must have exited. Callback/stdout/channel
failures and malformed metadata remain covered by the native core fixtures.
Test cleanup addresses only each fixture's own process group, monitor and
runtime paths; it never lists or signals the user's sessions.

An authenticated smoke test, when available, must show a real Grok turn acting
on delivered work in an isolated test repo, then another idle-to-work cycle.
Offline fork success and historical background completion are not substitutes
for that test. Do not mutate Safepay or change the user's installation to obtain
it. If authentication/access prevents it, record the exact unverified boundary
and avoid advertising tested live wake reliability.

Idle cost remains one existing monitor plus one blocking provider-owned wait
for each automatic Grok session. No new periodic timer, process scan, session
discovery loop, detached reader, terminal injector or permanent ACP helper is
introduced. Registration has one bounded startup deadline, not an idle cadence.
Native tool hooks run only on actual tool events; a Stop check runs once per
actual Stop fire. Fork preparation runs
once per explicit launch intent. This is the resource budget as well as the
ownership boundary reviewers should enforce.
