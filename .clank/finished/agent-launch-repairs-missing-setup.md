# agent-launch-repairs-missing-setup
# Agent launch repairs missing setup

## Why

Launching Grok currently refuses to proceed when its native Clank adapter is
missing and instructs the user to run setup manually. The user requests automatic
setup for ALL supported agents whenever required integration assets are missing.

## Contract

- Before a real Clank-managed agent launch, resolve the actual provider and its
  configuration home, inspect its required Clank integration, and run the existing
  setup process when assets are missing. Revalidate before launching. Cover Claude,
  Codex, Grok and OpenCode, including profiles and fork-selected providers.
- Trace every launch/resume/swap/fork/open path and put the check at their shared
  launch boundary where possible. Creating a fork without launching it should not
  install integrations. Preview/dry-run paths stay read-only.
- Missing hooks/plugins/required role skills must be repaired using setup's asset
  inventory and merge/ownership rules, not an independent installer or hand-written
  provider files. If setup needs a reusable provider-scoped entry point, extract it
  and have explicit setup and automatic launch share it. Default and custom homes
  must resolve identically to the actual launched process; an agent label selects
  a profile, not exclusive ownership of installed hooks.
- Preserve unrelated hooks, user settings, locally edited assets and credentials.
  Automatic repair must not use --force. Malformed/conflicting configuration or
  unavailable write access must produce a precise actionable error rather than
  overwrite user data or continue with broken automatic operation. Setup failure
  must not start the provider, create duplicate tabs, or claim successful launch.
- Run repair once per launch attempt, recheck, and stop with a useful diagnostic if
  it remains incomplete. Multiple simultaneous launches sharing a home must merge
  safely and avoid lost updates; reuse existing installation locks or add locking
  at the common setup transaction boundary. Successful repeat launches must be
  idempotent. A missing integration is recoverable; an unsupported provider version
  or failed capability probe must not become an endless setup loop.
- Run only at real launch boundaries. Add no polling, background process, periodic
  capability probe or checks in idle stop hooks. Check the selected provider rather
  than probing every installed provider on every launch. Retain explicit manual
  operation and define which assets it actually requires before implementation.
- Tell the user briefly when automatic setup repairs an integration; ordinary
  healthy launches remain quiet. Update setup/recovery documentation and errors so
  default-home users are not unnecessarily asked to choose an agent label.

## Implementation and validation

Initial trace: real pane launches converge on `agent start`; Grok readiness is
checked before fork preparation and before ordinary execution. Non-Grok fork
intent is currently consumed during composition, so repair must precede consuming
that intent as well as provider execution. Existing `setup::run` installs all
providers and probes Claude/Grok, so automatic selected-provider repair needs a
shared scoped installation seam rather than invoking unrelated provider probes.
Explicit full setup must retain its existing behavior through that shared seam.

1. Trace provider/home resolution, launch paths, setup inventory/merge semantics,
   capability checks, ownership and concurrency. Specify the required asset set per
   provider and the manual-mode behavior before implementation.
2. Extract/reuse the setup installation path and wire repair/revalidation into all
   real launch paths. Preserve read-only previews and actual profile selection.
3. Test absent integration for each supported provider, default/custom homes,
   multiple labels sharing a home, idempotence, concurrency, unrelated settings,
   locally edited assets, malformed JSON, write/setup failure and unsupported
   capabilities. Use isolated fixture homes and stub providers; no real accounts.
   Exercise the launch boundary so setup completes before the provider starts and
   failed repair does not launch or create a misleading successful tab.
4. Run affected tests and repository boundary checks; document behavior and limits.
   Submit milestones through Clank review. Do not install into live homes or change
   the installed binary merely by queueing this plan.

## Exit

Every supported Clank-managed agent can recover missing required integration at
launch through the shared setup process, without manual setup instructions in the
ordinary missing-file case, unsafe overwrites, duplicate launches or idle overhead.

## Reviewed implementation contract: launch and setup

### Launch ordering and profile resolution

`agent::start` is the real provider execution boundary used by opened panes. It
currently checks Grok before ACP fork preparation and before ordinary execution;
non-Grok fork composition consumes its intent before execution. Refactor selection
so the bound session's tool wins, otherwise the fork spec's tool wins, otherwise
the roster's tool wins. Resolve setup against that actual tool and its effective
launch environment, not the invoking agent or stale roster tool. Keep `--print`
before repair/probe and do not repair when only attaching to an existing holder.

Resolve configuration roots from inherited environment with launch overrides
applied exactly as in `launch_env`. Resolve relative paths against the launched
provider's working directory. Extract the same root resolver for setup and launch;
do not temporarily mutate process environment to call full setup. Grok already
has `Provider::resolve` for executable/profile/GROK_HOME. Establish the actual
supported home overrides for Claude, Codex and OpenCode from their launch/config
contracts before adding equivalent projections; default roots remain `.claude`,
`.codex`, `.grok`, and `.config/opencode` beneath effective HOME. Unknown executable
wrappers cannot justify guessing another home or rewriting their private profiles.

The real launch sequence is: select provider/profile; inspect required integration;
repair if needed; re-inspect; perform selected-provider capability checks; prepare
or consume fork intent; compose and execute. A failure leaves fork intent intact.
Creating a worktree/fork alone does not run installation. Trace swap/open/resume
callers to ensure none bypass this boundary. Do not create another execution path.

### Required assets and manual mode

All four tools need the existing inventory's master, reviewer, PR-review and
GitHub event skills. Claude additionally needs its `/clank` command and complete
Clank Stop/SessionStart entries in settings.json. Codex needs Stop/SessionStart
entries in hooks.json and the existing Clank prefix rule installer. OpenCode needs
the Clank plugin from the same owned-asset inventory. Grok needs all nine native
adapter entries in `grok_setup::EVENTS`, including terminal matchers and command
timeouts. Derive these checks from the same definitions used for installation,
not independent expected JSON literals. Include safe obsolete-skill cleanup through
the existing installer rules, rather than blindly deleting unrelated directories.

For manual/auto-off launches, install the same provider assets: hook/plugin guards
already consult auto mode, session binding is still useful, and enabling auto later
must not require another setup. Preserve Grok's current auto-off capability bypass;
manual mode must not require an automatic handoff or unsupported native gates.
Auto-off is not permission to overwrite malformed or locally edited setup assets.

### Shared installer and repair transaction

Extract a provider-scoped installation operation using `user_asset_inventory`,
the existing skill ownership manifest, hook merge routines, rule installer and
obsolete-skill handling. Explicit full `setup::run` iterates the same operation for
all providers and retains its current user-global autosquash default seeding and
output. Launch repair does not seed unrelated product defaults or probe other
providers. Custom provider roots receive projected relative paths and provenance;
preserve recognition of manifests already produced by full/default setup.

Inspection returns Ready, NeedsRepair, or Invalid with path/reason. Missing required
assets or incomplete setup-owned hook entries trigger repair once; unreadable,
malformed or conflicting configuration is Invalid. Reinspection must report the
specific remaining asset when repair cannot establish readiness. Do not force,
bless arbitrary content into provenance, or convert errors into missing state.
Use existing setup ownership classification for current/canonical alternate/known
old assets and user drift. Automatic repair follows non-force setup semantics;
drift is preserved and actionable, not overwritten. Configuration readiness means
the expected integration is installed, not that a provider has run it.

Acquire a shared advisory installation lock for the effective installation domain
before reinspection, read/merge/write and manifest updates. Explicit setup uses
the same lock; concurrent labels/profiles sharing a home must not lose additions
or provenance. Do not lock per label. If a transaction needs multiple roots, acquire
in a stable order. Hold locks only for filesystem inspection/writes, not executable
probes, network access, ACP preparation or provider execution. Publish each merged
file atomically; partial failure reports failure and the next run resumes safely.
An all-files atomic transaction is not claimed. Invalid pre-existing input must be
detected before rewriting that file. External editors that ignore advisory locks
are outside the concurrent-Clank-writer guarantee.

Healthy launch inspection is read-only and quiet. A repair emits one short stderr
notice naming the selected provider and resolved root, followed by precise failure
if applicable; do not print account metadata or secret launch arguments. No setup
loop, idle check, background installer, or new polling cadence is introduced.
Claude delivery-mode probing, when needed, must use its selected executable/profile
and keep hook entries and skill text consistent. Preserve Grok's bounded readiness
probe after installation for automatic launches; unsupported versions fail once
without rerunning setup. Preview performs neither probes nor writes.

### Observed Grok failure in clankstack

The user's live clankstack session ran bare `clank grok-wait`; it exited with the
missing-native-hooks diagnostic. The installed adapter now exists, Stop supplied
arming feedback, and the runtime record has two repairs and cancelled state.
The missing-arguments path at `grok_handoff::wait` proves only that native
PreToolUse did not supply a tagged command. It does not prove the adapter file is
absent. Installation alone cannot claim hooks were loaded, dispatched, or able to
continue the model. The exact reason the rewrite failed is not yet established.

Include this case in the implementation investigation and regression acceptance:
distinguish missing installation from missing runtime tagging; preserve the existing
bounded repair limit; do not tell the model to repeat an untagged command forever.
Check matcher/event envelopes and actual provider dispatch against its installed
native integration, then fix the installation/discovery/dispatch mismatch in this
plan. Use an isolated provider fixture to drive PreToolUse rewrite,
PostToolUse background registration, Stop and wake, alongside the case where Stop
runs but PreToolUse does not. The user explicitly requires this fix in the same
plan: do not defer the observed clankstack failure to another queued plan or mark
installation complete merely because hook files exist. Prove the supported Grok
integration can tag and register the wait through its actual native lifecycle,
and state any remaining provider limitation precisely. Do not report a live Grok
wake as proven by a stub test. Preserve the existing handoff architecture unless
the reproduced failure demonstrates that a change is necessary.

### Implementation stages and evidence

1. Shared scoped installer, root resolution, inspection and concurrent installation
   tests. Explicit setup and automatic repair use the same seams.
2. Launch ordering across tools, forks and holders; fixture tests prove setup is
   complete before execution and failure preserves intent. Test distinct roots,
   shared roots, missing individual assets, invalid input, drift and write failures.
3. Fix and verify the observed runtime handoff, with regression tests, accurate
   messages and docs. Verify selected
   provider only, healthy/preview paths do not write, unsupported capability does
   not retry setup, and no idle overhead. Run affected repository boundary checks.

This commit specifies contracts only. No implementation or live setup was changed.

## Milestone: shared provider-scoped installation

`setup::run` now installs each provider through `install_provider`; the same scoped
operation is available to the future launch repair boundary. Its inventory is
derived from `provider_asset_inventory`, and the combined inventory used by doctor
is assembled from those exact entries. A scoped installation neither installs
another provider nor runs executable probes. Full setup retains its all-provider
installation, Claude mode selection, Grok probe and autosquash default behavior.

Default providers preserve the existing user-home manifest paths. Custom provider
roots project their assets and manifest into that root. An alias resolving to the
default root preserves default-home provenance. Installations serialize both the
actual provider files and their manifest domain, acquiring canonical roots in a
stable order. This protects concurrent labels/providers and default/custom aliases
without holding a lock over probes or provider execution. Dry-run creates no lock
or configuration directories. The installation lock is never unlinked.

Skill/plugin files, Claude/Codex merged hooks and Codex rules now publish via an
atomic temporary-file replacement; the Grok adapter and manifest already used
atomic publication. The common helper preserves symlinked destinations and existing
permissions, and refuses read-only destinations. Non-force ownership/drift behavior
and unrelated hook fields remain unchanged. This is restart-safe per-file
publication, not an all-assets rollback transaction.

Validation: 63 setup tests, 19 doctor tests and both git-boundary integration tests
passed, plus formatting/whitespace checks. New fixtures cover every selected
provider, custom roots, default aliases, dry-run, eight concurrent installers,
manifest preservation, unrelated hooks, drift, malformed configuration, and
symlinked settings/permissions. Existing Grok fresh/resume acceptance passed.
Fork acceptance initially failed with a nonblocking handoff metadata-lock busy
diagnostic; its isolated rerun passed (19.48s). No installer error was involved,
but retain that contention observation for the same plan's runtime investigation.

Further live-log inspection corrects the earlier hypothesis: clankstack's
`updates.jsonl` records successful execution of global/clank PreToolUse and
PostToolUse handlers, yet the task command stayed bare. The handler ran, so file
discovery alone is not the missing step. The reason it did not return/apply a tagged
command remains unproven. The installed native documentation and official Grok
hook serialization both specify camel-case input and PreToolUse updatedInput;
fixture behavior is not sufficient evidence about this live session's payload.

Remaining: effective profile/root resolution for all launch tools, read-only
inspection, automatic non-force repair/recheck before consuming fork intent,
manual/preview/holder paths, exact live Grok handoff diagnosis and fix, contention
and lifecycle regression coverage, and documentation. This milestone does not wire
automatic launch repair or fix the user's live Grok session. No live installation
or installed binary was changed.

## Milestone: repair before provider execution

Real agent starts now inspect the selected provider's effective configuration
root, repair through the shared non-force installer, and revalidate before
execution or consuming fork intent. Preview and existing-holder attachment paths
do not install. Effective launch environment overrides select HOME and the
provider-specific configuration root; relative paths follow the provider's
working directory. Healthy launches remain quiet and read-only. Claude's initial
delivery-mode probe uses the selected executable and environment; existing owned
hook entries avoid repeat probes. Grok's doctor asset projection now shares the
installer's canonical default/custom manifest-domain resolution.

Validation passed: 68 setup tests, 70 agent tests, 19 doctor tests, 10 Grok setup
tests (one pre-existing ignored), two launch-repair acceptance tests, two git
boundary tests, formatting and whitespace checks. Real CLI fixture launches prove
repair precedes Claude, Codex and OpenCode execution; malformed Codex setup
preserves fork intent and does not execute the provider. A fresh Grok fixture
without installed hooks repairs them at launch and completes ten native handoff
cycles with one capability probe. These are isolated fixtures, not evidence that
the user's live clankstack session is repaired. No live installation or installed
binary was changed.

### User steering: one wait command

Remove the separate `clank grok-wait` command. `clank wait` must select Grok's
automatic lifecycle handoff from the bound session/environment, without requiring
the model to choose a provider-specific command. Preserve the native PreToolUse
tagging, PostToolUse background registration, session/turn/generation ownership,
bounded recovery and wake delivery contracts. Internal correlation parameters may
remain hidden implementation details; ordinary agent instructions say only
`clank wait`. Manual/auto-off operation and read-only peek must remain usable.
Update the CLI, generated hook instructions, shipped skills, documentation and
acceptance fixtures together, and remove the obsolete public command rather than
leaving an alias. Add no polling or idle capability probes.

Remaining in this plan: implement the unified wait surface and diagnose/fix the
actual untagged live Grok command; retain the metadata contention investigation;
verify OpenCode's additive custom/global plugin loading and Codex trust/profile
root alignment; complete lifecycle regressions and documentation. Do not finalize
on the strength of installation and stub lifecycle tests alone.

## Milestone: unified wait and native payload recovery

The CLI now exposes one `clank wait` command. Session environment selects Grok's
automatic handoff; native PreToolUse supplies hidden key/generation parameters.
Peek, observer and hook-owned waits retain their existing paths, and auto-off
retains manual operation. The separate `grok-wait` command is removed. Skills,
bootstrap instructions, README, integration contract and fixture commands use the
unified surface. Worker registration and delivery still use the same process and
monitor pipeline, with no added polling or idle timer.

The live clankstack log shows successful Pre/Post handler execution followed by
untagged failed waits; it also explicitly reports `unknown decision value 'allow'`
for exhausted Stop repairs. Inspection of official Grok tool-call dispatch reveals
that both native PreToolUse and PostToolUse envelopes are built with no promptId.
Clank previously required that field for an offer and registration. This milestone
accepts those actual envelopes and correlates by exact toolUseId, tool name,
session, generation and command. If a promptId is supplied it must still match.
A preceding Stop prompt is a turn hint, never substitute tool-call identity.

Stop no longer emits the invalid allow decision. Two unsuccessful repair attempts
remain exhausted across synthetic background-completion prompts, preventing failed
waits from resetting the budget indefinitely. Successful registration or a genuine
SessionStart resets recovery state, so resumed sessions are not stranded by an old
exhausted attempt. Untagged automatic waits accurately report missing native
rewrite rather than claiming adapter files are absent.

Validation: 25 handoff tests, 50 command tests, 83 agent-related tests, 68 setup
tests and two git boundary tests passed. Four native launch acceptance cases cover
fresh, resume, fork and automatic missing-setup installation, each with ten wake
cycles; fixture Pre/Post payloads intentionally omit promptId. Regression cases
also reject the removed command, enforce paired internal flags, preserve manual
operation, fence mismatched native calls and bound repairs across completion IDs.

The user explicitly requested reinstalling the fix and hooks. The release build
was installed with locked dependencies; non-force setup refreshed Grok role skills
for the clankstack profile and confirmed the complete adapter and Grok 1.0.46's
advertised PreToolUse/Stop gates. Doctor confirms the updated skills; it still
correctly distinguishes advertised gates from actual authenticated model reaction.
No pending events were acknowledged as part of recovery. A live model wake is not
claimed from fixture acceptance or setup. Resume the affected Grok session to run
SessionStart and re-arm with the new command.

Remaining before finalization: metadata contention investigation, OpenCode additive
plugin-root verification, Codex trust/profile-root alignment, doctor profile paths,
and final documentation/acceptance review. This milestone fixes the demonstrated
protocol incompatibilities; it does not assert that every remaining contract in
the larger setup plan is complete.

## Milestone: selected Codex profile trust

The pre-execution Codex trust step now resolves HOME and CODEX_HOME from the
composed launch's environment overrides and inherited environment, using the
same provider-root resolver and working directory as setup and execution. It
writes the selected profile's config.toml rather than the invoking shell's
default home. The actual launch tool still determines whether this step runs,
and preview returns before it. Existing trust or distrust entries remain intact;
failure remains a best-effort warning rather than a broken launch.

Trust read/append/publication now uses the shared installation lock at the
canonical profile root and the existing atomic writer. Simultaneous Clank
launches cannot lose each other's project entries; this does not claim protection
against external writers that ignore that lock. Locks cover only local file
operations, with no probes, network requests, idle checks or polling.

Validation: 85 agent-related unit tests, two launch acceptance tests and both
git boundary tests passed, plus formatting and whitespace checks. New tests
cover inherited versus launch-overridden roots, HOME fallback, relative roots
resolved against actual working directory, and eight simultaneous project trust
updates. The real Codex launch fixture asserts the selected profile's trust
exists before provider execution and the calling default .codex/config.toml
remains absent. Existing explicit distrust and idempotence coverage still passes.

Remaining: Grok metadata contention, OpenCode additive plugin loading, doctor
profile-root parity, final documentation and acceptance review. The already
installed Grok loop fix remains in place; this Codex-only milestone has not
changed the installed binary or live provider configuration.

## Milestone: serialize Grok metadata transitions

Ordinary metadata contention no longer drops a native hook or worker transition
with a "busy" error. The same stable regular-file lock now uses blocking flock
for short synchronous read/transition/publication sections. Contenders park in the
kernel; only interrupted syscalls retry, with no timer, polling loop or new idle
work. Lifetime worker leases still use nonblocking flock, so duplicate workers are
refused rather than queued. Metadata holders never await, launch providers, call
the network or block acquiring a worker lease. Configuration reads under the
metadata lock are unlocked reads; cancellation callbacks cannot form a lock cycle.
Lock files are never unlinked or replaced, and genuine I/O/unsafe-state errors
remain explicit. Read-only doctor diagnosis still acquires no metadata lock.

Validation: all 26 handoff tests, four native launch acceptance cases (forty wake
cycles), both git-boundary tests, formatting and whitespace checks passed. A new
contention fixture holds the lock, establishes that the contender observes it
busy, releases it, and proves the contender completes rather than errors. The
same inode survives reacquisition. Existing duplicate-worker, cancellation,
registration ordering and output fencing tests continue to pass.

At the user's existing reinstall request, the latest release was rebuilt and
installed with locked offline dependencies, including the reviewed Codex profile
trust milestone. The installed clank executable matches the release artifact.
Non-force setup for clankstack's Grok profile confirms its current hooks and
skills; no pending events were acknowledged. Installation is not evidence of an
authenticated model wake.

Remaining: OpenCode additive plugin-root verification, doctor profile-root parity,
final documentation and acceptance review. Authenticated Grok model reaction is
still not inferred from deterministic fixture cycles.

## Milestone: OpenCode additive plugin roots

Official OpenCode configuration discovery includes the global configuration root
and OPENCODE_CONFIG_DIR additively. Its local-plugin deduplication uses exact file
URLs, so global and selected-profile Clank files are distinct modules. The loader
creates one SDK client and passes the same input to each plugin in an instance:

- https://github.com/anomalyco/opencode/blob/dev/packages/opencode/src/config/paths.ts
- https://github.com/anomalyco/opencode/blob/dev/packages/opencode/src/config/plugin.ts
- https://github.com/anomalyco/opencode/blob/dev/packages/opencode/src/plugin/index.ts

Clank's plugin now uses a process symbol and WeakSet of client objects. The first
copy owns that client's hooks and bootstrap; subsequent copies return no hooks
and start no wait. Different client instances retain independent lifecycle state;
the weak registry does not retain disposed clients. This introduces no scan,
timer, watcher or new background helper. Ownership is per actual loader client,
not one process-global session or a guessed project path.

Launch inspection now includes an existing global Clank plugin for OpenCode,
resolved through effective HOME/XDG_CONFIG_HOME without the custom-directory
override, alongside the selected root. This is a clarification of selected-provider
repair: OpenCode loads both roots, so an older setup-owned global copy must also
be upgraded to participate in the shared guard. A missing global copy is not
created merely because a custom profile is launched. Canonical aliases collapse
to one inspection. Every loaded root is inspected before repairs begin; ownership
drift fails without overwriting it or repairing another missing profile asset.
Repair still calls the same provider-scoped installer and revalidates each root.
External writers that ignore installation locks remain outside the concurrent
Clank-writer guarantee, and publication is per-file rather than all-root atomic.

Validation: 69 setup tests, two launch acceptance tests, both git-boundary tests,
the Node lifecycle harness, formatting and whitespace checks passed. The JS
harness imports two actual filesystem modules and verifies the second exports no
hooks and does not bootstrap another wait; existing separate-client lifecycle
cases still pass. Rust fixtures exercise an old manifest-owned global plugin plus
a fresh selected profile, healthy read-only repetition, and edited global content
preserved before repairing a missing selected skill. An isolated installed
OpenCode debug-config probe timed out after 30 seconds; no helper remained. This
is not claimed as real-provider discovery or model-wake acceptance.

This milestone changes source and fixture documentation only; the previously
installed Grok/Codex fixes remain installed. Refresh the OpenCode plugin after
review with the final release. Remaining: doctor profile-root parity, final docs
and acceptance review, and release/setup refresh for subsequent changes.

## Milestone: doctor follows launch profiles

Launch repair and doctor now share read-only effective HOME, cwd and provider-root
selection. Doctor resolves actual bound/fork tools before roster defaults and
checks configured non-Grok profiles, retaining inherited/default checks for tools
without a profile. Identical profiles reuse a single inspection and Claude probe.
OpenCode diagnosis covers an existing additive global plugin and the selected
root; it does not create a missing global copy. Asset diagnostics name the actual
resolved paths, not a misleading default-home abbreviation.

Claude capability checks run the selected executable with its launch args,
environment and cwd, and compare its supported hook mode against that profile's
settings. A failed probe reports uncertainty rather than silently probing a
different binary. Grok asset paths and default auto-mode lookup now use the
effective launch HOME. Ambient path reads preserve OS strings, including inherited
non-UTF-8 provider paths. All diagnosis remains read-only, with capability probes
only at the explicit doctor command boundary; no new idle task or polling loop.

New isolated fixtures cover custom Codex HOME plus a relative CODEX_HOME, selected
Claude executable/mode, and additive OpenCode roots with a missing selected root.
They assert actual paths and no diagnostic installation. The initial Claude
fixture advertised 2.1.0 while expecting async-rewake support; corrected it to
the supported 2.1.223 threshold. Existing setup, agent, ambient-environment,
launch-acceptance and git-boundary checks also pass. Validation: 22 doctor tests,
69 setup tests, 85 agent tests, six ambient-environment tests, two launch
acceptance tests and both git-boundary tests passed; formatting and whitespace
checks passed.

README now explains automatic provider-scoped launch repair, preserved local
edits, read-only preview/diagnosis, profile-aware paths and absence of idle setup
work. Remaining: gate acceptance and release/setup refresh for the OpenCode and
doctor changes. Live authenticated Grok model-wake acceptance remains unproven;
configuration checks and native fixtures are not substitutes for that evidence.

## Final acceptance and installed release

Reviewed all launch/setup contracts against the milestone evidence above. The
shared launch boundary covers actual provider selection and fork intent ordering;
scoped installation covers ownership, merge safety, idempotence and concurrent
writers; selected-profile diagnosis is read-only; the demonstrated Grok envelope
and unbounded repair issues are fixed with native lifecycle regression coverage.
No setup daemon, recurring probe or idle polling was added. The OpenCode lifecycle
harness passed again before refreshing its installed plugin.

Built the reviewed source with `cargo build -p clank --release --locked --offline`
and reinstalled with `cargo install --path crates/cli --locked --offline --force`.
The installed clank binary is byte-identical to target/release/clank. Then ran
non-force `clank setup --grok-agent grok --repo /Users/llfourn/src/clankstack`:
Claude/Codex hooks merged, all provider skills verified, the Grok adapter verified,
and the owned global OpenCode plugin upgraded. Installed OpenCode plugin bytes
match the embedded source. CLI help exposes `wait`, with no `grok-wait` command.
Grok 1.0.46 advertises native PreToolUse and Stop gates.

The installed binary's `doctor --repo /Users/llfourn/src/clankstack --json` completed
with exit 0: 39 checks, 36 OK, three WARN and no FAIL. Its read-only Grok snapshot
reports a confirmed native background task and held owned-waiter lease. This is
live registration evidence, not proof of monitor subscription or subsequent
model reaction. The other warnings are historical orphan Claude agent state and
the explicit unverified background-to-model continuation capability. No orphan
state was deleted, pending events acknowledged or live session restarted as part
of diagnosis.

Release/setup refresh is complete. Submit this evidence to the final acceptance
gate; finalization still requires its FINISHED verdict. Remaining limitation:
authenticated end-to-end model wake has not been observed. The bounded fallback
preserves pending work if native registration cannot be established; it must not
be described as proof that all provider continuation behavior is healthy.
