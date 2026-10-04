# grok-wakes-through-a-supported-lifecycle-integration

> Historical implementation notes: the Grok registration adapter described below
> is retired by `grok-wait-uses-session-identity-without-a-registration-handshake`.
> Current Grok waits subscribe directly to the monitor with native session identity;
> there is no tool rewriting or PostToolUse registration requirement. See
> `GROK-INTEGRATION.md` for the current contract.

# Investigate missed Grok wakes, including fork launches, against current upstream support

## Report and evidence boundary

The user reports that Grok was not woken in `~/src/safepay`, and suspects
`clank fork` may not be setting Grok up correctly. The observed Safepay roster
has Codex as master and Grok as a commit reviewer. Do not assume this establishes
why a particular wake was missed.

Clank currently treats Grok hooks as passive, has no Grok stop-hook adapter,
and supplies an initial prompt instructing Grok to run `clank wait` as a
background terminal command. That is an implementation assumption to verify
against the installed and current upstream Grok CLI, not a timeless limitation.
Downloaded executable filenames are not proof of the running version.

## Investigation

1. Reconstruct the most recent missed Safepay wake using narrowly scoped,
   read-only evidence. Identify the relevant commit or event, roster role,
   bound session, auto-mode, launch/resume path, agent state, outstanding work,
   and whether a wait was armed. Distinguish work becoming eligible, the monitor
   delivering it, a waiter returning, Grok continuing a turn, and Grok acting on
   the work. Do not acknowledge or discard pending work to make the symptom
   disappear. Use Clank's supported feedback/event interfaces rather than
   directly reading feedback files. Avoid credentials and unrelated logs.
2. Identify the actual executable, supplier, running and installed versions,
   and supported invocation modes. Consult current primary upstream docs,
   source and release notes for hooks, plugins, SDK/session APIs, background
   command completion, continuation injection, and resume behavior. Record
   source links, investigation date and versions. Separate a passive
   notification from a mechanism that actually resumes a stopped agent.
3. Trace normal launch and `clank fork` end to end: fork repository creation,
   roster/config inheritance, agent selection, launch command and environment,
   initial prompt, binding, auto-mode and wait registration. Compare direct
   launch, automatic fork opening, explicit fork opening, existing-session
   resume and first-turn completion. Determine whether a fork loses the Grok
   prompt, changes identity/cwd, inherits a stale parent binding, or arms no
   waiter. Include the Zellij launch boundary if evidence points there, without
   assuming an existing tab proves a correctly initialized Grok session.
4. Reproduce the demonstrated failure with deterministic fixtures. Record
   whether the exact Safepay case was reconstructed or only a failure contract
   was reproduced. If its context is unavailable, keep that uncertainty explicit.

## End-to-end contracts and design decision

Before implementation, document one supported lifecycle path and its contracts:

- Identity: Clank resolves the repository, agent label, current roster role and
  session binding. A fork/session restart must not retain another repository's
  binding. Role changes are resolved when work is delivered.
- Ownership: specify exactly who arms the wait after a turn, who owns it while
  idle, and who cancels it on resume, replacement, shutdown or auto-mode off.
  Prevent duplicate concurrent waits and duplicate continuation injection.
- Delivery: specify how eligible work wakes the waiter and how its completion
  reaches Grok as actionable input. Delivery must not depend on the model
  remembering to run a tool after every turn if a supported lifecycle adapter
  can enforce this. Preserve unhandled work across interruptions; delivery
  alone does not acknowledge an event or write review feedback.
- Continuation: define first launch, post-turn idle, immediate pending work,
  session resume/clear, role change, disconnected/replaced sessions and fork
  startup. State how each path reaches the same adapter without recursive
  wake loops or hidden retries.
- Capability and failure: distinguish unsupported Grok versions, a missing
  adapter, an unbound session, no armed wait, failed delivery and pending work
  with no reachable agent. Setup/diagnostics must describe what is actually
  supported and give a concrete recovery action rather than claiming a healthy
  integration which cannot wake the agent.
- Compatibility: verify actual upstream capabilities before choosing native
  hooks, a plugin, a session API or the current background-tool mechanism.
  Prefer a supported event-driven adapter. If a fallback remains necessary,
  document its limits, supported versions and bounded failure behavior.

Select the smallest coherent change supported by this evidence. If a fork
initialization bug explains the failure, fix that path and verify normal Grok
startup; do not invent a new orchestrator unnecessarily. If the current
integration is fundamentally unreliable, replace the lifecycle ownership
boundary rather than adding prompts or retries symptom by symptom.

## Implementation and acceptance

- Implement the selected adapter or launch fix in the existing tool ownership
  boundaries, with setup/launch/resume behavior and version/capability checks
  consistent with the documented contract. Update stale passive-hook comments,
  first-turn prompts and user-facing setup guidance where evidence warrants.
- Exercise the production launch/adapter seams using a fake Grok executable or
  lifecycle API: first pending wake, idle then wake, repeated turns, fork
  creation/open, resume, session replacement, role changes, auto-mode off,
  unsupported version, adapter failure and work surviving interruption.
  Assert that work reaches the correct bound agent, only one wait/continuation
  owns a turn, and failures never silently clear work.
- Cover direct launch and fork launch with the same lifecycle assertions, so
  passing a direct-launch test cannot hide a broken fork bootstrap.
- Run relevant Grok/agent/fork/hook/monitor tests and ownership/cost checks.
  Document exact upstream versions tested. A live smoke test, if available,
  must show an actual Grok continuation responding to delivered work, not just
  a terminal tab existing or a callback exiting successfully.
- Keep Safepay inspection read-only while investigating; do not change its
  roster, acknowledge its work, restart its agents, or modify the user's Grok
  installation merely to gather evidence. Clearly separate Clank code/setup
  changes from any later live integration rollout.

## Resource and scope constraints

No periodic polling, process scans, repeated filesystem/session discovery,
network heartbeat, or extra idle worker just to hand work to Grok. Reuse Clank's
monitor and its blocking/event-driven wait. Any version or diagnostic probe is
bounded and runs during explicit setup/launch/diagnosis, not continuously.
No blind terminal typing, synthetic acknowledgements, automatic restarts, or
unrelated provider redesign. This plan investigates and fixes Grok wake
reliability; it is not permission to mutate the Safepay project or its live work.

## Promotion assessment

Ready against the current code as an investigation followed by the smallest
evidence-backed fix. The preceding Zellij lookup plan is finalized and there
is no other active plan. There are concrete launch seams to investigate:

- In `agent::start`, the bound-session resume path resolves effective auto-mode
  and the tool-specific initial prompt. The unbound-session paths instead use
  `compose_bootstrap_launch` or `compose_fork_launch`. They construct bind and
  orientation prompts separately; test whether those paths reach the same Grok
  wait-arming lifecycle rather than relying on resume-only prompt tests.
- `fork::run_fork_pinned` preserves explicit source auto-mode settings while
  dropping the old binding and writing a one-shot fork specification. Inspect
  launch and fresh-session binding together; copying auto-mode alone does not
  prove a waiter was armed or a completion can continue Grok.
- `setup::WORK_LOOP_GROK` assigns re-arming to the agent every turn, while
  `stop_hook` classifies Grok as undrivable/passive. These are the current
  ownership contracts to verify against current primary upstream evidence.

These findings establish plausible failure boundaries, not the Safepay root
cause or present upstream capabilities. Keep the first milestone focused on
that evidence and the end-to-end lifecycle decision. A supported native adapter
is conditional on verified capabilities; if the cause is a fork/bootstrap bug,
prefer its targeted correction. Lack of access to the historical session or a
live smoke test must be recorded and does not justify fabricated proof, idle
polling, or mutating Safepay's pending work.

## Milestone 1: reconstructed failure and implementation contract

See [GROK-INTEGRATION.md](../../GROK-INTEGRATION.md) for the complete evidence,
primary source links, supported lifecycle decision, end-to-end contracts and
implementation milestones. This milestone changes documentation, not runtime
behavior. The remaining launch and adapter work is explicitly required.

The recorded Safepay fork created its Grok child in the source repository and
the source label became bound to that child. The original wait completed with
an ownership rejection, which Grok received and acted on by declining to take
the label back. The exact first binding writer and historical executable version
remain unproved; the evidence does not justify calling this a monitor delivery
failure. The source's explicit session environment rules out newest-mtime
identity discovery as the demonstrated cause of that wait rejection.

Installed Grok 1.0.46 has active Stop support, but eight same-turn continuations
and fail-open hooks prevent an indefinite Stop-only work loop. Source pinned at
`2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8` also shows interactive fork restoring
the parent cwd even when a different process cwd is requested. A `--cwd`-only
patch is insufficient. An isolated, credential-free ACP fork probe successfully
copied fabricated empty history to the explicit child cwd, preserved the source,
and validated the persisted child identity/parent. No model was invoked.

Implement bounded ACP fork preparation before binding, followed by normal
interactive child resume. Separately validate and implement a fast native Stop
guard for Grok-owned background handoffs; use background completion for new
turns, not repeated Stop blocks as the ongoing wake mechanism. Gate the task
identity and start/completion race contract before claiming native enforcement.
No extra idle process or polling is permitted. README's passive-only claim is
corrected to distinguish current provider capabilities from Clank's still
model-armed integration.

## Milestone 2: prepare the correctly located child before binding

Implemented a launch-only Grok preparation owner in `cli::grok_launch`.
New Grok fork intents reserve a distinct UUID and record the source cwd; legacy
intents upgrade through one bounded exact-ID lookup, never newest-mtime.
Grok source existence is deferred to this owner rather than allowing the generic
default-home probe to discard history kept in a configured `GROK_HOME`.

Real fork starts initialize ACP and call `_x.ai/session/fork` with explicit
source/target cwd and child ID. Both the response and saved child summary must
agree on identity, cwd and parent. The helper has a 30-second deadline, a 1 MiB
total stdout bound, a 4 KiB stderr tail, process-group cancellation and bounded
reaping; it installs no reader task or idle worker. Its label/session environment
is scrubbed even when the launch profile supplied an override. The preparation
lease uses the existing uid-owned runtime namespace outside the repository.

A successful child is resumed by its own ID and target cwd. Fresh Grok forks
also have a pinned ID/cwd, and ordinary fresh starts now explicitly set their
cwd. Bound resumes validate that the session is already stored in this repo;
`--cwd` cannot relocate foreign history. Profile args cannot override these
managed session/cwd flags. Grok fork previews print the preparation-owning Clank
command without copying history or consuming an intent. Non-Grok exec cwd and
fork behavior are preserved.

The durable intent remains through preparation and exec failures. Binding rejects
an unprepared or wrong child before writing config, and consumes the intent only
when the intended child binds. Confirmed children are reused after interrupted
launch. An existing child without a recorded successful copy response is
ambiguous and fails closed; it is never overwritten or silently forked again.
Only a source proved absent before the copy uses the existing explicit fresh
bootstrap policy, with a diagnostic and retained orientation.

Validation: 14 preparation/ownership fixtures; 70 agent tests; 19 binding-store
tests; 25 fork unit tests; 38 fork integration tests, with the new fork/preview
case also rerun after adding its read-only assertion. The source boundary run
passed 12 checks and exposed direct signal probes in the separate test file;
those were converted to the existing typed signal API and both signal boundary
checks now pass. Formatting and whitespace checks pass.

The installed Grok 1.0.46 also passed an explicit test through this production
preparation owner, with fabricated empty history, a temporary HOME/GROK_HOME,
and a credential-free test process. The child was saved in the intended cwd,
the source summary was unchanged, and neither repository gained a binding.
This test is opt-in because it requires a provider executable; default tests use
the fake executable. It proves local fork preparation, not authenticated model
continuation. Safepay and the user's provider installation remain untouched.

Still required: validate and implement the native Stop handoff guard, setup and
payload normalization, arming/cancellation/duplicate/race contracts, and actual
wake/reaction acceptance. This milestone does not advertise a completed Grok
auto loop or establish which historical writer first changed Safepay's binding.

## Milestone 3: owned native handoff core, before rollout

Implemented `cli::grok_handoff` as Grok's separate native boundary, dispatched
by `clank stop-hook --tool grok`. It parses native camelCase identity, turn and
tool-call fields with bounded input; SessionStart binds the actual Grok session,
uses the existing fork binding checks, and resets the previous handoff. A native
cwd mismatch cannot bind through a repo override. Clank's imported Claude hook
is suppressed when the native Grok runner identifies itself; unrelated user
hooks are unaffected. Native setup is not installed by this milestone.

The model requests one native background `clank grok-wait` call. PreToolUse
reserves a random generation and rewrites the command to `exec` this Clank
executable with a short repository/label key and generation. Positive timeouts,
unsupported tool names/background shapes and replayed tagged commands are
refused. The exact provider tool-call ID, turn and command are retained.
PostToolUse must report an untruncated native `BackgroundTaskStarted` shell
result with the matching command/turn/call and running task ID before the worker
polls the existing Clank work future. Failure/permission events reject the
attempt instead. This prevents immediately pending work from completing inline
before provider background registration. Confirmation before worker startup is
persisted; confirmation afterward wakes it over local IPC.

The worker is the existing blocking wait in the same process, not a nested child
or permanent ACP helper. One fixed runtime slot per repo/label contains the full
session/generation, a nonblocking metadata lock, lifetime lease and Unix datagram
socket. Atomic, bounded records live in the vetted uid runtime directory outside
the repo. Reads refuse symlinks/non-files. Explicit lease unlock prevents a
concurrent fork's inherited descriptor from extending a released lease. There
is one 30-second registration deadline and no additional timer once idle.

Stop does not park: confirmed live/starting handoffs allow; immediately selected
work for that same turn allows its owed background completion; missing handoffs
get at most two repairs per native turn and then a visible diagnostic. The
worker never waits for Stop permission, because the provider's eight-round cap
can skip that hook. The final stdout boundary rechecks binding, effective auto
mode and generation. No delivery acknowledges an event or writes feedback.

Auto-off and binding replacement cancel/notify automatic slots at the existing
config mutation boundary. Session end, cancellation, failure and resume use the
same control channel. Existing monitor notes/local beats also check effective
auto-mode, including config edits outside that boundary, without another watcher
or cadence. Manual waits remain manual. Cancellation of a local wait future now
aborts its source tasks rather than detaching command producers.
The auto policy reuses the existing binding-check config read on idle beats.

The updated [integration contract](../../GROK-INTEGRATION.md) explains each
transition, proof, race and resource bound. Primary source inspection at the
same pinned upstream revision confirms the typed background result and that
prepared PostToolUse arguments retain the PreToolUse command rewrite. This is
source/fixture validation, not a captured authenticated installed native payload.

Validation: 20 native handoff tests, including real wait/inbox delivery without
acknowledgement; 62 wait tests, including actual producer cleanup on cancellation;
93 Stop-hook tests; 19 binding-store tests; 10 CLI tests; 14 preparation tests
(one installed-provider test remains opt-in). Thirteen source boundary checks
and six typed-config write checks pass. Formatting and whitespace checks pass.
Tests requiring local sockets ran outside the filesystem sandbox after its
socket creation denial; fixtures use temporary repos and touch no live work.

Still required before rollout: native setup with terminal-only matchers and
custom profile/home handling; bounded version/capability/doctor checks, including
enabled background-completion notifications; skill/launch instructions and
explicit handling of an existing manual waiter; direct/fork/resume lifecycle
acceptance at the fake-provider launch seam; installed native payload and real
Grok continuation/reaction acceptance when authentication is available. No
environment API authentication was available in this session. Safepay and the
user's installed Grok/configuration remain unchanged. This is a reviewable core
milestone, not a completed or deployed auto loop.

## Milestone 4: native setup, launch gates and honest diagnosis

Setup now installs the complete nine-event native adapter atomically at the
selected provider's `GROK_HOME/hooks/clank.json`, preserving unrelated hooks and
fields. Terminal events use only the verified terminal-name matcher; every
native callback has a five-second timeout. `clank setup --grok-agent <label>
--repo <repo>` resolves the actual bound/fork tool and the registered executable,
args and environment. All four Grok skills follow that profile's home, sharing
the installer/doctor inventory and preserving default-home manifest provenance.
Native installation is idempotent; malformed events cannot publish a partial
adapter. Dry run skips Grok initialization because it can create cache dirs.

The existing bounded ACP owner now supports an initialize-only handshake, with
a ten-second budget, bounded output/stderr, identity scrubbing, updater disabled
and process-group cleanup. Fork preparation retains its thirty-second deadline.
Grok 1.x >=1.0.46 and advertised `pre_tool_use`/`stop` gates establish a candidate
for the native integration. Automatic real launches require the complete adapter
at the exact profile home and this advertisement; fork launches check before
copying, previews do not probe, and explicit auto-off remains manual. Capability
errors expose neither raw provider errors/profile secrets nor initialize account
or model metadata. A missing default executable does not prevent other-tool setup.

Skills now teach the guarded `clank grok-wait` through native
`run_terminal_command` with `block_until_ms: 0`. The default resume prompt first
reaches the native Stop guard; custom/empty prompts remain respected. A Stop
snapshot identifying an existing ordinary `clank wait` records manual-task
proof and asks for an explicit terminate-or-auto-off choice. Automatic offers
are refused while that proof remains; only a later task snapshot clears it.
A missing snapshot cannot imply that a known manual task disappeared. No manual
task is cancelled or relabeled, and the existing per-turn repair bound remains.

Doctor separates adapter configuration, advertised gates, a read-only owned
phase/lease snapshot, and unverified completion-to-model/reaction evidence.
Configured profiles are checked with deduplicated bounded probes and per-home
skill inventory; a valid custom profile does not produce false missing-default
Grok skill reports. Snapshot inspection creates no runtime files, sends no IPC
and scans no processes. It shares the monitor's existing namespace trust policy,
which permits safe uid-owned 0755 directories as well as newly created 0700
ones. Native control sockets now use that same monitor's verified 0600
restriction, preventing other users sending wake hints through a readable dir.

Validation: 23 handoff tests; 10 setup/capability fixtures; 56 installer tests;
19 doctor tests; 70 agent tests; 14 preparation tests (one installed fork test
remains opt-in); 93 generic hook tests; 10 CLI/README tests. Fifteen source/typed
write boundary checks pass. Formatting and whitespace checks pass. Relevant
socket fixtures ran outside the filesystem sandbox using temporary repositories.
An additional installed Grok 1.0.46 initialize probe passed in `env -i` with
temporary HOME/GROK_HOME, disabled updater and no credentials. It advertised the
required gates and created no provider session summary or Clank binding. A
sandboxed attempt hit the bounded ten-second deadline; outside the sandbox the
probe completed promptly. Empty provider cache directories are allowed and are
not mistaken for a created session.

Still required: direct/fork/resume acceptance through the complete fake-provider
launch boundary, including first pending work and repeated completion cycles;
installed authenticated native payload and completion-to-model notification
validation; and actual model reaction to delivered work when authentication is
available. Initialize does not advertise notification enablement, and this
milestone deliberately leaves that diagnosis unverified instead of claiming a
healthy wake channel. No Safepay work, installed user hooks/skills, live session
or credentials were changed. This remains a reviewable implementation milestone.

## Milestone 5: full launch acceptance through the deterministic provider

Added `grok_launch_acceptance` to the existing single integration harness. It
uses the existing Clank binary because an in-process composer cannot exercise
exec, hook stdin/stdout, inherited environment or the generated waiter command.
The fixture's Python provider implements only the inspected ACP initialize/fork
transport and an explicit fake completion-to-turn callback. It invokes real
setup, native hook commands, background waiters and event acknowledgement. No
model, network client, terminal tab, credentials or user configuration is used.

Fresh, bound resume and an actual Clank worktree fork each deliver one event
already pending before launch and nine events created after an idle subscriber
has been accepted by the existing monitor. Every cycle rejects a duplicate
pre-offer, confirms the typed native background task and allows the genuine
Stop. The first cycle samples the actual waiter pipe before registration and
requires no inline output. After completion the driver verifies the event is
still pending; only an explicit fake-agent reaction calls the real ack command.
Ten turns per entry path give thirty complete cycles, beyond eight successive
work turns without using repeated Stop repairs for work delivery.

A single debug event in the existing wait path now reports the accepted monitor
subscription. Tests await that pipe event before creating idle work, so they
exercise a parked subscription rather than relying on a startup timing window.
This event occurs once per successful subscription, not on an idle cadence.
Doctor's native lease snapshot now explicitly leaves monitor subscription
unverified: a provider Registered record and process lease cannot prove it.

Assertions also cover read-only launch previews; profile/home paths containing
spaces; caller and profile identity scrubbing; pinned cwd/session at exec and
SessionStart; consumption of only the intended child intent; source binding and
summary preserved byte for byte; new generation each turn in one fixed runtime
slot; one shared monitor reused across all idle turns; and no ACP helper surviving
launch. Initialize counts stay at setup/launch only (two for direct/resume,
three for a copied fork), never one per idle turn. Fixture teardown addresses
only the fixture's own process group, monitor and runtime paths.

Validation: three full launch cases pass with thirty cycles; 62 relevant wait
tests and 23 native handoff tests pass; fifteen ownership/cost/identity/signal/Git
and typed-write source checks pass. Formatting and whitespace checks pass.
Local sockets require running the fixtures outside the filesystem sandbox;
its denied socket creation caused the initial sandboxed run to fail visibly.
The final fixture suite passes in about fifteen seconds on this host. Tests
remain in the existing integration target and add no new test executable.

The fake-provider launch acceptance gap called out by review is closed. The
remaining evidence gap is **installed authenticated native hook payloads,
enabled background completion-to-model notifications, and an actual model turn
reacting to delivered work followed by another idle cycle**. The fixture
explicitly models that callback and is not advertised as an authenticated Grok
continuation. Offline installed-provider initialize/fork evidence cannot fill
that gap. Safepay and the user's installed provider/hooks/skills are untouched;
no credentials were read, copied or introduced for these tests.
