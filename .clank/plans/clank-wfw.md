# clank-wfw

## Summary

Two changes in one plan, since they share a projection:

1. **Enrich `clank status`** to show, per active plan: gate
   state, latest reviewable SHA, who we're waiting on, worktree
   state. Today status is just plan slugs + commit counts — too
   terse to answer "what's pending?"
2. **Add `clank wfw`** — the agent's blocking "park me until I
   have work" primitive. Same projection status uses, filtered
   to the calling agent's perspective.

Clean split:

- `status` = view of the world. No role, no blocking, no agent
  filter. Use for snapshots, scripts, human checks.
- `wfw --author <label>` = block until something meaningful for
  *this* agent appears, then print the work item. Use for the
  agent loop.

Both surfaces project from one function in `clank-core` — single
source of truth. The MCP `wait_for_work` tool was deleted with
the daemon; `clank wfw` replaces it.

The agent picks its own label. `--author <label>` is required on
every invocation — no env detection, no fallback. Two parallel
agents in the same repo collide on the feedback path if they pick
the same label; making the label an explicit choice surfaces that
decision instead of hiding it.

Stubs that still reference `trinity-*` get renamed to `clank-*`
as a prereq.

## Hard Direction

- Daemonless. Both surfaces read the same `RepoState` every
  other subcommand does. No HTTP, no shared in-process state.
- One canonical per-plan projection in `clank-core::plan_view`:
  `pub fn project(state, plan_key, feedback_view) -> PlanView`
  returns gate state, latest reviewable SHA, waiting_on, worktree
  status. `status` aggregates over all active plans; `wfw`
  filters and decides per-agent action.
- `wfw` blocks by filesystem watch (`notify` crate, already a dep
  via `fs_watcher`). On any change under `.git/`, `.clank/plans/`,
  `.clank/feedback/`, `.clank/finished/`, refold and recompute.
  No polling loop. `status` never blocks.
- `wfw --author <label>` is mandatory. Clap rejects invocations
  without it. No env detection, no config file, no inference.
  The agent picks its label and owns the consequence. `status`
  has no `--author` — it's repo-wide.
- Human-readable output by default. `-j` / `--json` opts into
  JSON. Standard CLI convention; agents pass `-j` explicitly.

## Agent Label

Mandatory on every `clank wfw` invocation. The label is parsed
through `clank_core::ids::AgentLabel` (the same validation used by
feedback file paths), so it must match `[a-z0-9][a-z0-9-]*` —
errors come from one place.

The label is identity, not session. It keys two things:

- `.clank/feedback/<plan>/<sha>/<author>.md` — one file per
  label per commit. Two writers with the same label clobber
  each other.
- Participant set (cumulative across a plan's reviewable
  commits). Once `alice` reviewed any commit in a plan,
  `alice` must approve every subsequent commit for the gate
  to close.

Stable across sessions is the property we want. Inferring from
`$CLAUDECODE` or similar gives stability but breaks down with two
parallel Claudes in one repo; inferring from session IDs gives
uniqueness but breaks the participant-set semantics. Both failure
modes are silent. Making the label explicit pushes the choice up
to the agent operator, who can pick meaningfully:

- Solo `claude` in this repo → `--author claude`.
- Parallel Claudes on different concerns → `--author claude-fe`
  and `--author claude-be`.
- Human plus an agent → `--author lloyd` and `--author claude`.

Failure mode if absent: clap-level error, non-zero exit, message
naming the flag.

## Per-Plan Projection (`clank-core::plan_view`)

Both `status` and `wfw` read this shape. One function, two
surfaces.

```rust
pub struct PlanView {
    pub plan: PlanKey,
    pub latest_reviewable_sha: CommitSha,    // always present; intro touches the plan file
    pub gate_state: CommitGateState,         // Unreviewed | ChangesRequested | Approved
    pub waiting_on: WaitingOn,               // see below
    pub worktree_status: PlanWorktreeStatus, // Clean | BodyDirty | PlanFileMissing
    pub last_activity_ts: i64,
}

pub enum WaitingOn {
    /// Latest reviewable commit lacks feedback from one or more
    /// reviewers. `missing` is the cumulative participant set the
    /// gate is short on — `[]` means "no participants yet; open
    /// for the first review." `wfw --role reviewers` treats both
    /// shapes as eligible work (own label in `missing`, OR
    /// `missing.is_empty()`).
    Reviewers { missing: Vec<AgentLabel> },
    /// A reviewer requested changes or left an ambiguous verdict.
    /// `requesters`/`ambiguous` tell the master who to address.
    MasterToRevise { requesters: Vec<AgentLabel>, ambiguous: Vec<AgentLabel> },
    /// Gate is approved + worktree clean. Master just needs to
    /// run `clank finish`.
    MasterToFinalize,
    /// Gate is approved but the plan file has uncommitted edits.
    /// Master needs to commit the next revision (or stash).
    MasterToCommit,
}
```

Every plan in `state.fold.plans` has at least one reviewable
commit (the intro itself, which touched the plan file), so
`latest_reviewable_sha` is always `Some(_)` and there's no
"initial-commit" variant.

`waiting_on` is the human-meaningful summary of "what's blocking
this plan." `status` renders it directly. `wfw` consults it +
the agent's label to decide if the calling agent has a turn.

## Work Derivation

A typed `WorkItem` enum in `clank-core::work`, built on top of
`PlanView`:

```rust
pub enum WorkItem {
    /// You're the plan owner; a reviewable commit just landed and
    /// needs your attention (revise, code, or finalize).
    MasterAction {
        plan: PlanKey,
        sha: CommitSha,
        next: MasterNext,            // Revise | Implement | Finalize
        reason: WaitingReason,       // From clank-core::vocab
    },
    /// You're a reviewer; this commit is new since you last looked.
    ReviewerAction {
        plan: PlanKey,
        sha: CommitSha,
        feedback_path: String,       // Where to write your review.
    },
    /// No work for this role right now. Caller decides whether to
    /// block (watch) or exit.
    Idle,
}
```

`derive_work(views: &[PlanView], author: &AgentLabel, role: Role)
-> Vec<WorkItem>` filters PlanViews by the agent's perspective:

- Master role: for plans whose `waiting_on` is
  `MasterToRevise` / `MasterToFinalize` / `MasterToCommit`,
  emit the corresponding `WorkItem`. Skip plans where master
  isn't blocked.
- Reviewer role: for plans whose `waiting_on` is
  `Reviewers { missing }` with `author` in `missing`, emit a
  `ReviewerAction` carrying the canonical feedback path
  `.clank/feedback/<plan>/<sha>/<author>.md`.

The deeper rules — how the gate is computed, who counts as a
participant, which commit is "latest reviewable" — live entirely
inside `plan_view::project`. `derive_work` only consumes the
output. This keeps the agent-perspective filter (small, simple)
separate from the projection (which is the same data both
surfaces share).

`FeedbackView` is the projection-time scan of `.clank/feedback/`
we already do in `preview.rs`; factor it into core so both
`plan_view::project` and `preview.rs` consume the same source.

## CLI Surface

### `clank status`

```text
clank status                                # human rendering of the one inferred active plan
clank status --all                          # every active plan
clank status --plan clank/foo.md            # specific plan (active or finished)
clank status -j                             # JSON envelope (any of the above)
```

Plan inference matches `clank finish`: exactly one visible active
plan in the cwd-repo → use it. Zero → print "no active plan,
nothing pending" (and the head/branch/dirty header) and exit 0.
More than one → exit non-zero with a candidate list and a hint
pointing at `--all` and `--plan`.

`-j` / `--json` envelope (status-wide, finished plans summarised
inline):

```json
{
  "repo_root": "...",
  "branch": "master",
  "head_sha": "...",
  "worktree_dirty": false,
  "plans": [
    {
      "plan": "clank/foo.md",
      "latest_reviewable_sha": "abcd...",
      "gate_state": "approved",
      "waiting_on": {"kind": "master_to_finalize"},
      "worktree_status": "clean",
      "last_activity_ts": 1716345600
    }
  ],
  "finished_plans": [...]
}
```

`--once` from earlier drafts is gone: `status` covers the snapshot
use case.

### `clank wfw`

```text
clank wfw --author <label>                              # blocking, human-readable, role inferred from prior feedback
clank wfw --author <label> -j                           # JSON output
clank wfw --author <label> --role master
clank wfw --author <label> --role reviewers
clank wfw --author <label> --timeout 30m                # max wait; "0" = indefinite (default)
clank wfw --author <label> --plan-id clank/foo.md       # restrict to one plan
```

Role inference: if `<label>` has ever written a feedback file in
this repo, default to `reviewers`. Otherwise `master`. Simple
heuristic; `--role` is the canonical knob, treat the heuristic
as a fallback.

Exit codes:
- `0` — work returned (printed) or `--once` returned Idle.
- `2` — timeout exceeded with no work.
- `3` — ambiguous setup (multiple active plans, no `--plan-id`).
- `1` — error (bad args, fold failure, missing identity).

JSON shape (one object per item, NDJSON for streams):

```json
{"kind":"master","plan":"clank/foo.md","sha":"abcd...","next":"finalize","reason":"latest_approved"}
{"kind":"reviewer","plan":"clank/foo.md","sha":"abcd...","feedback_path":".clank/feedback/foo/abcd.../codex.md"}
{"kind":"idle"}
```

## Watch Loop

When the initial fold returns empty `WorkItem::Idle` and we're
not in `--once`:

1. Build a `notify::RecommendedWatcher` over
   `.git/HEAD`, `.git/refs/`, `.clank/plans/`, `.clank/feedback/`,
   `.clank/finished/`.
2. Debounce events (200ms) — many writes per logical change.
3. On debounced event: refold, re-derive, return first non-idle
   work item.
4. Honour `--timeout`; exit code 2 on expiry.

Borrow the existing `src/fs_watcher.rs` watcher. It already knows
the filesystem layout and emits typed signals.

## Stubs Cleanup (Prereq)

The stubs under `.clank/stubs/` were never rewritten by the
trinity → clank rename — they're frozen drafts. Reading
`trinity wfw` references in a `clank` project causes confusion.
As a prereq commit:

- Rename filenames containing `trinity` → `clank`:
  - `trinity-cli.md` → `clank-cli.md`
  - `trinity-wfw-cli.md` → `clank-wfw-cli.md`
  - `daemonless-trinity-workflow.md` → `daemonless-clank-workflow.md`
- Sweep `s/trinity/clank/g` and `s/Trinity/Clank/g` across all
  files in `.clank/stubs/`. They're forward-looking drafts; the
  rename is a strict improvement.
- The text `.trinity/` inside stubs becomes `.clank/`.
- The acceptance grep gains `.clank/stubs/` once this lands —
  zero `trinity` hits anywhere in the repo.

This is a separate prereq commit. The `clank-wfw` plan body
references the renamed stub names.

## Sequencing

Three commits, in order:

1. **Stubs cleanup.** Rename + sweep `.clank/stubs/`. Mechanical
   sed + git mv. No code changes.
2. **`clank-core` projection.** Adds:
   - `clank-core::feedback_view` (project from `.clank/feedback/`,
     factored out of `preview.rs`; both `preview.rs` and the new
     projection consume it).
   - `clank-core::plan_view::{PlanView, WaitingOn, project}`.
   - Tests as in the Tests section below for the projection.
3. **CLI surfaces.** Adds:
   - Rewires `crates/cli/src/cli/status.rs` onto `plan_view::project`,
     adds `--all` / `--plan`, enriches JSON to the new envelope.
     Single-plan inference matches `finish`.
   - `clank-core::work` (`WorkItem`, `Role`, `derive_work`) — the
     agent-perspective filter over `PlanView`.
   - `crates/cli/src/cli/wfw.rs` (CLI subcommand + watch loop).
   - `clank wfw` + status changes registered in `main.rs` clap.

## Tests

- `clank-core::plan_view::project` — table-driven over synthetic
  `RepoState` + `FeedbackView`:
  - `Reviewers { missing: [] }` when the intro just landed and
    no one has reviewed yet (open for first review).
  - `Reviewers { missing: [bob] }` when bob is a participant and
    the latest reviewable commit has no `bob.md`.
  - `MasterToRevise` when a reviewer left RequestChanges.
  - `MasterToFinalize` when gate is Approved + worktree clean.
  - `MasterToCommit` when gate Approved but worktree dirty.
- `clank-core::work::derive_work` — table-driven over a synthetic
  `[PlanView]`:
  - master role picks plans whose waiting_on is master-flavored.
  - reviewer role picks plans where `author ∈ missing` OR
    `missing.is_empty()` (open for first review).
  - role=reviewers + author not eligible on any plan → Idle.
- `clank status` integration: synth a temp repo with one active
  plan, assert default JSON matches the documented envelope.
  Add a second active plan; assert default exit is non-zero
  with a candidate list, `--all` succeeds with both.
- `clank wfw` integration: synth temp repo, run
  `clank wfw --author alice` in a thread, write a feedback file,
  assert the command returns the expected work item within the
  debounce window.
- Missing `--author` on `wfw`: clap exits non-zero, stderr names
  the flag.

## Acceptance

- `clank status` in a one-plan repo prints the enriched JSON
  envelope with `waiting_on` populated, no blocking.
- `clank status` with two active plans exits non-zero, message
  names `--all` and `--plan`.
- `clank status --all` lists every active plan.
- `clank wfw --author alice` (blocking) in a repo with no work
  for alice watches FS and returns the first work item when one
  appears.
- `clank wfw` with no `--author` exits non-zero with clap's
  standard "required argument missing" error naming the flag.
- `clank wfw --author alice --role master` and
  `--role reviewers` produce different work sets for the same
  repo/state.
- `clank wfw --author alice --timeout 30s` exits with code 2
  after ~30s when no work appears.
- Zero `trinity` references anywhere in `.clank/stubs/` after
  the prereq commit.
- `plan_view::project` is the single source of truth — both
  `status.rs` and `derive_work` consume it, no parallel
  computation of gate state / waiting_on.

## Out of Scope

- Codex Stop hook integration (see
  [[codex-stop-hook-wfw-experiment]]).
- Multi-repo wait. `clank wfw` is cwd-repo only. Watching N
  repos can be a later flag.
- Multi-phase completion (`--really-finished` semantics — see
  [[wfw-multi-phase-completion-flag]]).
- A persistent agent runner / daemon. The whole point is
  daemonless.
- Web UI / dashboards.

## Open Questions

- Should reviewer-mode `wfw` surface multiple plans' work at
  once, or block until one appears and return it alone? Lean:
  surface all pending reviewer items in one shot — agents can
  iterate.
- For `wfw -j` blocking mode: NDJSON stream (one work item per
  line as they appear) or a single envelope on wake? Lean:
  single envelope — `wfw` returns one round of work and exits,
  agent loops back for the next round.
