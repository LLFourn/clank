# Hook ownership

Provider hooks report native evidence and deliver the monitor's verdict. They
must not resolve auto mode, roles, pending work, blocks, question state or
recovery policy, even by calling a helper in a monitor module.

| Entry point | Permitted data and effects |
| --- | --- |
| Claude/Codex Stop, SessionStart, PostCompact, subagent hooks | Native payload and transcript evidence; binding/bootstrap identity; monitor exchange; provider reply framing |
| Grok lifecycle hooks | Native payload, raw continuation flag, launch/binding identity; monitor exchange; provider reply framing |
| Recovery hooks | Native failure evidence and delivery results; monitor-authorized retry transport |
| Claude question hooks | Native question evidence sent to the monitor; connection to the optional question broker for remote replies |
| OpenCode plugin | Native SDK session/events, volatile delivery guards, hook subprocess lifetime and monitor replies; no config-file access |
| Shared carrier/owner/upgrade handoff | Socket ownership, process lifetime, private upgrade handoff and binding identity; no policy fallback |
| Git post-rewrite | Git's supplied rewrite pairs and feedback migration; no agent turn policy |

Binding helpers project only session identity and bootstrap tool from shared
JSON documents. Identity writes preserve other fields without parsing their
policy. Missing, invalid or unsupported identity documents cannot authorize a
binding. Main/command dispatch does not preload repo policy for hooks.

The monitor records question observations even without a viewer. The separate
question broker owns remote answer transport; its policy/state reads are not
executed by the hook command. Grok continuation interpretation, including legacy
outage markers, happens in monitor custody before those markers are cleared.

If attachment/reconnection or a hook deadline expires without a verdict, the
hook emits a transport diagnostic. It neither manufactures a wake nor marks
work delivered. Automatic progress may stop until the monitor becomes available
and a new hook/wait runs. OpenCode can renew its wait without a model turn.
Normal deadline wakes authorized by a healthy monitor remain unchanged.

The `hook_is_a_messenger` integration gate covers the adapters, carrier and
identity helpers. Runtime regressions additionally exercise malformed policy
fields, unavailable monitors, stale bindings and question evidence without a
viewer; a source scan alone cannot establish transitive execution ownership.
