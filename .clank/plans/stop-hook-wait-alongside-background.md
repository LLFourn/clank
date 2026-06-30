# stop-hook-wait-alongside-background

## Problem

The shipped `stop-hook-yield-on-background-work` made the claude Stop hook
yield (Silent) whenever `background_tasks` is non-empty, so Claude Code's
auto-wake can deliver the background result. But yielding means clank is
**not** watching for review work while the agent is parked on a background
process: if a review lands while the process runs, the agent isn't woken
for it — only the process completing wakes it.

We can't fix this by running the Stop hook's own blocking `clank wait`
while a process runs — that reintroduces the original bug (a blocking wait
in the hook prevents the process-completion wake).

The fix: let a **manually-launched, backgrounded `clank wait`** coexist
with the agent's other background process. Then **either** the process
finishing **or** the background `clank wait` returning work can wake claude.
The Stop hook's job becomes: detect whether such a background `clank wait`
is already running, and if not, nudge the agent to start one — instead of
running its own (blocking) wait.

## State machine (claude; auto-mode On)

Classify `background_tasks` into **W** = tasks that are `clank wait`, and
**O** = all other in-flight tasks.

| Condition | Outcome |
| --- | --- |
| **W non-empty** | **Yield (Silent).** A background `clank wait` is already armed; it covers review-work wake and any O covers process-completion wake. The Stop hook must NOT run its own wait. |
| **O non-empty, W empty** | **Hint (Continue, once).** There's a background process but nothing watching for clank work. Nudge the agent to start `clank wait` as its own `run_in_background` task, then end its turn — so both wakes are armed. Bounded to once per stop-chain (see below); otherwise Yield. |
| **both empty** | **Run `clank wait`** — genuinely idle, the existing path. |

Once the agent acts on the hint, the next Stop sees W non-empty → Yield.
When the background `clank wait` later fires and the agent handles the work,
a fresh turn ends with O-only again → it's re-nudged → re-arms the wait.
This convergence is intended.

## Why remove the `wfw` alias

Detection keys on the task's `command` being `clank wait`. The `clank wfw`
**command** alias (`main.rs:78`) is a second spelling an agent could launch,
which detection would miss. Removing it leaves one canonical command, so
the skill doc only ever teaches `clank wait` and detection stays simple.

## Spike result — nudge compliance validated

The load-bearing risk (the nudge is a model-compliance assumption) was
spiked before implementing, mirroring the prior plan's auto-wake spike. A
capture Stop hook implementing the W/O gate, driven by a real claude
session that launched `sleep 60` in the background:

```
Stop #1: bg=["sleep 60"], has_wait=false  → NUDGE (continuation)
  agent → Bash(command="clank wait", run_in_background=true)   ✓ complied
  task_started: "Wait for clank review work in background" (local_bash)
```

So the nudge reliably gets the agent to launch `clank wait` as a
`run_in_background` task, and its command is exactly `clank wait` — what
`is_clank_wait` keys on. (The temp dir wasn't a clank repo, so `clank wait`
errored instantly and didn't persist to Stop #2; in a real bound repo it
blocks and persists, and `background_tasks[].command` is the raw command —
already verified for `sleep`.) The nudge wording in `nudge_reason` matches
the spike's.

## Implementation

### 1. Remove the `wfw` command alias (prerequisite)

- `crates/cli/src/main.rs`: drop `#[command(visible_alias = "wfw")]` on
  `Wait`, and delete the `wait_command_keeps_wfw_alias` test (do **not**
  add a "wfw is rejected" test — the framework enforces absence).
- `crates/cli/src/cli/setup_assets/skill_shared_core.md`: drop the
  "Formerly `clank wfw`, which still works as an alias." sentence.
- Full-repo grep for stranded `clank wfw` / command-alias references
  (README, skills, docs) and fix the real ones.
- **Scope / keep:** the `wfw_timeout` **config-key** serde alias
  (`agent_config.rs:56`) stays — it's back-compat for on-disk configs and
  doesn't affect detection. The `--wfw-timeout` **flag** `visible_alias`
  (`mod.rs:755`) is cosmetic; reviewer's call whether to drop it for
  consistency (keep the flag's behavior either way). The
  `wfw-output-is-a-minimal-hint` design-tag strings are NOT the alias —
  leave them.

### 2. Detect a background `clank wait`

- `crates/core/src/hook_io.rs` — `BackgroundTask` gains
  `command: Option<String>` (`#[serde(default)]`; claude reports it for
  shell tasks).
- `BackgroundTask::is_clank_wait()` — tokenize `command`, strip a path
  prefix from the program word, true iff basename is `clank` and the first
  arg is `wait`. Best-effort heuristic; document that env/`bash -lc`
  wrappers aren't handled (claude reports the raw command — verified: a
  `sleep 12` task reported `command:"sleep 12"`).
- `HookInput::has_background_clank_wait()` and
  `has_non_wait_background_work()` (any live task that is NOT clank wait).
  Keep/replace `paused_for_background_work()` accordingly.

### 3. The Stop-hook decision

Add a **pure** gate in core, fully unit-testable (no repo/identity needed):

```rust
enum BgGate { Yield, NudgeIfDriving, NoBackgroundWork }

fn background_gate(tool: Tool, input: &HookInput) -> BgGate {
    if input.has_background_clank_wait() { return BgGate::Yield }
    if input.has_non_wait_background_work() {
        // Non-claude has no run_in_background auto-wake; an already-nudged
        // stop chain shouldn't nag → just yield.
        if tool != Tool::Claude || input.stop_hook_active { return BgGate::Yield }
        return BgGate::NudgeIfDriving
    }
    BgGate::NoBackgroundWork
}
```

`compute_outcome` (`stop_hook.rs`) consumes it, **preserving the
"yield before identity resolution" property** for the Yield cases:

```rust
match background_gate(tool, &input) {
    BgGate::Yield => return HookOutcome::Silent,         // before any resolution
    BgGate::NudgeIfDriving => { /* resolve auto; On → Continue(hint); Off → Silent */ }
    BgGate::NoBackgroundWork => { /* existing resolve + compute_wait_outcome path */ }
}
```

Only the nudge/idle paths resolve repo/identity/cfg (so a config Diagnostic
can only surface when we were going to engage clank anyway). The hint is a
`Continue` whose reason tells the agent: you ended your turn with a
background task running but no `clank wait` watching for review work; start
`clank wait` as its own `run_in_background` Bash task and end your turn, so
that **either** the task finishing **or** new clank work wakes you. Gate the
nudge on `Tool::Claude` (codex never has `background_tasks`, so it can't
reach this branch anyway).

## Design decisions to flag for review

- **Nudge bounded by `stop_hook_active`** (nudge once per stop-chain). Known
  edge: if the agent keeps taking turns while ignoring the hint and keeping
  a process alive, `stop_hook_active` resets on tool use and it could
  re-nudge. Accepted as low-harm for v1; raise if reviewers want a stronger
  bound.
- **Detection precision:** a missed `clank wait` → a redundant nudge (the
  agent may start a second wait, harmless); a false positive → yield with no
  real wait armed (review-work wake waits on the process). The
  basename+`wait` match is precise for the realistic command shapes.
- **`clank wait` as the only background task** (W non-empty, O empty) →
  Yield is still correct (the background wait wakes the agent).

## Tests

- core: `is_clank_wait` across `clank wait`, `/path/to/clank wait`,
  `clank wait --repo x`, non-matches (`clank status`, `sleep 12`,
  `clank waitx`); the two `HookInput` classifiers; `background_gate`
  exhaustively (W set → Yield; O-only + claude + first stop → NudgeIfDriving;
  O-only + codex → Yield; O-only + `stop_hook_active` → Yield; empty →
  NoBackgroundWork).
- cli: real-binary check — a payload with a `clank wait` background task
  yields silently; a payload with a non-wait task + claude + first stop
  emits a continuation hint; an empty payload proceeds to the wait path.

## Acceptance criteria

- `clank wfw` no longer parses; `clank wait` unchanged; skill doc + docs
  swept; `wfw_timeout` config alias still loads old configs.
- The Stop hook never runs its own blocking wait while a non-wait
  background task is live; it yields on a background `clank wait`, and nudges
  (once) when a process runs with no background wait.
- `background_gate` is pure and unit-tested; clippy at baseline; git
  boundary untouched.

## Deploy

After FINISHED: `cargo install --path crates/cli --force`, then
`clank setup` to refresh the skill docs (drops the `wfw` mention).
