# Grok integration

Clank uses Grok's native session ID and the ordinary repository monitor. There
is no tool-call registration handshake. The previous adapter has been retired:
its tool rewriting, hidden correlation flags, registration records, leases,
notification sockets and registration deadline are removed. Old design notes
are historical descriptions, not requirements of the current integration.

## End-to-end flow

1. Clank launches the selected Grok profile in the intended canonical repository
   or linked worktree, with foreign harness identity removed from the environment.
   Existing bounded ACP preparation copies fork history into the child workspace
   before interactive launch. It is not an idle helper or delivery transport.
2. The native SessionStart hook binds Grok's actual `sessionId` to the launched
   `CLANK_AGENT`. A lookup-only session cannot silently claim another label.
3. At genuine main-session turn end, the native Stop hook reads `backgroundTasks`.
   If an ordinary background `clank wait` is running, Stop allows the turn to end.
   Otherwise it requests one terminal tool call with `block_until_ms: 0` and no
   positive timeout. It neither starts a process nor changes the tool arguments.
4. The wait gets Grok's native `GROK_SESSION_ID`, its agent binding and the launch
   repository. Bare `clank wait` and `clank wait --repo <the same repository>` are
   equivalent. An automatic wait cannot change its author or move to another
   repository/worktree. Auto-off, inspection and observer waits keep their own
   existing semantics.
5. Before selecting even immediately pending work, the automatic wait connects
   to the shared monitor using the existing author and tool/session fields. The
   monitor derives current roster role, checks the binding, and refuses a second
   connected automatic waiter for that same session. No additional identity or
   reservation token crosses the protocol.
6. The monitor routes pending work or parks the connection until work arrives.
   Configured event sources are hosted once by the monitor. Automatic Grok waits
   use these sources; invocation-only `--event` sources require manual operation.
   A real disconnect/liveness failure causes normal monitor reattachment, not
   periodic retries or a local watcher that bypasses admission.
7. Immediately before output the wait verifies the session still owns the agent
   and automatic mode remains on. It flushes the selected work, then records
   exact-event first-delivery evidence while the binding still holds. Delivery
   does not acknowledge an event or satisfy a required reply.
8. Grok owns the background task and its completion notification. The agent acts
   on the returned work, acknowledges only after handling it, and starts the next
   ordinary background wait when appropriate. Native SessionEnd releases only
   that session's binding, under the existing binding lock; a delayed end cannot
   release a successor's binding.

## Ownership, cancellation and resource use

The monitor owns connections, not provider task IDs. EOF ends the socket writer
and closes its queue. Admission drops disconnected clients before considering a
replacement. Each connection cleans up its own resources; closing an old socket
cannot release some other socket's ownership. A binding change refuses old
connections, and the wait independently checks the binding before delivery.

Stop's `stopHookActive` flag limits arming to one request within a continuation
turn. An immediately completed wait may already be absent from the task snapshot;
Stop lets that continuation end so Grok can deliver its completion. A failed wait
is visible through its tool result and is not repeatedly relaunched by Stop.
An omitted native task snapshot is a visible adapter error, not proof of idleness.

An idle waiter blocks on the existing monitor socket. It creates no adapter state
file, lease, extra socket, filesystem watcher, registration helper or periodic
poll. The existing shared monitor heartbeat/liveness policy remains unchanged.
Configured producers and timers remain shared monitor resources.

## Setup and upgrade

The native `hooks/clank.json` contains Clank SessionStart, Stop and SessionEnd
entries. Installation atomically removes the previous Clank-owned tool and
failure/cancellation hook entries, preserves unrelated user hooks and fields,
and is idempotent. Validation detects obsolete owned entries so launch repair can
replace a stale profile automatically. Custom `GROK_HOME` and launch profiles use
that profile's actual configuration home, rather than an unrelated default home.

Install the reviewed binary and run normal Clank setup for each affected Grok
profile. Existing provider processes may retain their loaded hook configuration;
end/cancel their old background wait and restart those sessions if the provider
has not reloaded the new hooks. Do not copy an old generated command into the
new CLI. Leftover files from the removed adapter are not read by current Clank
and do not confer ownership; discard those inactive legacy files during the
one-time upgrade after the old tasks have ended, never while a legacy task owns
its lease. No compatibility adapter continues the retired protocol.

## Provider evidence and validation limits

The previously inspected supported baseline is Grok 1.0.46, public source pinned
at `2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8`. ACP initialize advertises native
Stop decision support; neither that advertisement nor a version proves that an
authenticated model consumes background completion notifications. Notifications
must be enabled in the actual Grok session.

The official hook documentation describes in-flight `backgroundTasks` and the
native `stopHookActive` continuation flag. The terminal-tool source's current
contract maps `block_until_ms: 0` directly to background execution and returns
`BackgroundTaskStarted` from that path, independently of whether the command has
already completed. This is not an inline foreground-return path that Clank must
hold open. Older pinned two-knob tool contracts explicitly ignore `block_until_ms`;
those configurations need their actual background parameter instead and are not
proved compatible by a CLI version alone. No observed provider failure has
justified retaining a Clank-specific registration acknowledgement.

Relevant upstream sources:

- [Native hooks and Stop task snapshots](https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-pager/docs/user-guide/10-hooks.md)
- [Native tool execution and result handling](https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-shell/src/session/acp_session_impl/tool_calls.rs)
- [Current terminal tool background execution](https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-tools/src/implementations/grok_build/bash/mod.rs)
- [Background tasks](https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-pager/docs/user-guide/20-background-tasks.md)

Deterministic subprocess acceptance exercises the installed hooks and binary in
fresh, resumed and forked launch environments, pending work, same-repository
`--repo`, repeated idle deliveries, acknowledgement and source-binding isolation.
It models Grok's background-completion-to-turn transport explicitly and must not
be presented as authenticated model acceptance. Socket tests cover duplicate
admission, EOF/replacement and stale sessions. Hook tests cover immediate/failing
completion without an arming loop, auto-off and safe SessionEnd release. A live
provider check of immediate and delayed terminal completion remains separate
from these fixture guarantees.
