# stop-hook-yield-on-background-work

## Problem

On **claude**, when an agent ends a turn with a `run_in_background` task
(or subagent / MCP monitor) still in flight, our Stop hook immediately
long-polls `clank wait`. That blocks the very session Claude Code is
about to **auto-wake** when the background task completes — so the agent
never gets to process the background result, and the wait sits holding
the turn. The agent appears stuck even though it has live background work.

## Fix

Claude Code's Stop-hook stdin carries a `background_tasks` array (added
v2.1.145+) for exactly this purpose — the docs describe it as letting
hooks tell *"session is done"* apart from *"session is paused waiting for
background work to wake it back up."* It lists **only in-flight tasks**
and is `[]` when the session is genuinely idle.

The stop hook should **yield** (emit `Silent`, exit 0) whenever
`background_tasks` is non-empty, instead of running the wait. Claude Code
re-fires `Stop` once the task finishes — and that next firing, with an
empty `background_tasks`, runs the real wait. No new wake machinery on
our side; we just stop stepping on Claude's.

## Spike evidence (already validated, see conversation)

- Captured a live claude Stop payload: a running task shows
  `background_tasks:[{id, type:"shell", status:"running", description,
  command}]`; an idle turn shows `[]`.
- Interactive PTY test with a yield-on-nonempty hook: Stop fired at t=0
  with one running task → hook yielded → **~8s later the session
  auto-woke on its own**, firing Stop again with `background_tasks:[]`.
  The full desired chain works.
- Real built `clank stop-hook` against the two payloads: yields silently
  on the running-task payload (short-circuiting before identity
  resolution); proceeds to the normal wait path on the empty payload.

## Implementation

**`crates/core/src/hook_io.rs`**

- Add to `HookInput`:
  ```rust
  #[serde(default)]
  pub background_tasks: Vec<BackgroundTask>,
  ```
- New type (only the fields we reason about; forward-compatible):
  ```rust
  #[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
  pub struct BackgroundTask {
      #[serde(default)] pub id: Option<String>,
      #[serde(default)] pub status: Option<String>,
  }
  ```
- Predicate:
  ```rust
  impl HookInput {
      /// True when the turn ended only because the agent is still
      /// waiting on its own background work (Claude lists only in-flight
      /// tasks here and re-fires Stop once they finish). The hook yields
      /// on this; the real wait runs on the next, idle Stop.
      pub fn paused_for_background_work(&self) -> bool {
          !self.background_tasks.is_empty()
      }
  }
  ```
- Re-export `BackgroundTask` from `crates/core/src/lib.rs`.

**`crates/cli/src/cli/stop_hook.rs`** — at the very top of
`compute_outcome`, right after parsing `input`, before any repo /
identity / auto-mode resolution:
```rust
if input.paused_for_background_work() {
    return HookOutcome::Silent;
}
```
Placed first because "paused for background work" is a property of the
**turn**, independent of clank identity/auto-mode — and short-circuiting
avoids surfacing a config Diagnostic mid-background-run. (For
`auto_mode=off` the outcome is `Silent` either way, so the early return
changes nothing there.)

## Codex: intentionally exempt (no change)

Codex's Stop payload has **no** `background_tasks` field, and codex shell
commands are **synchronous within the turn** — the turn doesn't end until
the command finishes, so the Stop hook only fires when the agent is
genuinely idle. The claude failure mode can't occur there. With the field
absent, `background_tasks` defaults to empty → `paused_for_background_work()`
is always false on codex → zero behavior change. A unit test pins the
codex payload shape so this stays true.

## Design rationale

Trust Claude's contract: non-empty ⇒ live. **No status/type
whitelisting** — re-deriving liveness ourselves would be a second source
of truth (and a hang risk if we guessed Claude's status vocabulary
wrong). Yielding on *any* live task (shell, subagent, monitor) is correct:
they all wake the session.

## Known limitation (accepted)

A **permanently-running** background task (e.g. an unbounded `Monitor`)
keeps the session in yield indefinitely, so clank auto-mode can't drive
it until that task ends. Periodic crons live in the separate
`session_crons` field and are *not* affected. Deliberately not mitigating
now (YAGNI); if it bites, add a max-consecutive-yield count or a
task-type filter.

## Acceptance criteria

- `HookInput` parses `background_tasks`; `paused_for_background_work()`
  is true iff the array is non-empty.
- `clank stop-hook --tool claude` with a running-task payload yields
  (exit 0, no output) and does **not** spawn `clank wait`; with an empty
  payload it proceeds to the existing wait path.
- Codex unaffected: missing-field payload → never yields (covered by a
  test that uses the real codex payload shape).
- New core unit tests: idle (`[]`) not paused; live (`status:"running"`)
  paused; missing field not paused.
- `cargo clippy -p clank-core -p clank` stays at baseline (clank-core
  remains at its existing 6 warnings; no new ones). Git-boundary test
  unaffected (no git/gix touched).

## Deploy

After FINISHED: `cargo install --path crates/cli --force`, then confirm
the live `~/.cargo/bin/clank` mtime bumped. (This replaces the binary the
current session's own Stop hook uses — expected.)
