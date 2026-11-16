# codex-wakes-when-its-declared-work-ends

Invariants: I1, I3, I5.

## Why

Codex runs clank's Stop hook synchronously, so a parked Codex agent's turn is held open. `docs/codex-tui-background.md` measured that while it is parked, Codex never continues the agent when a background command exits: nothing can reach it but clank. `a-run-ending-writes-nothing` removed clank's run-ended wake on the premise that "your harness tells you when a run ends". That holds for:

- Claude: its Stop hook runs in `asyncRewake` mode, so the turn really ends.
- Grok: its Stop lists `backgroundTasks`.

It is false for Codex. A Codex agent that parks on a `clank run` is never told it finished.

Codex's Stop payload lists no background work. The only general detection is a private app-server control socket, which the no-new-sockets gate forbids. So for Codex, only declared work can wake it. `clank run` registers an attendance, so this is one mechanism, attendance ending.

## Build

1. A per-tool fact: whether the harness itself reports background completion to a parked agent.
   - Claude: yes (asyncRewake, `background_tasks`).
   - Grok: yes (`backgroundTasks`).
   - Codex: no.
   - OpenCode: decide from its plugin's behaviour and record why.
2. When a `clank run` ends (the process it owns is gone) for an agent whose tool does not report completion, the monitor owes that agent's binding one wake item.
   - **Content:** it names the run and says it ended. Clank does not know its exit status: `run.rs` execs the command and leaves output and status with the launching harness. So the item asks the agent to collect the result where it started the run, and claims no exit code or success.
   - **Holding:** the owed item is kept in the monitor's memory, tied to the concrete binding (tool and session) that registered the run. It is delivered at that binding's next park or eligible hook boundary. So a run that ends while the agent is working, or just before its Stop registers, is delivered at that Stop, not lost.
   - **Rebind:** if the label is rebound to another session first, the item is dropped, never delivered to the replacement.
   - **Best effort (I3):** no durable record, no inbox entry, no timeline row, no receipt. Accepted limitation: a monitor restart, or a delivery whose reply is lost, loses the item. The run's own attendance is already retired, so nothing re-wakes for it.
   - Tools that report completion get nothing, as now.
3. A bare `clank attend "<name>"` has no process for clank to watch. On a tool that does not report completion it stands until cleared, and its `--expect` check-in is what wakes the agent. Its behaviour is unchanged.
4. Docs say plainly what is true per tool and per kind of declaration. Process-backed runs get a completion wake; bare declarations stand until cleared and get expected-time check-ins:
   - `setup_assets/skill_shared_core.md:25-26`, `README.md:947`, `clank run --help` (`command.rs:393`), the book, and the comments at `turn_policy.rs:743` and `:1880`.
   - The Codex skill text: "Background commands you start natively do not wake you while you are parked. Start long work with `clank run` to be woken when it ends, then collect its result where you started it. A `clank attend` declaration does not end on its own: clear it, and it checks in at `--expect`."
5. Delete `docs/codex-followups.md`. In `docs/codex-tui-background.md`:
   - Keep the measurements.
   - Remove the recommendations for native command detection and the private-server launch.
   - Note that the socket route is closed by the remote gate.
6. Narrow I1 in `docs/invariants.md` to what holds, and mark it `holds`, naming the tests below:
   - a harness that lists in-flight work is never parked over it;
   - on one that cannot, a process-backed `clank run` ending wakes its agent's binding;
   - a bare declaration stands until cleared and checks in.

   Remove this plan from I1's `Plans`.

## Acceptance

- Deterministic monitor-level tests for a Codex agent. In each case the item claims no exit status.
  - The run exits while it is parked: exactly one Deliver carrying the item.
  - The run exits while it is working, or before its Stop registers: the item is delivered at that Stop.
  - The run exits, then the label is rebound to a new session: the new session never receives it.
- The same setup for a Claude agent gets nothing.
- Mutation: removing the tool fact (treating Codex like Claude) fails the Codex test.
- No text anywhere still claims every harness reports run completion. Sweep the belief, not the wording.
