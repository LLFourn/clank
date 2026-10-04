# grok-wait-uses-session-identity-without-a-registration-handshake
# Grok wait uses session identity without a registration handshake

## Problem and outcome

Grok's current Clank integration rewrites terminal commands to inject
`--handoff-key` and `--handoff-generation`, maintains a separate registration
state machine, and requires PostToolUse confirmation before a waiter can deliver
work. This creates extra failure paths, including rejecting an ordinary
`clank wait --repo <this session's repository>` because the command matcher only
recognises bare `clank wait`. The claimed need to prevent completion before
background registration has not been demonstrated with actual Grok behaviour.

Remove Grok's Clank PostToolUse registration handshake and both handoff identifiers
as concepts throughout the codebase. Use the normal monitor subscription with
the existing repository, agent and harness binding; the existing native session
ID is the only permitted Grok-specific identity passed to the monitor. Do not
replace the removed concepts with renamed tokens, attempt IDs, generations,
registration secrets or opaque correlation strings.

Implement the complete plan in one implementation commit, with no internal named
milestones.

## Evidence and exceptions

Removal is the default. An exception requires an observed, reproducible failure
or an explicit guarantee in the actual supported Grok implementation proving
that ordinary background commands cannot safely complete without this handshake.
A hypothetical delayed hook, the current Clank implementation's assumptions, or
a fixture that models those assumptions is not sufficient evidence.

Inspect the supported Grok version's background-task completion and hook dispatch
behaviour. Exercise a command that finishes immediately and a blocking command
that later finishes, and distinguish real provider acceptance from fixture tests.
If evidence establishes a genuine missing guarantee, explain the exact failure,
provider version, reproduction and simplest remedy to the user before retaining
PostToolUse or either identifier. The user has not authorised additional protocol
identifiers even if a provider problem is found. Missing live-provider access is
a validation limitation, not evidence that the handshake is necessary.

## Monitor and wait contracts

- `clank wait` identifies the actual bound session through the existing environment
  and monitor protocol. The monitor derives current roster role and validates
  the repository, agent and session binding. The agent does not declare its role.
- Automatic Grok waits use the same monitor-owned delivery and subscription path
  as ordinary waits, without a second adapter registration exchange. Pending
  work can be delivered immediately without waiting for PostToolUse.
- If exclusive automatic waiting requires additional handling, put that handling
  at monitor connection admission and cleanup using existing session identity and
  live connections. Define the duplicate policy explicitly: reject a concurrent
  automatic waiter with a useful diagnostic rather than silently replacing an
  active one. Preserve legitimate observer and other-provider subscriptions.
- A disconnected or superseded session loses delivery authority. An old connection
  closing cannot clear another live connection's ownership; use the monitor's
  existing connection lifecycle rather than transmitting a new identifier.
- Bare `clank wait` and `clank wait --repo <canonical bound repository>` behave
  identically, including an equivalent repository path spelling. A different
  repository/worktree is refused for this automatic session. No command-text
  matcher or injected flags decide whether a wait is authorised.
- Cancellation, process exit, session rebinding and monitor restart clean up
  through existing connection/session lifecycle. Reconnecting never acknowledges
  work or invents a delivery receipt. Preserve exact-event receipt and reply
  semantics on the established successful output path.

## Grok adapter and removal scope

- Keep the supported native Stop behaviour and background completion mechanism
  needed to wake Grok. Stop may instruct Grok to launch one ordinary background
  `clank wait`; it must not create polling, duplicate task loops or a synthetic
  registration protocol. Use existing session/monitor evidence where needed.
- Remove Clank-owned PostToolUse registrations and handlers whose purpose is the
  handshake, and associated PostToolUseFailure/PermissionDenied handling when it
  exists solely for this machinery. Remove PreToolUse command rewriting and any
  hook maintained solely to reserve or tag a wait. Preserve unrelated user hooks
  and genuinely unrelated lifecycle functionality.
- Delete the hidden flags, parsing, generated commands, key/generation records,
  leases, notification sockets, registration deadlines, setup checks, diagnostics
  and obsolete tests that exist solely for this handshake. Do not keep a dormant
  alternative path or a compatibility layer that perpetuates these concepts.
- Sweep source, CLI help, setup-generated adapters, templates, shipped skills,
  fixtures, integration tests and architecture documentation for these concepts.
  Update historical descriptions as needed to make retirement explicit; do not
  rewrite Git history or alter unrelated PostToolUse implementations.
- Setup must remove the obsolete Clank-owned hook entries from existing profiles,
  retain user-owned entries, and install the simpler integration idempotently.
  Old running waits and persisted adapter files need a clear one-time upgrade
  path; do not strand Grok in an arming loop or retain legacy runtime machinery.

## Resource contract

Reuse the shared repository monitor, existing lifecycle notifications and blocking
subscription. An idle Grok waiter adds no periodic polling, filesystem scanning,
background registration helper or timer-driven repair loop. Keep connection state
proportional to live sessions/connections and remove it on disconnect.

## Validation and completion

Cover immediate pending work, work arriving after idle subscription, duplicate
automatic waits, cancellation followed by replacement, stale session rejection,
same-repository `--repo`, different-worktree refusal, monitor reconnect, and
receipt/reply behaviour. Include setup migration preserving unrelated hooks and
repeat setup producing no additional changes. Use meaningful integration coverage
instead of recreating the removed state machine in fixtures.

Check idle resource behaviour and compatibility of the shared monitor path with
Codex, Claude and OpenCode. Run appropriate focused tests and checks. Document
what was verified on real Grok versus fixtures, and any remaining provider
limitation. After review, reinstall Clank and its updated hooks through normal
setup and verify that installed commands and adapter files no longer contain the
removed handshake, flags or instructions. Provide the user a plain explanation
of the final end-to-end wait flow.

## Implementation

The native adapter now has only SessionStart, Stop and SessionEnd. SessionStart
binds the native session; Stop inspects Grok's running background task snapshot
and requests one ordinary `clank wait` when necessary. Native `stopHookActive`
allows the continuation to end after an immediate completion or failure, without
an arming counter or repair loop. SessionEnd releases only its own binding.
The old adapter module, hidden CLI arguments, command rewriting, confirmation,
registration files, leases and notification sockets are deleted.

Automatic Grok waits enter the shared monitor before selecting pending work.
The existing protocol's author and tool/session binding identify the client;
there is no additional identity field. The monitor rejects concurrent automatic
waits for that bound session and refuses stale bindings. EOF closes the writer
queue; admission removes disconnected connections before accepting replacements.
Cancelling a wait closes its socket immediately and releases its blocked reader.
The common monitor protocol is version 11 to negotiate these admission semantics.

The wait validates its canonical launch repository, author and current binding.
Same-repository `--repo` is accepted; another worktree is refused. Configured
sources remain monitor hosted. Existing PR draft watching starts only after an
admitted idle subscription, uses the established role/block projection, and is
shut down on return. Reconnection uses the existing disconnect/liveness path.
Successful flushed output records exact-event delivery and never acknowledges
work or satisfies a required reply. No new idle polling or helper is introduced.

Setup readiness and installation share the same retirement transformation:
obsolete Clank-owned hooks are removed, user hooks and unrelated fields remain,
and repeating installation changes nothing. Current architecture and upgrade
instructions are in `GROK-INTEGRATION.md`; older finished plans explicitly mark
their adapter descriptions as retired. Installed profiles are updated after
review; old running tasks must be cancelled and sessions restarted if Grok retains
loaded hooks. Legacy inactive files are not read by the new integration.

Parallel testing exposed an existing monitor repair permit remaining locked while
a concurrently forked child inherited its open file description. Explicitly
unlocking the existing permit on drop makes connection repair independent of
that child's exec; this adds no new lease or protocol identity.

## Validation evidence

- CLI library suite: 2,229 passed, three ignored, and one unrelated timer lease
  contention test failed under the parallel suite. That timer test passed on its
  focused rerun. This is not reported as a fully green single suite run.
- Real-socket monitor tests cover duplicate refusal, EOF followed by replacement,
  safe old-connection cleanup, rebinding and stale-session rejection. Hook tests
  cover immediate/failing completion without rearming loops, manual mode and
  safe SessionEnd; setup tests cover retirement and preservation/idempotence.
- Four deterministic subprocess launch fixtures exercise fresh, resumed, forked
  and missing-setup repair paths, ten cycles each. They cover immediate pending
  work, idle delivery, bare and same-repository `--repo`, monitor termination and
  reconnection, source isolation, wake receipts remaining unacknowledged, and
  explicit acknowledgement afterward.
- Formatting and diff whitespace checks pass. The removed adapter/identifier
  names have no remaining implementation or current documentation hits; retired
  hook event names remain only where needed for migration.
- Supported Grok's pinned public terminal implementation returns a background
  task result from the current `block_until_ms: 0` path even for immediately
  finishing commands; its native hooks document task snapshots and continuation
  state. These are upstream source findings, not an authenticated model test.
  The fixtures explicitly model completion notification. Live immediate/delayed
  completion acceptance and post-review installation remain completion checks;
  no observed failure justifies retaining the removed handshake.

## Review correction

Grok's review of `e2fee3db` identified one remaining obsolete paragraph in the
README setup instructions. It now describes SessionStart, Stop and SessionEnd,
the advertised Stop decision gate, and retirement of owned tool hooks. The wake
receipt description no longer calls the ordinary wait registered. A targeted
documentation sweep and diff whitespace check verify this correction; no runtime
behaviour changed and no additional runtime tests were needed.
