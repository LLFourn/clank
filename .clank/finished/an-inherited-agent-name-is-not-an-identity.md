# an-inherited-agent-name-is-not-an-identity
# An inherited agent name is not an identity

## Problem

On 2026-10-24, in safepay's `story-short` worktree, a new Codex standby agent `painter` took over the reviewer `codex`'s binding. After that neither agent could be woken.

Mechanism, observed:
- Codex 0.164 panes launched without `--no-daemon` run their hooks and their shell tool commands inside one shared app-server daemon (pid 7016). They don't run in the pane's process tree.
- The daemon was first started from clank's own reviewer pane. Its environment carries `CLANK_AGENT=codex`, and every daemon-hosted hook and command on the machine inherits it.
- `CLANK_AGENT` outranks the session binding everywhere it is read:
  - `resolve_identity_in` (`agent_env.rs:365`), the ordinary CLI;
  - `resolve_identity_for_hook_in` (`agent_env.rs:414`), the Stop hook;
  - `stop_hook::session_started` (`stop_hook.rs:340`);
  - `grok_hook.rs:65`.
- Each fresh painter session was named `codex` at SessionStart. Each was handed the reviewer's work and wrote a verdict as codex. One of them was bound as `codex`, and the user then closed it.
- The real reviewer no longer held `codex`. painter ran `clank as painter` and was bound correctly. Its Stop hook still read `CLANK_AGENT=codex` ahead of that binding, found `codex` bound to a different session, and stayed silent. So were the reviewer's hooks.

Other repos escaped only because their one Codex agent happens to be labelled `codex`.

The violated invariant: **a session that is bound is identified by its binding**. `CLANK_AGENT` is a launch hint, for binding a session clank just started that has no binding yet. An inherited environment can't say which process it was meant for, so it can never outrank an established binding.

## Design

Scope set by LLFourn on 2026-10-24: "just run the plan to fix it to --no-daemon". This plan is the launch default only. The binding-precedence and bootstrap-authority work in the reviews of 00a9a16 and 4b3ab10 moves to a follow-up plan; see Out of scope.

Every Codex argv clank composes carries `--no-daemon` exactly once, whatever `launch.args` holds:
- fresh start (`compose_bootstrap_launch`);
- `resume` (`compose_launch`);
- `fork -C` (`compose_fork_launch`).

One function, `launch_args(tool, launch)`, builds the profile args for all three, so no composer can forget the flag. The tool, its hooks and its shell commands then run as descendants of the pane, and the `CLANK_AGENT` they see is the one `clank agent start` set for that pane. The installed Codex advertises `--no-daemon` for the root command, `resume` and `fork`; codex's review of 00a9a16 confirmed that. `--print` shows the flag. This is a launch default, like `tool_env_defaults`, so no config migration is needed.

## Acceptance

- `every_codex_launch_runs_outside_the_daemon` covers fresh, resume and fork, each with no profile and with a profile that already passes `--no-daemon`. Codex gets the flag exactly once, and Claude and OpenCode get none beyond their profile's.
- Mutation-checked: the test fails when the flag is dropped, when the de-duplication is dropped, and when the flag is given to every tool.
- The existing argv-pinning tests (bootstrap, and fork with and without a source session) are updated to expect the flag.
- README: one paragraph under "An agent's launch" says why.

## Out of scope

Follow-up plan, from codex's reviews of 00a9a16 and 4b3ab10:
- one identity reader in which an existing binding outranks `CLANK_AGENT`, for the ordinary CLI and every hook;
- an unbound session in a shared or unknown execution context must never replace another session's binding through an inherited name;
- malformed or conflicting harness evidence must not fall through to `CLANK_AGENT`.

Until that lands, a Codex session started outside clank, or one still on the old daemon, can mis-identify itself as the daemon's label.

Also out of scope:
- pane identity by command text, and the duplicate-start guard for fresh agents (a separate plan);
- repairing story-short's bindings and restarting the daemon, both manual: LLFourn relaunches that workspace.
