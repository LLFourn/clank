# a-gone-claude-session-starts-fresh
# A Claude agent whose session is gone starts fresh

## Why

`clank agent start` on a bound Claude agent runs `claude --resume <id>`.
When that conversation no longer exists, Claude prints `No conversation
found with session ID: <id>` and exits, and the pane is dead. Seen
2026-11-03 in a fs-war-room story: a master bound to `07f0bbf0-…`. The
binding then stays pointed at the dead id, so every later start fails
the same way until someone clears it by hand. Lloyd's ask: clear the
session id and try again.

The fork path already guards this (agent.rs, "Boundary TWO"):
`session_probe::session_jsonl_exists` is re-checked right before
`--resume … --fork-session`, and a gone transcript launches fresh. The
plain start path (`compose_launch` → `session_restore_args`) has no
such check.

## What changes

### The evidence is Claude's own check-in

Lloyd's design: start the resume, and if the session never checks in,
clear it. No file layout is read and no probe runs, so a change to how
Claude Code stores conversations cannot make a live session read as
gone.

The check-in is Claude's SessionStart hook. It fires when a resume
succeeds (source `resume`). A missing conversation exits with `No
conversation found with session ID: <id>` before any hook runs (seen
live, Claude Code 2.1.308). The probe approaches are dropped: there is
no side-effect-free Claude lookup. `-p` can run a turn, and
`--init-only` exits 0 with no output whether or not the session
exists.

1. **Arm.** A real bound Claude start (not `--print`, not a fork) writes
   `resume_pending: {session, at}` to the agent's config. It is written
   under the binding lock and through the agent store's typed writer,
   right before it `exec`s `claude --resume <id>`. The launch stays an
   exec. If a marker for the same id is already there, it is left
   unchanged, so the 60s run from the first unconfirmed attempt. A
   quick retry after Claude's error does not push the window back.
2. **Check in.** The SessionStart hook, for a Bound Claude session,
   clears `resume_pending` when its `session` equals the hook's session
   id. This is a compare-and-clear under the binding lock, so a
   marker for another id is left alone.
3. **Judge at the next start.** A later bound Claude start reads the
   marker before composing. The prior resume is judged to have **never
   checked in** only when all of these hold:
   - `resume_pending.session` equals the binding's session id;
   - `at` is at least `CHECK_IN_WITHIN` (60s, Lloyd's choice) old;
   - the existing live-holder check found nobody holding it. That check
     already runs first and refuses a start while the agent's pane is
     live.

   Then clank:
   - prints one stderr line: "`<label>`'s Claude session <id> never
     started (no check-in from its last resume); starting a fresh one";
   - clears the binding and the marker together through one new agent
     store operation, `release_unconfirmed_resume(repo, label, session,
     now)`. Under the binding lock it re-evaluates the whole predicate
     against the CURRENT config: the binding id is still `session`, and
     `resume_pending` is still present, for that id, and at least 60s
     old. Only then does it clear both. `release_binding` is not used,
     since it compares only the id;
   - if that returns false, something changed after the judgment. The
     binding may have been replaced, the hook may have disarmed the
     marker, or a fresh arm may have replaced it. Clank does not launch
     fresh on the stale judgment: it says so and exits 1.
   - `exec`s the unbound Claude bootstrap launch
     (`compose_bootstrap_launch`). The fresh session binds at its
     SessionStart as usual.

   A marker younger than 60s resumes as normal. So a retry sooner than
   that shows Claude's error again, and one after it starts fresh.

So the first start after a conversation vanished still shows Claude's
own error. The next start, by a restart in the pane or a re-run of
`clank agent start`, clears the dead id and comes up fresh. Nothing
relaunches on its own, which matches the console's manual-respawn rule.

**This is a policy, not proof the conversation is gone.** The rule is
deliberate: a bound session that hasn't checked in within 60s of its
last unconfirmed resume, with no pane holding it, is forgotten at the
next start. That also forgets a conversation that still exists when the
resume failed for another reason before SessionStart, such as an auth or
config error, a crash at startup, or a missing or broken clank hook.
The conversation itself is not deleted; `clank as` can rebind it.
Arming only when the binding records a `transcript`, a field only the
SessionStart hook writes, limits this to agents whose hook has worked
before. It does not exclude those cases.

Codex, Grok and OpenCode are out of scope.

## Acceptance

- The judgment is a pure function over (marker, binding, now, holder),
  with a table test. It clears only for the marker id matching the
  bound id, 60s or older, with no holder. It keeps for:
  - a younger marker;
  - another id;
  - a live or uncertain holder;
  - no marker.
- Arm/disarm are pinned:
  - a real bound Claude start writes the marker with the bound id;
  - `--print` and fork launches write none;
  - an agent with no recorded transcript writes none;
  - the SessionStart hook for the same id clears it;
  - one for another id leaves it;
  - an unbound or bootstrap start leaves it.
- Race fixtures, each with a change landing between the judgment and
  `release_unconfirmed_resume`. In all three, nothing is cleared and no
  fresh launch is composed:
  - (a) the binding is replaced by another id. The successor is neither
    cleared nor superseded;
  - (b) a same-id SessionStart disarms the marker;
  - (c) the marker is replaced by a fresh arm, younger than 60s.
- Arming over an existing same-id marker keeps its `at`.
- `--print` never judges or writes.
- The config field round-trips, and old configs without it still parse.
- Mutation checks: each of these fails a test:
  - the 60s age guard removed;
  - the id comparison removed, in the judgment or in the hook's
    disarm;
  - the hook's disarm skipped;
  - the marker recheck in `release_unconfirmed_resume` bypassed;
  - its false result ignored;
  - arming without the recorded-transcript guard.
- Tests are run as separate invocations,
  `cargo test -p clank --lib cli::agent`, `... stop_hook` and
  `... agent_store`, then the whole lib. No test spawns `claude`.

## As landed

- `AgentConfig.resume_pending: Option<ResumePending { session, at }>`,
  serde-default and omitted when `None`.
- `agent_store` adds `CHECK_IN_WITHIN`, plus `resume_unconfirmed` (the
  one rule, shared by the start's judgment and the locked release),
  `arm_resume`, `resume_checked_in` and `release_unconfirmed_resume`.
  All of them go through `BindingGuard` + `mutate_locked`. An `at` that
  cannot be parsed never clears.
- The "never started … starting a fresh one" line prints after the
  release succeeds, so a refused release never claims a fresh start.
- If arming fails, the start warns and still launches.
- `start_with` takes its `exec` as a parameter, so the real start path
  is tested end to end except for the exec itself.
