# attending-the-same-work-is-continuous
# Attending the same work is continuous

## Problem

On 23 Oct the master was woken about a dozen times with work it had already been shown, each time while it was attending a subagent that was still working. Two routes in `crates/core/src/turn_policy.rs` release the `received` set, which makes attendance stop quieting work already shown.

1. **Re-attending.** `AttendRequest::Attend` calls `release_received()` whenever an attendance already exists (around line 997), even when the new attendance names the same task. Every `clank attend "<same name>"` therefore re-presents everything already shown.
2. **A subagent waiting on its own work.** `turn_end` treats `Qualifies::Ended` as the end of the attendance (around line 829). When a Claude Code subagent ends its turn while its own background shell job runs, the Stop payload's task snapshot shows the shell task and not the subagent. `qualifies` then reads the subagent as ended, the attendance is dropped and `received` is cleared, even though the subagent resumes when its job finishes.

A related symptom is `commit_plan_revision`, which fires while the attended subagent is itself editing the plan file.

This plan fixes route 1 only (see Child continuity). Route 2 is unresolved: a subagent that ends its turn while its own shell job runs still ends the attendance and releases `received`, so that part of the 23 Oct incident can still happen. A `commit_plan_revision` first owed during an attendance is new work and is still presented.

## Model

Attendance quiets work on the strength of one piece of background work being continuously alive. Replacing an attendance with one on the same work is not an end. A subagent with a live child task of its own is still running.

- **Re-attending the same work.** Re-attending the same resolved task (the same task id, or a name that resolves to the same task id) keeps the Standing attendance, and its `received` set, while refreshing its expectation. The request is held beside the Standing attendance until the next Stop resolves it. Until then the Standing attendance, its expectation and its check-in stay in force, so recovery, a re-evaluated parked Stop and the check-in timer all see it. Resolving to the same task id renews the expectation and starts a fresh check-in. Resolving to a different task, several tasks or none ends the attendance and releases `received`; so does clearing.
- **A subagent with a live child.** If the snapshot carries what it takes to link a child task to its parent subagent, a subagent counts as running while any of its own child tasks is live. If the snapshot has no parent field, find another sound signal from the hook payload or transcript (check what Claude Code 2.1.3xx sends). If no sound signal exists, say so and keep only fix 1. Never guess the link from timing.

## Child continuity

The Stop payload carries no link from a child task to its parent subagent, so this change keeps only fix 1.

- **Payload.** Claude Code 2.1.308 (the installed binary) declares each `background_tasks` entry as `id`, `type`, `status`, `description`, plus `command` (shell), `agent_type` (subagent), `server`/`tool` (monitor or MCP task) and `name` (workflow). Its builder for the Stop and SubagentStop input copies only those fields from the task registry. The registry does record which agent owns a `local_bash` task (`agentId`), but that reaches only the remote-session metadata (`running_background_tasks[].owner_agent_id`), never a hook. The committed fixtures agree: a subagent entry is `{id, type, status, description, agent_type}`.
- **Transcript.** A sound link does exist outside the payload. The subagent's own record, `<session>/subagents/agent-<task id>.jsonl`, names the subagent by the same id the payload lists (`agent-a25eee2eef81caf3e.jsonl` for the fixture's subagent). Each background Bash result in it carries `toolUseResult.backgroundTaskId`, which is the shell's task id. Using it would mean the hook adapter reads that file at every Stop to attach owners to shell entries. That is an adapter change this plan does not make; it would need its own captured fixture of a subagent waiting on its shell.
- **Decision.** This plan is narrowed to fix 1. The transcript is Claude Code's private, unversioned storage, not a hook contract. Parsing it on every Stop would make attendance depend on a file format nothing promises to keep. The remaining symptom, a subagent waiting on its own shell, is not fixed here. Until it is, a subagent should run long work in the foreground. Revisit if a hook payload gains an owner field.

## Acceptance

- Pure `turn_policy` tests:
  - re-attending the same task keeps `received` and sends nothing new;
  - attending different work releases it;
  - a subagent whose only live entry is its own child shell task still qualifies, if the link exists;
  - a subagent that has truly ended still releases.
- Restoring the old release-on-any-attend body must fail a test.
- The `turn_policy`, `turn_custody` and `monitor_turn` suites pass.
