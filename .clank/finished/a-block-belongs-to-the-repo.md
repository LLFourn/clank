# a-block-belongs-to-the-repo
# A block belongs to the repository, not an agent

## Why

Lloyd: blocks are repo-wide questions for the human, almost always raised by
the master. Reviewers should return REQUEST_CHANGES; the master decides whether
human input is needed. Requiring an agent name to answer a block contradicts
that model. The asking agent is attribution, not an owner or recipient.

Today `block answer <agent> <name>` and per-agent blocks/unblocks storage,
answer delivery and receipts encode agent ownership. Correct the model as well
as the command spelling; do not merely infer the original author behind the CLI.

## Contract

- `clank block ask <name> -m "question"` creates a repository-owned question.
  `clank block answer <name> -m "answer"` answers it without an agent argument
  or session binding. Retain `--repo` for selecting the repository.
- Remove agent ownership selectors from the public blocking CLI, including
  ask's `--author`. Derive optional asking-agent attribution from the session;
  attribution must not be required for identification, answering or cleanup.
  Retire the old positional answer syntax with a useful error; do not guess
  which argument is an agent or retain a hidden agent-owned alternate path.
- Give blocks a repository-wide name namespace and durable occurrence identity.
  Reusing a name must not let old answers, receipts or delayed writes resolve a
  newer question. Concurrent creation or answering must not overwrite custody.
- Move canonical questions, answers, delivery state and cleanup to repository
  scope. Remove asking-agent ownership from core types, wake items, storage
  keys and APIs wherever it controls block identity or resolution. Retain
  author attribution and actual delivery-attempt evidence only where useful.
- A pending block pauses the repository's review cycle for all plans and agents,
  while unrelated events/messages remain deliverable. Reviewers use
  REQUEST_CHANGES rather than opening human questions in the normal workflow;
  align master/reviewer instructions and command guidance with that division.
- Human answers are repository decisions, readable by the team. Responsibility
  for receiving and acting on an answer follows the current master, including
  after the asking agent is removed. With no master, retain it until a master
  exists. Handover before or during delivery must not lose an answer or let an
  obsolete delivery receipt suppress what the new master still needs to see.
  Already received answers remain inspectable by a successor until explicit
  repository-level cleanup; do not replay all historical answers on every turn.
- Cleanup is repo-scoped and removes only safely settled state. It must not
  remove unanswered questions, owed answers or unresolved delivery attempts.

## Permission blocks and delivery

Keep the existing permission-block distinction and human-message requirement.
Removing agent ownership must not turn an ordinary stored answer or a hook wake
into permission approval. Resolve new `--nudge` attempts to the current master
without an agent argument. Retain the actual session/pane and occurrence as
attempt evidence so a role change cannot falsely confirm delivery to a successor.
Preserve unknown-outcome, explicit resend and `--delivered` semantics; no automatic
resend or false completion. If there is no eligible master/session, retain the
question and explain why delivery cannot proceed. Human-confirmed delivery to
an old session is not proof of approval delivery to a new session.

## Migration and integration

- Migrate legacy per-agent questions, answers, receipts and outstanding attempts,
  including those under removed agents, without discarding pending state or
  replaying settled actions. Handle legacy same-name questions explicitly:
  preserve each under deterministic unique repo names and report the mapping;
  never merge distinct questions or require agent naming to operate afterward.
- Migration is restart-safe, serialized with writers and does not leave duplicate
  active custody. Corrupt/unknown records remain visible and unresolved. Account
  for already-running processes during upgrade using existing version/retirement
  mechanisms rather than letting old and new layouts accept competing writes.
- Update monitor/wait gating and delivery, CLI, TUI and web answer/cleanup paths,
  generated reference, README/book, embedded skills and any recovery hints.
  All surfaces use the same repository block identity and lifecycle.

## Validation

Use direct Rust tests and shared fixtures, minimizing subprocesses. Cover parsing
without agent selectors; repo-wide suppression; separate concurrent questions;
name reuse and stale receipts; answers from an unbound human shell; master
handover before/after answer and during delivery; no-master recovery; removed
askers; safe cleanup; legacy duplicate names and interrupted migration; corrupt
custody; and permission nudge unknown-outcome/resend/confirmation across handover.
Exercise CLI/TUI/web resolution through shared handlers so no surface retains
agent-owned behavior. No Python, nested binary builds or new polling loops.

Implement as one complete plan. This changes block ownership and its dependent
surfaces, not the general event model or the review verdict system.
