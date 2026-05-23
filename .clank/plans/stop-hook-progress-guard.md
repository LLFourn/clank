# stop-hook-progress-guard

## Bug

`clank auto on --mode wait` fires the Stop hook at most once per user
prompt. After the first hook activation returns work and the agent does
real work (commits, edits, tool calls) and ends its turn, the hook is
silent for every subsequent turn-end in that chain — the user has to
manually `clank wfw` to fetch the next batch of work, defeating the
whole point of wait mode.

Cause: `crates/cli/src/cli/stop_hook.rs:44-46` unconditionally
short-circuits to `HookOutcome::Silent` whenever the hook stdin shows
`stop_hook_active: true`. The comment cites "claude force-stops after
8 consecutive blocks and codex loops" as the justification. That's
half-true and over-applied.

## The protocol our guard misreads

Per Claude Code's hooks docs:

- `stop_hook_active: true` means *"this hook fire is a Stop-hook
  continuation, not a user-submitted prompt."* It stays true for every
  hook fire within one chain between user messages.
- The 8-consecutive-blocks force-stop counts **blocks with no agent
  progress between them**. Any intervening tool call resets the
  counter implicitly. The intended pattern is to check
  `last_assistant_message` to detect whether the agent actually wrote
  new content (vs. re-blocking with nothing happening).
- The cap is overridable via `CLAUDE_CODE_STOP_HOOK_BLOCK_CAP`.

So claude's safety net is *progress-aware*. Our guard is *binary*:
"`stop_hook_active=true`? Silent." That conflates "agent did real
work, now stopping" with "agent did nothing, hook is spinning." The
first should fire (it's a legitimate next iteration of the wait loop);
only the second needs to short-circuit.

Codex has no native cap — for codex our guard is the only safety
against runaway loops. The fix needs to keep codex safe.

## The fix

Replace the binary guard with a progress detector that's tool-agnostic
and safe for both claude (where claude's own cap is a backstop) and
codex (where ours is the only one).

Rule:

```text
if !stop_hook_active:
    fresh chain → fire normally, persist this fire's progress signal
elif progress signal differs from last persisted:
    agent did work → fire normally, persist new signal
else:
    no progress → Silent (this is the real spin case)
```

Progress signal: `last_assistant_message` from hook stdin. Claude
populates it directly; codex provides the same field name in its hook
stdin (already typed as `Option<String>` on `HookInput` —
`crates/core/src/hook_io.rs:54`).

Comparison is byte-equality. Hashing or normalization is overkill —
either the field is the same bytes (agent produced nothing new) or
it's not (agent said something).

## Persistence

The hook is a fresh subprocess per fire, so "last persisted progress
signal" lives on disk. One file per agent label per session id:

```text
.clank/agents/<label>/stop-hook-state/<session_id>.json
```

Schema:

```json
{
  "last_assistant_message_hash": "<sha256 of last_assistant_message>",
  "fired_at": "<rfc3339 timestamp>"
}
```

Why session-keyed: a single repo can have multiple concurrent agent
sessions (claude in one terminal, codex in another, two claude
windows). Each session has its own chain; state must not collide.

Why hash, not raw bytes: assistant messages can be tens of kilobytes;
no reason to store them verbatim. SHA-256 of the UTF-8 bytes is fine.

Why a directory under `<label>/`: matches the existing per-agent state
layout (`.clank/agents/<label>/config.json`, `feedback/`, etc.). The
parent `.gitignore` already excludes everything under
`.clank/agents/<label>/` except what's explicitly tracked.

## Implementation surface

### `crates/cli/src/cli/stop_hook.rs::compute_outcome`

Replace:

```rust
if input.stop_hook_active {
    return HookOutcome::Silent;
}
```

with progress-aware logic, **fail-closed on any IO error** for
continuation fires. Codex has no native block-cap, so the guard is
the only thing preventing runaway loops — any uncertainty about
whether we can durably track progress must resolve to
"don't issue a continuation."

### Progress signal validity

Define **valid progress signal** as:
`last_assistant_message.as_deref().map(str::trim).filter(|s| !s.is_empty())`
— `Some(non-whitespace)`. `None`, `Some("")`, and `Some("   ")` all
mean "agent produced nothing new this turn." A trimmed-empty value
is no more evidence of progress than a missing field; both fail
closed.

This trim happens before hashing so the stored hash always
represents non-empty trimmed content. A subsequent fire that comes
back with `Some("")` doesn't accidentally hash to a "different"
value from the prior hash and trick the differs-path into firing.

Decision table (precedence is row order — first match wins):

| `stop_hook_active` | progress signal | state read | state write | Outcome |
| --- | --- | --- | --- | --- |
| any | invalid (`None` / empty / whitespace) | (skipped) | (skipped) | **Diagnostic** (no progress to record) |
| `false` | valid | (skipped) | OK | proceed to normal outcome compute |
| `false` | valid | (skipped) | IO error | **Diagnostic** |
| `true` | valid | OK + hash matches | (skipped) | **Silent** (real spin — agent re-emitted the same message) |
| `true` | valid | OK + hash differs | OK | proceed to normal outcome compute |
| `true` | valid | OK + hash differs | IO error | **Diagnostic** |
| `true` | valid | NotFound | (skipped) | **Diagnostic** (anomaly: mid-chain with no prior state) |
| `true` | valid | IO error (perms, etc.) | (skipped) | **Diagnostic** |

The "invalid progress signal" row sits at the top so it short-
circuits before any state IO; that's the safe order regardless of
`stop_hook_active`.

`Diagnostic` is the right outcome for these failure modes — it exits
0 with a stderr message and **no continuation prompt**, which is
exactly the "fail closed" behavior we want. The existing comment in
`hook_io.rs:71-75` already describes Diagnostic as the "internal
problem" path that exits cleanly without breaking the agent. The
plan's earlier draft (revision 1) claimed Diagnostic would "log but
still fire" — that was wrong; Diagnostic and Continue are mutually
exclusive outcomes.

Sketch:

```rust
let progress = input
    .last_assistant_message
    .as_deref()
    .map(str::trim)
    .filter(|s| !s.is_empty());
let Some(progress) = progress else {
    return HookOutcome::Diagnostic {
        message: "hook: no assistant-message progress signal; refusing to continue".into(),
    };
};
let new_hash = sha256(progress);

if input.stop_hook_active {
    match read_state(&repo, &label, &input.session_id) {
        Ok(Some(prev)) if prev.last_assistant_message_hash == new_hash => {
            return HookOutcome::Silent; // real spin
        }
        Ok(Some(_)) => { /* progressed; fall through to write + fire */ }
        Ok(None) | Err(_) => {
            return HookOutcome::Diagnostic {
                message: "hook: progress state unreadable mid-chain; refusing to continue".into(),
            };
        }
    }
}

if let Err(e) = write_state(&repo, &label, &input.session_id, &StopHookState { ... }) {
    return HookOutcome::Diagnostic {
        message: format!("hook: state write failed ({e}); refusing to continue"),
    };
}

// fall through to the existing auto_mode dispatch
```

The state write happens **before** the auto_mode dispatch so a write
failure aborts the continuation regardless of whether hint or wait
mode would otherwise have produced one.

### State file I/O

New small module `crates/cli/src/stop_hook_state.rs` or inline helpers
in `stop_hook.rs`. ~50 LOC. Two functions:

```rust
fn read_state(repo: &Path, label: &AgentLabel, session: &SessionId)
    -> io::Result<Option<StopHookState>>;
fn write_state(repo: &Path, label: &AgentLabel, session: &SessionId,
               state: &StopHookState) -> io::Result<()>;
```

State struct:

```rust
#[derive(Serialize, Deserialize)]
struct StopHookState {
    last_assistant_message_hash: String, // hex-encoded sha256
    fired_at: String,                    // rfc3339
}
```

### `.gitignore`

No change. `.clank/.gitignore` already blanket-ignores under
`agents/<label>/` except tracked subpaths. The new
`stop-hook-state/` directory falls under the ignore.

### Docs

Update the comment block in `hook_io.rs:48-52` to reflect the actual
protocol: `stop_hook_active=true` marks the chain; `last_assistant_message`
distinguishes progress from spin; claude's cap is the backstop, not a
hair-trigger we have to pre-empt.

Update the inline comment at the old short-circuit site in
`stop_hook.rs` similarly.

## Tests

### Unit (`crates/core` or pure helpers in cli)

- Hash determinism: same input bytes → same hash; differing inputs →
  different hashes.
- State round-trip: write → read → equal.

### Integration (`crates/cli/tests/stop_hook_integration.rs`)

Stop-hook tests already exist there for the per-tool wire shapes.
Extend with progress-guard scenarios:

1. **First fire (`stop_hook_active=false`)** → state persisted,
   normal outcome.
2. **Second fire, new `last_assistant_message`** → state updated,
   normal outcome.
3. **Second fire, same `last_assistant_message`** → Silent, state
   unchanged.
4. **Fire with `last_assistant_message=None`** → Diagnostic, no
   continuation.
4a. **Fire with `last_assistant_message=Some("")`** → Diagnostic
   (empty is not progress, even if a prior fire had a non-empty
   hash that would otherwise "differ").
4b. **Fire with `last_assistant_message=Some("   \n  ")`** →
   Diagnostic (whitespace-only is not progress).
5. **Second fire, prior state file unreadable** (chmod 000 or
   corrupt JSON) → Diagnostic, no continuation. Locks in the
   fail-closed contract.
6. **Second fire, state directory unwritable** (chmod 555 the
   parent) → Diagnostic, no continuation.
7. **Second fire, state file missing while `stop_hook_active=true`**
   (delete it between fires) → Diagnostic, no continuation.
8. **Multiple sessions same label** → independent state files; one
   session's progress doesn't satisfy another's chain.

The existing per-tool wire-shape tests (claude exit 2, codex
`{"decision":"block",...}`) keep working unchanged — the
discriminator runs before outcome rendering.

## Acceptance criteria

- After the hook fires, the agent commits + ends its turn, the hook
  fires AGAIN on the next turn-end (within the same user-prompt
  chain), and continues to fire until either work runs out
  (`HookOutcome::Silent` from the normal hint/wait branches), the
  agent produces no new `last_assistant_message` (real spin), or
  claude's `CLAUDE_CODE_STOP_HOOK_BLOCK_CAP` triggers.
- Per-session state file lands at
  `.clank/agents/<label>/stop-hook-state/<session_id>.json` and
  round-trips correctly.
- A genuine spin (hook returns block, agent does nothing) still
  short-circuits to Silent on the second fire — protecting codex
  (which has no native cap) and reducing the rate of claude's
  force-stop trigger.
- State-file IO failures (read error mid-chain, write error,
  unreadable corrupt file, missing field on stdin) **fail closed**:
  the hook returns `Diagnostic` (exit 0, stderr message, no
  continuation). The agent stops cleanly; the user sees the
  stderr line. The hook never issues a continuation it can't
  durably track.

## Out of scope

- **Raising the 8-cap.** It's a Claude Code internal set by
  `CLAUDE_CODE_STOP_HOOK_BLOCK_CAP`. Users who want longer chains can
  export the env var (document in README's troubleshooting section);
  having `clank setup` mutate the env is brittle and not worth it.
- **Codex's equivalent of the cap.** Codex has no native cap; our
  progress guard is the only safety. Good enough — the spin condition
  (no `last_assistant_message` change) catches the same failure mode
  claude's cap catches.
- **Cross-session coordination.** State is per-session; concurrent
  sessions in the same repo don't share or contend.
- **Cleaning up old state files.** Stale entries under
  `stop-hook-state/` will accumulate as sessions end; a future
  `clank purge` extension or a TTL sweep can address this. Not
  blocking for the bug fix.
