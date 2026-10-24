# a-binding-outranks-an-inherited-name
# A binding outranks an inherited name

Follow-up to an-inherited-agent-name-is-not-an-identity, which only made clank launch Codex with `--no-daemon`.

Problem: `CLANK_AGENT` outranks the session binding wherever it is read: `resolve_identity_in` (`agent_env.rs:365`), `resolve_identity_for_hook_in` (`agent_env.rs:414`), `stop_hook::session_started` (`stop_hook.rs:340`) and `grok_hook.rs:65`. A Codex session in the shared app-server daemon inherits the `CLANK_AGENT` of whichever pane started the daemon. That happens with Codex started outside clank, or still on an old daemon. Such a session reads as that pane's agent, and at SessionStart `bind_session_to_agent` replaces that agent's binding unconditionally. On 2026-10-24 this took story-short's `codex` binding away from its reviewer.

Contract, from codex's reviews of 00a9a16 and 4b3ab10:
- One identity reader, used by the ordinary CLI and every hook. An existing binding for the invoking session identifies it. An inherited name never overrides that binding or replaces it.
- A known but unbound harness session doesn't become an identity just because `CLANK_AGENT` is set. It needs a supported SessionStart bootstrap or an explicit `clank as`.
- SessionStart may use the launch hint only in an execution context that establishes it as the intended launch. For Codex, use the existing hook_owner/proc_identity parent inspection to exclude the shared app-server and unknown ownership. No new token, handshake or pid scheme.
- Malformed or conflicting harness evidence, such as `BothToolsDetected` or an invalid id, is an identity failure. It never falls through to `CLANK_AGENT`. Explicit flags stay explicit. The no-session convenience outside a harness stays.

Acceptance: `codex` is bound to R and `painter` to P, and an unbound U runs in a shared daemon context with `CLANK_AGENT=codex` inherited. SessionStart for U leaves both bindings unchanged and delivers neither agent's work, and U's implicit CLI identity is refused. A malformed-session fixture likewise doesn't acquire `codex`. These existing behaviours are kept:
- bound-session resolution;
- standalone fresh, resume and fork launches;
- explicit overrides;
- `/clear` rebind.
