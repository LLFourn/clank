# declared-work-ends-by-the-harness-listing
# A declared attendance ends when the harness's own task listing no longer has it; clank still never attends work nobody declared

## Why

all-background-work-is-attended (PR #25) removed two things at once:

1. **Non-explicit attendance.** clank inferred work the agent never declared, and woke it with "You have live background work you are not attending".
2. **The precise end of declared work.** Claude's and Grok's Stop hooks send `background_tasks`, which lists the harness's own live subagents, shells and monitors. That is how clank knew a declared subagent was still running, and when it was gone.

LLFourn wanted only the first gone: "No we should be reading that list. That's how we detect subagents are gone through the stop hook. I didn't mean we want less precise detection what I wanted was to remove all non-explicit attendance."

Losing the second forced a guess. A declaration now ends at the agent's next turn start, whatever started it. So every message from the person clears the master's declarations, and the owed plan re-wakes it unless it re-declares. LLFourn hit this on 2026-11-10, chatting with the master while two subagents ran. It's a regression from before PR #25.

## Build

1. **Read the listing again, only to resolve and end declarations.**
   - Restore reading `background_tasks` from Claude's Stop payload and Grok's `backgroundTasks` (the readers `hook_io` and `grok_hook` had before PR #25).
   - In `turn_policy`, a `clank attend "<name>"` declaration on a tool that sends a listing:
     - is resolved against the listing (the name matches a live task's description);
     - holds while that task is listed;
     - ends when a known listing no longer has it.
   - That's the pre-#25 behaviour for declared work. Restore what's needed from the removed code (`git show bc292662^:crates/core/src/turn_policy.rs` and the files beside it), keeping only the declared-work path.
2. **Never infer undeclared work.**
   - The listing never creates attendance, never raises a notice about undeclared work, and never wakes the agent because something it didn't declare is running.
   - The `Uncovered` notice and demand stay deleted.
   - A listed task nobody declared is ignored.
3. **Tools without a listing** (Codex, OpenCode): a declaration lasts until the agent clears it (`clank attend --clear "<name>"`) or its `--expect` deadline (one check-in).
   - It doesn't end at the next turn start. That rule goes away for every tool: the person's messages never end declarations.
   - The skill tells agents on these tools to `--clear` when their harness reports the work finished.
4. **Matching problems are reported to the agent who declared them:**
   - a declaration that never matches a listed task within a turn end or two;
   - a name that matches several tasks.

   That's feedback on an explicit request, not detection. Keep only the messages that serve this, and say which pre-#25 notices come back and why.
5. **Unchanged:** `clank run` keeps its exec model, its PID-and-birth ending and its durable `run_ended`.
6. **Skill.** Update `skill_shared_core.md` per tool:
   - Claude and Grok: a declaration ends when the harness stops listing the work.
   - Codex and OpenCode: clear it yourself, or it checks in at `--expect`.

   Drop the "re-declare after every wake" instruction.

## Tests

- **Claude-shaped listing:**
  - A declared subagent holds across a turn started by the person's message (a prompt with no delivery), while still listed.
  - It ends when a listing no longer has it.
- **Undeclared tasks:** a listed task nobody declared produces no attendance, notice or wake.
- **Codex** (no listing): a declaration survives several turns, ends at `--clear`, and gets one check-in at `--expect`.
- **Grok:** the waiter verdict from PR #25's registered-waiter evidence is unchanged. Grok's listing is used only for declared work.
- **Mutation checks:**
  - ending declarations at turn start again fails the message-turn test;
  - letting an undeclared listed task attend fails the no-inference test.

## Done when

The master can be talked to while its declared subagents run without being re-woken, and declared subagents end when the harness stops listing them. clank never attends, notifies about or wakes for work an agent didn't declare.

## As landed

- **Readers restored.** Claude's `background_tasks` (`hook_io::BackgroundTask`, with the three Stop fixtures) and Grok's `backgroundTasks` are read again into a `TaskSnapshot`, which is `Known` or `Unavailable`. A missing listing never counts as an empty one.
- **Resolving declarations.** `resolve_declarations` runs on each TurnEnd and on Grok's native Stop, scoped to the session:
  - a pending declaration with one live match binds to that task's stable id;
  - a bound declaration ends when a known listing no longer has that id, and is never re-bound to another task with the same description;
  - several matches give `Ambiguous`;
  - two unmatched listings give `NoMatch`;
  - a single match with no id gives `Untrackable`, dropped at once.

  An undeclared listed task is ignored. `Uncovered`, `ShellNamed`, `completion_wake` and all automatic attendance stay deleted.
- **No turn-start expiry,** for any tool. Codex and OpenCode declarations end only on `--clear`, with one check-in at `--expect`.
- **Feedback.** Declaration feedback is held in `SessionState.feedback` (persisted) and makes its own wake only when `may_wake_without_work` allows. That's `wake_gate` (asking, blocked, auto off) plus a pending person's command, and `release_for_user_command` runs first. It joins any Deliver or Notify that wakes anyway, and is delivered once.
- **Unchanged:** Grok's waiter verdict and the `clank run` ending.
- **Skills,** per tool:
  - Claude and Grok: "ends when your harness stops listing that work".
  - Codex and OpenCode: "clear it with `clank attend --clear`".

  The "re-declare" line is removed, and the Codex reviewer skill's size ceiling is 6,175 bytes.
- **Old state.** Old `turn.json` bytes load: pre-#25 records keep their resolved task id, and #25-era declarations load as pending.
