# one-state-file-per-agent

Invariants: I3, I6. After `block-answers-are-best-effort`.

## Why

An agent's directory holds up to about 12 state files, and several hold copies of the same facts, which are then reconciled:

- **The session binding is stored four times:** `config.json`, `turn.json` (`binding`), `state.json` (`session`) and `delivered.json` (`session`).
- **`state.json` repeats role and session** from the roster and the binding. It is a published copy of `turn.json`'s working state.
- **"What reached the agent" is stored several ways:** `delivered.json` receipts, `turn.json` `received`, and presentation records.
- **`recovery-issued.json` holds the highest nudge attempt number,** which `nudges.jsonl` already records.
- **`question.json` is state** but lives in its own file with its own lock.
- **There are dead files nothing reads:** per-agent `blocks*` and `unblocks/` (gone after `blocks-have-no-migration`), `repeat.json`, `repeat.lock` and `attending.holder`.

The user's direction: `config.json` holds only configuration; runtime state lives in `state.json`; no receipts.

## Build

1. **The per-agent layout separates configuration, control state and history.**
   - `config.json`: only what the user sets (auto mode, launch).
   - `state.json`: all authoritative runtime state, under one lock. It holds:
     - the binding, the only copy of the session;
     - the monitor's turn record (today's `turn.json`), including its run-ended notices (I1);
     - the open questions (today's `question.json`);
     - **recovery control state:** the current episode (today's `recovery.json`) and the attempt-number high-water mark (today's `recovery-issued.json`), which recovery claims durably *before* dispatch (`recovery.rs:3010-3024`);
     - the projection the TUI reads.

     Nothing in it repeats the roster.
   - `nudges.jsonl`: history only, best effort, written after an outcome. Nothing is allocated from it.
   - `events/`: every inbox source's log and acks, GitHub, command, timer and message alike.
   - `feedback/`: reviewers' verdicts.
2. **One transaction boundary for the shared document.**
   - **Each writer reloads and patches.** Every writer of `state.json` (binding hooks, question updates, the monitor's turn and recovery persistence, the TUI projection) takes the one lock, reloads the document, applies a typed patch to its own fields only, and writes it back atomically. No writer persists a cached whole document, so a stale in-memory copy can never restore an old binding or erase a question.
   - **Session-scoped fields are validated in the transaction.** A patch to the turn record, questions, recovery episode or run-ended notices names the session it belongs to and applies only if that session still holds the label, checked inside the transaction.
   - **A rebind is one transaction.** It sets the binding and resets every session-owned field together. The attempt-number high-water mark is never reset.
   - **The serve loop never waits.** On the monitor's serve loop, the state lock is taken with the non-blocking acquire and handled as contention, with the release-driven retry (`Releases`), as for the binding lock today (`agent_store.rs:540-558`, I9). Off the loop, writers keep waiting.
   - **Hooks stay narrow.** Hooks still read only the binding projection (I4); the `hook_reach` audited readers move to the new location (step 7).
3. **Delivery semantics (I3).** Every inbox event, whether from GitHub, a command, a timer or a message, keeps at-least-once delivery until acked: removing presentation receipts never removes pending inbox work. What becomes best effort is only the per-session record of which non-inbox items a session was shown.
4. Delete `turn.json`, `question.json`, `recovery.json`, `recovery-issued.json`, `binding.lock` and `question.lock` once their contents live in `state.json`. Delete `delivered.json` per step 5. This is a breaking change with no migration (step 6).
5. **Before deleting receipts, establish what reads them.** `delivered.json` is read by the monitor (`presentation::delivered_to`, in `gather`), and the turn record keeps `received` and `seen`. State what each answers today, and either show it is no longer needed under I3 (best effort, no per-session record) or keep it inside `state.json` as the monitor's working state. That would not be a receipt replayed across sessions, and it is reset on rebind. Do not silently change what a wake presents.
6. **The upgrade consequence.** The binding leaves `config.json` with no migration, so every live session is unbound after the upgrade, and its hooks go silent as stale. That is acceptable only because protocol 25 already requires restarting agents on the old build. Say so in the CHANGELOG's Breaking entry: restart every agent after upgrading. A pane `clank open` launched rebinds at session start.
7. **The hook gate moves with the binding.** `hook_reach`'s audited projections (`identity::tool`, `binding_in`, `identity::mutate`) read the session from `config.json` today. Move each to the new location, keep the projection audit (only the binding fields), and keep the gate green.
8. **Registry:** I6 (one copy of each fact) and I3 (no per-session delivery record) are updated to `holds` if this plan makes them true, naming the tests. If anything remains, keep them `target`, naming it.

## Acceptance

- **One copy:** each fact listed in step 1 exists in exactly one file. A fresh agent has exactly the listed file set, and its session appears once.
- **Attempt numbers:** `an_unwritable_history_never_recycles_an_identity` and `a_restart_never_reuses_an_attempts_identity` still pass, against the high-water mark in `state.json`. A test with `nudges.jsonl` unwritable or torn still never reuses an attempt number.
- **Inbox survives a rebind:** a command event, a timer event and a message, each unacked, are still owed and presented after a rebind, and each acks.
- **A stale write cannot win.** A monitor holding a pre-rebind copy persists its turn record after the rebind. The new binding stands, the session-owned fields stay reset, and the stale patch is refused.
- **Independent fields:** a question update and a monitor turn persist interleaved through a seam. Both survive, and neither overwrites the other.
- **The loop stays responsive:** with the state lock held by another thread (through the test-only registry, no real fd), the serve loop serves another agent and retires before release, and the release wakes it.
- The hook gate stays green, its audited projections reading only binding fields from `state.json`.
