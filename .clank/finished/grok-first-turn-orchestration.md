# grok-first-turn-orchestration
# grok's first turn must arm the loop; doctor must show EFFECTIVE auto

A fresh grok reviewer never woke for its first review
(world-cup-goal-hazard, 2026-07-12). Two defects, both reproduced in a
scratch repo:

1. **`clank doctor` prints the raw per-agent `auto_mode=unset`** while
   `clank auto status` on the same agent resolves the effective mode
   to `on` (the `~/.clank` user-global default). The agent read
   doctor, concluded auto was off, and "fixed" the wrong thing.
2. **The default auto-on initial prompt strands grok.** `clank agent
   start` under effective auto-on launches with `"Session resumed."`,
   whose documented contract is "do NOT instruct the agent to run
   `clank wait` — the stop hook is the orchestrator". That premise is
   claude/codex-only: grok has NO clank hook (passive by design,
   grok-first-class). Grok acks the prompt, ends the turn, nothing
   nudges it, no wait is armed → commits never wake it. The arming
   discipline exists only in the grok skill, which a minimal ack turn
   doesn't reliably engage.

## Fix

- **Doctor**: the agent line reports the EFFECTIVE auto mode with
  provenance, keeping the raw field visible — e.g.
  `auto_mode=on (user default; per-agent unset)` /
  `auto_mode=off (per-agent)` — via the SAME resolver `auto status`
  uses (`resolve_effective_auto_mode`), so the two surfaces cannot
  disagree.
- **Tool-aware default prompt**: `resolve_initial_prompt` learns the
  session tool. Claude/codex keep `"Session resumed."` verbatim (their
  hook orchestrates; double-trigger stays avoided). Grok's default
  becomes an arming instruction, e.g. `"Session resumed. Arm your
  clank work loop now: run `clank wait` as a background command
  (background: true), then end your turn."` — the wake channel grok
  actually has. An explicit per-agent `initial_prompt` (including the
  empty-string opt-out) still wins, unchanged.
- Update the `agent-start-initial-prompt` design comment where it
  claims the stop hook orchestrates universally.

## Acceptance

- `resolve_initial_prompt` unit tests: grok + auto-on + unset
  declaration → the arming prompt; claude/codex unchanged
  (`assert_eq!` on the strings, as today); explicit declaration and
  `Some("")` opt-out still win for grok.
- Doctor test: an agent with unset per-agent auto under a seeded
  user-global default-on reports effective `on` with provenance (and
  a per-agent `off` reports `off (per-agent)`).
- `clank agent start grok --print` in a grok-bound fixture composes
  the arming prompt (in-process compose test, not a spawned binary).
- fmt/clippy at the 18/6 baseline; 23 suites green.
