# fork-stale-binding-launches-fresh

## Problem

`clank fork` copies each member's bound session id into the fork spec,
and `compose_fork_launch` (`crates/cli/src/cli/agent.rs:423`) emits
`--resume <sid> --fork-session` whenever `from_session` is `Some` —
without checking that the session transcript still exists.

When the binding is stale the agent process exits immediately at
launch, silently: no pane, no transcript, no error surfaced. The
fork's gate then blocks forever on a reviewer that never started.
Measured: `~/src/frostsnap`'s claude was bound to a session from
2026-06-16 whose transcript is long gone; the resulting fork sat on
`waiting on: claude — missing approval` for ~24h. 14 stale bindings
exist across the reporter's repos.

**A stale binding is currently strictly worse than no binding.** With
no binding the fork launches a working fresh session; with a dangling
one it launches nothing at all. That inversion is the bug.

## The modeling error

Session forking is already documented as BEST-EFFORT. `ForkSpec.
from_session` (`fork.rs:33-38`) says so, and the `None` arm of
`compose_fork_launch` already falls back to a FRESH session carrying
the fork's orientation prompt. The fallback exists and is correct.

It is simply keyed on the wrong question. It asks "did the source have
a bound session id?" when the question that matters is "is that
binding still resumable?". A dangling id answers yes to the first and
no to the second, and lands in the resume arm.

**The validator already exists and fork does not call it.**
`crates/cli/src/cli/open.rs:618` has `session_jsonl_exists(tool,
session_id, home) -> Option<bool>`, per-tool
(`claude_session_jsonl_exists`, `codex_session_jsonl_exists`,
`grok_session_dir_exists`), and `open.rs:563-583` branches its three
states deliberately. `clank open` therefore already refuses to resume
a dead session; `clank fork` bypasses the same check. This is one
question with two answers in the codebase, which is the real defect —
the launch bug is its symptom.

## Goal

`clank fork` never emits `--resume` for a session that is known to be
gone. It launches fresh instead, exactly as it does for an absent
binding, and says so.

## Approach

The decision and the warning live at TWO different boundaries, and
they are different processes. Getting this wrong was the first
draft's error: it detected staleness at agent-start, whose stderr
goes to the newly opened pane, while requiring a warning for the
person who ran `clank fork`. Those cannot be the same place.

1. **Share the probe.** Lift `session_jsonl_exists` (and its per-tool
   helpers `claude_session_jsonl_exists`, `codex_session_jsonl_exists`,
   `grok_session_dir_exists`) out of `open.rs` into a module both
   callers own, exported `pub(crate)`. Do NOT copy it — a second
   implementation is how the two paths drifted apart. `open` keeps
   calling the moved function and its behaviour must not change.

   The probe's TRI-STATE is a contract, not an implementation detail:
   `Some(true)` resumable, `Some(false)` known gone, `None`
   unprobeable. `open.rs:616-617` records the review that established
   `None` must never read as "gone"; opencode's store cannot be
   probed, and downgrading unknown to fresh would silently discard
   live sessions.

2. **Boundary one — fork time, while building `MemberSeed`**
   (`fork.rs:485-501`). This is where the source binding is read, and
   the only place whose stderr reaches the caller.

   Probe each member's session there. On `Some(false)`, set
   `from_session = None` in the seed — so the dead id never reaches
   the spec at all — and collect the member for a warning. On `None`
   or `Some(true)`, carry the id through unchanged.

   Warn on STDERR, joining the existing no-binding warning
   (`fork.rs:502-509`), which already reports members that will get a
   fresh session. A stale binding is the same outcome and belongs in
   the same report, distinguished by naming the dead session id.

   **stdout is reserved.** `fork.rs:16-18` and `:197` guarantee the
   worktree path is the SOLE stdout line so
   `clank open --repo "$(clank fork --no-open x)"` composes. No member
   warning may touch stdout.

3. **Boundary two — launch time, in `compose_fork_launch`**
   (`agent.rs:423`). A transcript can be deleted between fork creation
   and pane launch, so a seed validated at boundary one is not proof
   at boundary two. Re-probe before emitting `--resume`; on
   `Some(false)` compose the FRESH launch instead, still carrying the
   orientation prompt.

   This warning goes to the pane, which is correct here: at this point
   the caller is gone and the pane is the only surface. The two
   boundaries are not redundant — the first is the one a human sees,
   the second is the one that makes the guarantee true.

## Required tests

In-process library tests (no binary spawning, no real agent launch):

Boundary one — seed construction:

- **Dangling binding is dropped from the spec**: a source member bound
  to an id with no transcript under a fixture HOME produces a seed
  with `from_session = None`.
- **The caller is told**: that case emits a stderr warning naming the
  member and the dead id.
- **stdout stays clean**: the composition contract holds — the
  worktree path remains the sole stdout line with a stale member
  present. Guards the contract `fork.rs:16-18` states.
- **A live binding is untouched**: transcript present → seed keeps the
  id.

Boundary two — launch composition:

- **Disappearance between fork and launch**: a spec carrying
  `from_session = Some(id)` whose transcript is absent composes the
  FRESH launch — no `--resume`, no `--fork-session` — and still
  carries the orientation prompt. This is the case boundary one cannot
  cover.
- **Live binding still resumes**: composes `--resume <sid>
  --fork-session` unchanged. Guards against "fixing" the bug by
  disabling session forking.
- **Unprobeable tool still resumes**: an opencode spec composes the
  resume form even with no transcript found. Pins `None` is not
  `false` at the fork path.
- **Per-tool coverage**: claude, codex and grok each take the fresh
  path on a dangling id — the resume forms differ per tool
  (`agent.rs:423-452`), so one tool passing proves nothing about the
  others.
- **`open` is unchanged**: its existing resumable tests keep passing
  against the moved function.

## Where these tests may live (measured 2026-08-13)

Driving `run_fork` from the LIB test binary breaks unrelated tests, and
the failure looks like flakiness in someone else's code.

`run_fork` spawns `git`. A forked child briefly inherits open file
descriptors, and an inherited fd holding an `flock` keeps that lock
alive until the child execs. The lib binary also hosts the flock-based
lease tests (`cli::github_events::tests::
ingest_lease_is_exclusive_and_freed_on_drop`, `cli::stop_hook::tests::
park_decision_matrix_and_generation_reads`), whose assertion is
precisely "dropping the holder frees the lease". Two `run_fork` tests
added to the lib binary made both fail under `cargo test` — 2/2 runs,
while passing 3/3 under `cargo test --lib` and in isolation.

Established by bisection, not inference: a worktree at the parent
commit ran green, and re-running with only the two new tests skipped
ran green.

So:

- Classification logic is tested PURELY in the lib
  (`classify_binding`, `summarize_seeds`) — no git, no worktrees.
- The end-to-end assertion on the persisted spec lives in
  `crates/cli/tests/fork_integration.rs`, a separate binary where
  spawning git cannot reach the lib binary's leases.

Note also that `bind_session` in that harness now writes the session's
transcript. Without it "bound" means "dangling" under this plan's
probe, and three existing tests would silently assert the fresh
fallback while claiming to test session forking.

## Acceptance

- No `--resume` is emitted for a transcript that is known absent, on
  any tool, at either boundary.
- A stale binding produces a working fresh session and a stderr
  message to the FORK CALLER naming the member and the dead id.
- `clank fork`'s stdout remains exactly the worktree path.
- `None` (unprobeable) still resumes; no test asserts otherwise.
- One shared probe; `grep` finds no second transcript-existence
  implementation.

## Out of scope

- Repairing or clearing the 14 stale bindings in the reporter's repos,
  and any `doctor` surfacing of stale bindings. Both are worth doing
  and neither is needed to stop fork launching nothing — `open`
  already computes `session_resumable` for that surface, so it is a
  natural follow-on rather than part of this fix.
- The `clank fork` subcommand grammar (see `fork-cli-is-a-noun`).
