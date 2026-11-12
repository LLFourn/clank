# the-monitor-resolves-grok-continuations
# The monitor resolves Grok continuations

## Why

Release candidate 9d130109 passed the previously failing Grok launch tests but
failed Linux CI 43177454995 at `hook_is_a_messenger`: the recovery correction
reads agent configuration inside grok_hook.rs to interpret stopHookActive.
The user explicitly reaffirmed that hooks must not read configuration owned by
other components. Provider hooks report evidence; the monitor owns policy.
No v0.2.0 tag has been created.

The user's subsequent instruction expands this to every installed hook and its
transitive helpers: keep hooks minimal and keep monitor-owned configuration
and decisions out of their processes. This is a scope revision before any
implementation, not an additional implementation milestone.

## Audit findings and boundary

- Grok's `continued_wait` directly reads agent configuration to reinterpret
  `stopHookActive`.
- Every provider shares `turn_child::unmonitored`, which calls `Gathered::of`
  inside the carrier process. This reads auto mode, role, question and block
  state. Calling a monitor module does not make this monitor-owned execution.
  The current architecture gate explicitly overlooks this path.
- Claude's question-hook command calls `record`, which writes question state
  directly before contacting the question broker. The command and broker share
  a module, so the existing hook-module gate misses this write.
- Stop, SessionStart, PostCompact and subagent adapters otherwise carry native
  evidence and use identity/binding helpers. Recovery adapters submit control
  messages and deliver monitor-authorized retries. The OpenCode plugin uses
  native session events and local delivery guards, with no config-file reads.
  Main/command dispatch does not preload repo policy. The Git post-rewrite
  hook migrates feedback from Git's supplied rewrite pairs, not agent policy.
- Binding/bootstrap validation, transcript evidence, private upgrade handoff
  files, socket ownership and process lifetime are legitimate hook concerns.
  Reading a shared config document for binding must not expose its policy
  fields to hook code or make unrelated policy parsing a prerequisite for
  routing. Check those helper boundaries explicitly during implementation.

## Complete change

- Remove the Grok hook's decision-config read entirely. Report the native
  continuation flag unchanged. Resolve outage versus wait-arm continuation
  using the monitor's gathered state before its verdict clears the outage marker.
  Do not evade this boundary by hiding the read in another hook-side helper.
- Preserve the reviewed monitor behavior: recovery requests the first wait, genuine arm
  continuations cannot duplicate waits, registered waits remain authoritative,
  and stale bindings/manual mode/pending work remain protected.
- Remove the carrier's local policy fallback, not merely its direct call to
  `Gathered`. Without a monitor verdict, report a transport diagnostic rather
  than fabricate a model wake or inspect auto/role/block/question configuration.
  Keep bounded attachment/reconnection, owner cancellation and pending-work
  preservation. Document this deliberate outage behavior change: an unavailable
  monitor cannot authorize a new model turn. Preserve handling of legacy outage
  markers when the monitor resumes; adapt the native recovery regression to
  distinguish old markers from new diagnostic-only outages.
- Send Claude question evidence to the monitor for persistence, independently
  of whether a remote viewer/question broker is running. Keep remote question
  response transport and native terminal answering intact. Separate the thin
  command adapter from the broker implementation so their ownership is clear.
- Expand the architecture gate to the omitted hook entrypoints and shared
  paths, including the question adapter, carrier fallback and OpenCode plugin.
  Remove the fallback exception; do not weaken existing forbidden operations
  or hide them behind new helpers. Add behavioral regression coverage for
  missing/unreadable policy config and absent monitors, along with question
  evidence delivery without a viewer and monitor-owned Grok interpretation.
- Finish the transitive audit of installed hook entrypoints and record their
  permitted data dependencies. Avoid unrelated refactors of the broker, Git
  feedback migration or session transport.
- Run focused hook/custody tests, the full CLI integration harness (including
  architecture gates), formatting and strict workspace Clippy. Commit one
  complete fix for review.

After review and finalization, continue the authorized v0.2.0 release: push the
corrected commit normally, require full green CI at that SHA, then create the
immutable annotated tag, follow native Release builds and verify assets and the
temporary installer smoke test. Never tag either failed candidate.

## Implementation and validation

All installed hook entrypoints and their shared helpers were audited; the
permitted dependencies are recorded in `docs/hook-boundary.md`. Grok reports
the native continuation flag unchanged, and monitor custody interprets legacy
outage markers before the monitor clears them. The carrier no longer gathers
policy or authorizes outage wakes. It requires an explicitly routed identity
and emits diagnostics when no monitor verdict arrives.

Claude question adapters now submit native evidence to the monitor, which
validates the binding and persists it before acknowledging, independently of
the remote viewer. Binding helpers project session identity without parsing
unrelated policy and preserve those fields during identity updates. Architecture
gates cover these shared paths and the OpenCode plugin as well as adapters.

Validation: 211 focused unit tests passed (one optional test ignored), and the
full CLI integration harness passed 431 tests (two optional tests ignored),
including native Grok outage recovery and the architecture gates. After the
last carrier and question-persistence regressions were added, both passed and
all four hook-boundary tests passed. Strict workspace/all-target Clippy,
formatting and diff whitespace checks passed on the final implementation.

Review correction: split the durable-format writer regression between policy
and identity ownership. Policy writers still reject invalid policy, while
bind/release preserve opaque policy and reject malformed JSON, unsupported
versions and invalid identity without changing bytes. The concurrent hook/wait
regression now holds diagnostic delivery with channels, proving that the hook
neither spends the wait's outage notice nor changes its persisted policy state.
No production behavior changed in this correction.

The requested full workspace suite passed: 3,586 CLI unit tests (four ignored),
431 CLI integration tests (two ignored), and all other workspace tests and
doctests (three ignored). The 33 focused correction tests, strict workspace
Clippy, formatting and whitespace checks also passed.
