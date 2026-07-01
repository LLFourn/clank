# stop-hook-work-aware-hint

## Problem

The shipped `stop-hook-wait-alongside-background` nudges the agent to run
`clank wait` whenever a non-wait background task is live and
`!stop_hook_active`. In practice this **loops**: the master launches a
background process while it's still its turn (gate = "continue"), the hook
nudges, the agent runs `clank wait` — which **returns immediately** because
the master always has standing "continue" work — so it never persists as a
watching task, and (worse) running `clank wait` is a *tool use* that
**resets `stop_hook_active`**, so the next Stop re-nudges. The loop is
literally caused by the agent complying.

Two root errors:

1. **Wrong discriminator.** The hook keyed on "is there a non-wait
   background task"; the real question is **"does the agent have immediate
   clank work right now (is it still its turn)?"**
2. **`stop_hook_active` is not a usable bound** — it resets on any tool
   use, so it can't gate a nudge whose whole point is to make the agent do
   a tool use.

## Model (the 2×2)

At turn-end, two independent dimensions — immediate clank work? × a
background task running? — give:

| | no bg task | **bg task running** |
| --- | --- | --- |
| **has immediate work (its turn)** | deliver work / continue (normal) | **yield silently** — blocked on its own task; don't nudge (this was the loop) |
| **no immediate work (idle / just committed)** | run `clank wait` (block for work) | **yield + hint** to start `clank wait` alongside the process |

The discriminator is **immediate-work**, not task-presence. "Master
committed → now waiting for reviews → no immediate work → hint" and "still
the master's turn → yield silently" are exactly the two cells.

Why this is loop-free by construction: the hint fires **only when the agent
has no immediate work**, i.e. exactly when `clank wait` would *block*. So a
nudged `clank wait` persists as a watching task; the next Stop detects it
(`has_background_clank_wait`) and yields. No `stop_hook_active` needed.

## Spike evidence (already validated)

The closing-the-loop question — "after being woken by a bg task and doing a
commit, does the Stop hook run again so it can deliver the Case-A hint?" —
was verified with a PTY spike: an agent launched two bg tasks, yielded, was
auto-woken when the first finished, ran a foreground command (the
"commit"), ended its turn, and **the Stop hook fired again**, correctly
showing the *other* task still running:

```
STOP bg=["sleep 5","sleep 40"]     ← launched both, ended turn (Case B: yield)
STOP bg=["sleep 40"]               ← woke on sleep 5, ran `echo committed`, ended turn
```

So the hook re-runs after a woken-turn commit and sees the still-live
process → it will deliver the Case-A hint at that point. The chain is
closed.

## Implementation

### 1. Non-blocking work peek (`clank wait --peek`)

`clank wait` already computes work once before its poll loop (wait.rs
≈196–234). Add a `--peek` flag that runs that one-shot computation and
returns **immediately** — items (exit 0) if the agent has work now, empty
otherwise — never entering the watcher loop. Reuses the exact
`derive_status` → `work_for` path and the `--json` envelope the stop hook
already parses. (Alternative if cleaner in review: compute `WorkStatus` +
`is_actionable` directly in the hook — but `--peek` keeps all git access in
the existing `wait` command per the git-boundary rule and avoids
duplicating the work construction.)

### 2. Reshape the gate (`crates/core/src/hook_io.rs`)

Replace `BgGate`'s `stop_hook_active`-based nudge with a disposition that
defers the work question to the cli:

```rust
pub enum BgDisposition {
    YieldArmed,       // a clank wait is already backgrounded, OR bg task on
                      // non-claude → yield, no hint
    NeedsWorkCheck,   // claude, non-wait bg task, no clank wait → cli peeks
                      // for immediate work: work → yield, none → hint
    NoBackgroundWork, // proceed to the normal wait/deliver path
}

pub fn background_disposition(tool: Tool, input: &HookInput) -> BgDisposition
```

- `has_background_clank_wait()` → `YieldArmed`.
- non-wait bg present, `tool == Claude` → `NeedsWorkCheck`.
- non-wait bg present, `tool != Claude` → `YieldArmed`.
- else → `NoBackgroundWork`.

Drop `stop_hook_active` from the decision entirely. Keep `is_clank_wait` /
`has_background_clank_wait` (still needed to avoid re-hinting once armed).

### 3. Wire it (`crates/cli/src/cli/stop_hook.rs`)

```
match background_disposition(tool, &input) {
    YieldArmed       => Silent,                       // before resolution
    NoBackgroundWork => <existing resolve + compute_wait_outcome>,
    NeedsWorkCheck   => resolve identity/auto; Off → Silent;
                        On → peek: work present → Silent (Case B),
                                   no work      → Continue(nudge_reason) (Case A)
}
```

Keep `nudge_reason` (spike-validated wording). The `YieldArmed` cases still
return before identity resolution.

## Codex: unaffected (explicit)

Codex has no `background_tasks`, so it is **always** `NoBackgroundWork` →
the existing deliver-work / block-wait path, untouched. `NeedsWorkCheck`
(the only new branch, and the only caller of `--peek`) is gated on
`tool == Claude`, so codex never reaches it, never peeks, and its Stop wire
form (exit 0 + `{decision:"block"}`) is unchanged. A test pins codex + a
(hypothetical) bg task → `YieldArmed` (never a hint).

## Known minor friction (accepted)

When a `clank wait` fires and exits (delivered work) while a long process is
still running, the next commit lands back in Case A → re-hint → re-arm.
That's one forced turn per work-cycle to re-arm, not a tight loop (gated by
real review cycles). Acceptable; the alternative (never re-hint) risks the
wait not being re-armed.

## Tests

- core: `background_disposition` — clank-wait present → YieldArmed; non-wait
  bg + claude → NeedsWorkCheck; non-wait bg + codex → YieldArmed; no bg →
  NoBackgroundWork. (No `stop_hook_active` dependence.)
- cli: `--peek` returns immediately with work / empty and never blocks.
- cli real-binary: bg + a bound repo where work exists → Silent (Case B);
  bg + no work → Continue(hint) (Case A); clank wait present → Silent;
  codex bg → Silent.

## Acceptance criteria

- No nudge loop: an agent that has immediate work + a bg task yields
  silently; the hint fires only when the agent has no immediate work.
- Once a background `clank wait` is armed, no re-hint.
- Codex behavior byte-for-byte unchanged (no bg tasks → normal path; never
  peeks; wire form intact).
- `--peek` never blocks; `background_disposition` unit-tested; clippy at
  baseline; git boundary untouched.

## Deploy

After FINISHED: `cargo install --path crates/cli --force` (skill docs
unchanged this time — no `clank setup` needed).
