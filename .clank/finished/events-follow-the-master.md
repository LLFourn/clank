# events-follow-the-master
# A watch follows the master unless explicitly assigned to an agent

## Why

Lloyd, 2026-11-12: GitHub watches disappeared from the TUI and stopped running
when Claude left the roster and Codex became master. Their configuration was
stored against Claude. Repository work usually belongs to whoever is master,
not to the agent that happened to configure the watch.

Make master the default target when no agent is specified. This is a persistent
role target, not shorthand that resolves to today's master's name at creation.

## Contract and CLI

- `clank events watch add '<json>'` creates a repository watch targeting the
  master role, regardless of the invoking session's binding or role. It must
  also work from an ordinary human shell without an agent binding.
- An explicit named-agent selector keeps a watch pinned to that agent. Prefer
  the existing `--author <agent>` spelling unless the current CLI conventions
  justify changing it; do not require a new spelling merely for this plan.
- Make list/remove address the same default master scope and expose each
  watch's target clearly. Provide an unambiguous way to inspect all targets.
  Choose the smallest coherent CLI surface during implementation; document
  exact commands and errors, including named agents and names resembling roles.
- Apply role targeting consistently to persistent GitHub, command and timer
  watches. Do not silently change unrelated event acknowledgments, messages,
  or temporary `clank wait --event` behavior as a side effect of CLI defaults.
- Store the target explicitly at repository scope; do not copy configuration
  among per-agent files each time master changes. Follow existing store and
  locking boundaries for concurrent configuration and roster updates.

## Handover and durable state

- On a master change, route subsequent work to the new master and stop waking
  the old master for the role-owned watch. Named-agent watches remain pinned.
- Preserve source identity, poll cursors/baselines, timer progress, event
  history and acknowledgment state across handover. A role change must not
  silently baseline away new work, replay acknowledged history, or duplicate
  producer instances. Restart only where a producer's actual configuration
  or agent-specific execution context requires it.
- Define the exact ownership boundary for events already delivered but not
  acknowledged, concurrent acknowledgment, and a master change during an
  in-flight poll or producer emission. Pending role work must remain visible
  and become actionable by the new master. Preserve at-least-once delivery;
  do not claim exactly-once execution of external actions during handover.
- If no master exists, retain configuration and pending work, show the source
  as awaiting a master, and resume correctly when one is selected. Specify
  whether ingestion continues while unassigned; never silently discard events.
- The TUI EVENTS panel and `clank status` show master-targeted sources and their
  current recipient, including unassigned sources. Timeline/history remains
  inspectable after agents leave the roster.

## Compatibility and this repository

- Existing per-agent watches stay explicitly pinned on upgrade. Do not infer
  role ownership from an agent's historical role or rewrite every old watch.
- Provide a supported way to transfer an existing watch to the master target
  while preserving its durable state. Account for an agent already removed
  from the roster, as in this repository. Avoid editing config JSON by hand.
- Use that path for this repo's existing `LLFourn/clank` CI watch left under
  Claude, preserving its event filters and operator prompt. Confirm it is
  hosted and visible for the current master. Do not act on historical CI or
  merge/publish anything merely as a migration test.
- Update CLI help, relevant book/docs and master/GitHub skill text to explain
  the default, explicit pinning, handover and migration behavior.

## Validation

Use shared Rust fixtures and direct calls for policy/store coverage. Exercise
creation without a binding, creation by a non-master, named-agent pinning,
master reassignment, removed-agent migration, no-master recovery, concurrent
configuration changes, pending/acknowledged events and an in-flight emission.
Cover GitHub baseline/cursor preservation, a pending timer and command producer
ownership without requiring live GitHub or a paid harness. Verify TUI and CLI
show the target and recipient accurately. Retain a small native producer test
where process ownership is the contract; avoid sleeps, new polling and nested
binary builds.

Implement as one complete plan after the current test cleanup. Publishing a
release and unrelated event-system redesign are outside scope.
