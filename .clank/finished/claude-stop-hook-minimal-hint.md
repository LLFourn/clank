# claude-stop-hook-minimal-hint

Experiment: on claude, the stop hook never waits and never delivers work.
Its only continuation is one minimal hint — "arm `clank wait` as a
background task" — and all work delivery moves to the armed wait
completing (claude wakes the session on background-task completion).
Codex is untouched: it has no task-completion wake channel, so the
in-hook long-poll + block-with-items model stays.

## Problem

Today, an idle claude session with auto on parks INSIDE the stop hook:
`compute_wait_outcome` spawns `clank wait --timeout 0` (indefinite) and
the session never reaches a genuine stopped state. Consequences:

- Anything that needs real stops can't coexist: /loop, `ScheduleWakeup`,
  scheduled crons — the harness sees a Stop hook that runs forever.
- Work is delivered over TWO channels (hook block-with-items AND armed
  background-wait wake), and the `BgDisposition` peek machinery exists
  to arbitrate between them. The BusyOwnWork loop concern documented at
  `stop_hook.rs:289` is a bug class born from having two channels.

A Stop hook's only model-visible channel is the blocking continuation
(exit 2) — a "hint" on exit 0 goes to the transcript, not the model. So
"never block" literally would leave idle sessions permanently deaf. The
design instead makes the one block **minimal, constant, and
self-extinguishing**: it only teaches where to get work, never what the
work is, and following it makes the next stop silent.

## Design — claude decision table (auto on)

1. **Armed live `clank wait` in `background_tasks`** → Silent
   (`YieldArmed`, unchanged).
2. **Live non-wait background task, no armed wait** → peek (unchanged
   mechanism): has work right now → Silent (`BusyOwnWork` — the model is
   mid-task; its own task completion wakes it and the work keeps); no
   work → block with the hint (park a watcher alongside). This preserves
   the dark-skippy fix exactly.
3. **No background work** → block with the hint. NEVER run `clank wait`
   inside the hook, NEVER render work items. This is the behavioral
   change: today this branch long-polls indefinitely and then blocks
   with rendered items.

The hint (adapt `nudge_reason`, one function, two preambles):

- with live bg tasks: current wording, unchanged.
- genuinely idle: "Nothing is watching for clank work. Start
  `clank wait` as its OWN background task now — call the Bash tool with
  command `clank wait` and run_in_background set to true — then end
  your turn. It will wake you when there is clank work."

Self-extinguishing: the block fires only in the un-armed state and its
instruction removes the condition. If work exists right now, the armed
wait exits immediately and the task-completion wake delivers the items
— the model acts on `clank wait`'s own output (which is already the
minimal per-item hint form) instead of a hook-rendered copy of it.

## Consequences

- `render_wait_items` + the `WaitItem`/`WaitEnvelope` mirror in
  stop_hook.rs become codex-only (claude never renders items). Keep
  them; do not fork them.
- `compute_wait_outcome` (in-hook wait) becomes codex-only.
- `wait_timeout` agent config: now only meaningful on codex. Document
  that on the field; do not remove it.
- Decision trace: new/renamed silent+continue branch names so
  `stop-hook.json` still says exactly which branch fired.
- Skill docs (`setup_assets/skill_*.md`): teach the steady state — end
  turns with a backgrounded `clank wait` armed; acting on a wake means
  re-arming before ending the turn. The hook hint is the safety net,
  not the primary mechanism.
- Master auto-drive costs up to two extra hops per step (hint turn +
  wake turn) when the session doesn't re-arm on its own. Accepted —
  this is the experiment's main cost to evaluate.

## Risks (what dogfooding must watch)

- Single wake-channel bet: if claude ever fails to wake on background
  task completion, work stalls until the next genuine stop re-hints.
  Self-healing but latent. Watch for stalled reviewers.
- Hint compliance: if a session ignores the hint and stops without
  arming, claude's own block cap bounds the re-fires; the session goes
  idle with work pending until something wakes it. Watch for it.
- No feature flag: claude behavior changes unconditionally. Rollback is
  reverting the plan commit.

## Tests

In-process cores only (no binary spawning). Update
`stop_hook_integration.rs` claude paths:

- claude + auto on + idle + no bg work → Continue whose reason contains
  the arm-the-wait instruction and NO work-item lines, even when work
  exists right now.
- claude + armed wait → Silent (existing, unchanged).
- claude + live bg task: BusyOwnWork/idle-nudge cases (existing,
  wording assert updated).
- codex + auto on + idle → unchanged: in-hook wait, block with rendered
  items (pin that codex still renders items so the claude change can't
  silently leak).

## Acceptance

- On claude, no stop-hook invocation ever spawns `clank wait` without
  `--peek`; grep the claude code path.
- The hook's continuation text on claude never contains a work item
  (no plan names, no shas) — only the constant hint.
- Codex behavior byte-identical (existing codex tests untouched and
  green).
- Decision trace records the new branch names.
