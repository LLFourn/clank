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

## Hard Direction

- Daemonless. Both surfaces read the same `RepoState` every
  other subcommand does. No HTTP, no shared in-process state.
- **`clank-core` stays pure.** No filesystem reads, no git
  shell-outs in core. Core defines the typed shapes
  (`FeedbackView`, `WorktreeFacts`, `PlanView`) and the pure
  projection over them. CLI does the IO and feeds the results
  in:

  ```rust
  // crates/core/
  pub fn project(
      state: &RepoState,
      plan_key: &PlanKey,
      feedback: &FeedbackView,
      worktree: &WorktreeFacts,
  ) -> PlanView;

  // crates/cli/ — the IO
  //
  // Participants are cumulative across a plan's reviewable
  // commits, so the scan needs the full list of reviewable SHAs
  // (read off `state.fold.plans[key].commits`), not just the
  // latest. Output's `per_commit` is in chronological order.
  pub fn scan_feedback(
      repo: &Path,
      plan: &PlanKey,
      reviewable_shas: &[CommitSha],
  ) -> FeedbackView;

  pub fn read_worktree_facts(
      repo: &Path,
      plan_path: &str,
      head_blob: Option<&str>,
  ) -> WorktreeFacts;
  ```

  `status` and `wfw` both call the CLI-side IO, then hand the
  results to the core projection.
- `wfw` blocks by filesystem watch (`notify` crate, already a
  dep via `fs_watcher`). `status` never blocks.
- `wfw --author <label>` and `--role` are BOTH mandatory. Clap
  rejects invocations missing either. No env detection, no
  config file, no inference. Defaulting role from "has written
  feedback before" breaks the brand-new-reviewer case (their
  first review is exactly when they have no prior feedback);
  rather than ship a heuristic that misfires on the most
  important case, require explicit `--role`. `status` has
  neither — it's repo-wide.
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

Pure function over typed inputs. CLI reads filesystem +
worktree, hands the results in. Both `status` and `wfw` use it.

```rust
/// CLI scans `.clank/feedback/<plan>/<sha>/<author>.md` files
/// into this shape, then hands it to the projection. Core never
/// touches the filesystem.
pub struct FeedbackView {
    /// For every reviewable commit in the plan up to the latest:
    /// the parsed verdicts indexed by author. Older commits feed
    /// the participant set; the latest commit's entries decide
    /// the gate.
    pub per_commit: Vec<CommitFeedback>,
}

pub struct CommitFeedback {
    pub sha: CommitSha,
    pub entries: BTreeMap<AgentLabel, FeedbackEntry>,
}

pub struct FeedbackEntry {
    pub verdict: Verdict,
    pub body_hash: ContentHash,
    /// Path relative to repo root — round-trips for the CLI to
    /// re-read when sealing approvals. Core just stores it.
    pub source_path: String,
}

/// CLI compares `.clank/plans/<key>.md` to its HEAD blob and
/// hands the result in.
pub struct WorktreeFacts {
    pub status: PlanWorktreeStatus, // Clean | BodyDirty | PlanFileMissing
}

pub struct PlanView {
    pub plan: PlanKey,
    pub latest_reviewable_sha: CommitSha,
    pub gate_state: CommitGateState,
    pub waiting_on: WaitingOn,
    pub worktree_status: PlanWorktreeStatus,
    pub last_activity_ts: i64,
}

pub enum WaitingOn {
    /// Nobody has reviewed any commit in this plan yet. Any
    /// reviewer agent can pick it up. Distinct from
    /// `ReviewerApprovalsMissing { missing: [] }` (which would
    /// be ambiguous and is therefore not representable).
    FirstReview,
    /// Latest reviewable commit lacks feedback from one or more
    /// existing participants. `missing` is non-empty by
    /// construction — the empty case is `FirstReview`.
    ReviewerApprovalsMissing { missing: NonEmptyVec<AgentLabel> },
    /// A reviewer requested changes or left an ambiguous verdict.
    /// `requesters` / `ambiguous` tell the master who to address.
    MasterToRevise { requesters: Vec<AgentLabel>, ambiguous: Vec<AgentLabel> },
    /// Gate is approved + worktree clean. Master just needs to
    /// run `clank finish`.
    MasterToFinalize,
    /// Gate is approved but the plan file has uncommitted edits.
    /// Master needs to commit the next revision (or stash).
    MasterToCommit,
}
```

The reviewer-eligibility rule then matches by variant:

```rust
// In derive_work for role=reviewers:
match plan.waiting_on {
    WaitingOn::FirstReview                                  => true,
    WaitingOn::ReviewerApprovalsMissing { missing } if author in missing => true,
    _ => false,
}
```

No empty-vector sentinel, no two-branch test that drifts apart.

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
- Reviewer role: for plans whose `waiting_on` is `FirstReview`
  (any reviewer eligible) OR `ReviewerApprovalsMissing { missing }`
  with `author ∈ missing`, emit a `ReviewerAction` carrying the
  canonical feedback path
  `.clank/feedback/<plan>/<sha>/<author>.md`.

The deeper rules — how the gate is computed, who counts as a
participant, which commit is "latest reviewable" — live entirely
inside `plan_view::project`. `derive_work` only consumes the
output. This keeps the agent-perspective filter (small, simple)
separate from the projection (which is the same data both
surfaces share).

`FeedbackView` is the typed shape both `plan_view::project` and
`preview.rs`'s gate computation consume. Core owns the shape +
the pure interpretation rules. CLI owns the IO that builds it
(`crates/cli/src/feedback_scan.rs`, replacing the inline scan
currently in `preview.rs`). Same data path; the boundary stays
at the IO boundary.

## CLI Surface

### `clank status`

```text
clank status                                # human rendering of the one inferred active plan
clank status --all                          # every active plan
clank status --plan foo                     # specific plan (stem, "foo.md", or ".clank/plans/foo.md" — all accepted)
clank status -j                             # JSON envelope (any of the above)
```

`--plan` accepts the same three forms `clank finish` already
takes via `cli::plan_resolve::parse_arg`: bare stem, `<stem>.md`,
or the full repo-qualified `<basename>/<stem>.md`. Don't invent
a fourth spelling.

Plan inference matches `clank finish`: exactly one visible active
plan in the cwd-repo → use it. Zero → print "no active plan,
nothing pending" (and the head/branch/dirty header) and exit 0.
More than one → exit non-zero with a candidate list and a hint
pointing at `--all` and `--plan`.

`-j` / `--json` envelope. Plans carry both the bare stem
(`plan`, for display) and the canonical path (`plan_path`, for
round-tripping):

```json
{
  "repo_root": "...",
  "branch": "master",
  "head_sha": "...",
  "worktree_dirty": false,
  "plans": [
    {
      "plan": "foo",
      "plan_path": ".clank/plans/foo.md",
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
clank wfw --author <label> --role <master|reviewers>             # blocking, human-readable
clank wfw --author <label> --role <...> -j                       # JSON
clank wfw --author <label> --role <...> --timeout 30m            # max wait; "0" = indefinite (default)
clank wfw --author <label> --role <...> --plan foo               # restrict to one plan (same parser as status)
```

Both `--author` and `--role` are mandatory. Clap rejects
invocations missing either.

JSON output uses the same `plan` + `plan_path` shape as `status`:

```json
{"kind":"master","plan":"foo","plan_path":".clank/plans/foo.md","sha":"abcd...","next":"finalize","reason":"latest_approved"}
{"kind":"reviewer","plan":"foo","plan_path":".clank/plans/foo.md","sha":"abcd...","feedback_path":".clank/feedback/foo/abcd.../codex.md"}
{"kind":"idle"}
```

Exit codes:

- `clank status`:
  - `0` — snapshot emitted (one plan, `--all`, or specific `--plan`).
  - `1` — error (bad args, fold failure).
  - `3` — multiple active plans without `--all` / `--plan`
    (ambiguous; emit candidate list to stderr).
- `clank wfw`:
  - `0` — at least one work item returned. Multi-plan repos
    return the full eligible set; no ambiguity error.
  - `1` — error (bad args, fold failure).
  - `2` — timeout exceeded with no work.

`wfw` does NOT use exit code 3. Multiple active plans is the
common case for reviewers — surfacing the full eligible set is
the point. `--plan <stem>` narrows when the caller wants one.

## Watch Loop

When the initial fold returns `WorkItem::Idle`:

1. Resolve the actual git dir via `git rev-parse --git-dir` and
   `--git-common-dir`. `.git` may be a regular file (worktrees,
   submodules) pointing at the real dir elsewhere, and refs in
   the common dir need watching even when invoked from a linked
   worktree. Watching `<repo>/.git/...` blindly misses both
   cases.
2. Build a `notify::RecommendedWatcher` over:
   - `<git_dir>/HEAD`
   - `<git_common_dir>/refs/`
   - `<git_common_dir>/packed-refs` (if it exists)
   - `<repo>/.clank/plans/`
   - `<repo>/.clank/feedback/`
   - `<repo>/.clank/finished/`
3. Debounce events (200ms) — many writes per logical change.
4. On debounced event: refold, re-derive, return first non-idle
   work item.
5. Honour `--timeout`; exit code 2 on expiry.

Borrow the existing `src/fs_watcher.rs` watcher for the
`.clank/` half; add git-dir-resolution helpers to `git_io.rs`
for the git half.

Linked-worktree invariant the regression test must cover: when
wfw runs inside a linked worktree, it pins the git dir / common
dir from THAT worktree (its own `HEAD`, its own resolved ref).
Updating the main worktree's HEAD does NOT necessarily change
the linked worktree's effective HEAD; updating the ref the
linked worktree points at DOES. The test reflects this: it
mutates the specific ref the linked worktree's HEAD resolves to
(via a separate `git` invocation that targets that ref), then
asserts wfw wakes and refolds against the linked-worktree
context.

## Stubs

`.clank/stubs/` is git-ignored (root `.gitignore` has
`.clank/*` with `!plans/` and `!finished/` carve-outs). Stubs
are operator-local drafts, not tracked project artifacts.
A "prereq commit" can't touch them.

If you have local stubs and want them renamed, do it yourself
(`sed -i '' 's/trinity/clank/g'` over the directory). Clank
doesn't track them and the acceptance grep doesn't cover them.
Out of scope for this plan.

## Sequencing

Two commits, in order:

1. **`clank-core` projection.** Adds:
   - `clank-core::feedback_view::{FeedbackView, CommitFeedback,
     FeedbackEntry}` — pure data shape only. No IO.
   - `clank-core::plan_view::{PlanView, WaitingOn, WorktreeFacts,
     project}`.
   - `clank-core::work::{WorkItem, Role, derive_work}` — the
     agent-perspective filter over `PlanView`.
   - Tests per the Tests section for `project` and `derive_work`.
2. **CLI surfaces.** Adds:
   - `crates/cli/src/feedback_scan.rs` — IO that builds a
     `FeedbackView` from `.clank/feedback/`. Used by both status
     and wfw, and replaces the inline scan in `preview.rs`.
   - `crates/cli/src/worktree_facts.rs` — IO that compares the
     worktree plan file to its HEAD blob and returns
     `WorktreeFacts`.
   - Rewires `crates/cli/src/cli/status.rs` onto the new core
     projection, adds `--all` / `--plan`, enriches JSON to the
     new envelope. Single-plan inference matches `finish`.
   - `crates/cli/src/cli/wfw.rs` — clap subcommand, git-dir
     resolution helpers, watch loop, role + author required.
   - `clank wfw` + status changes registered in `main.rs` clap.

## Tests

- `clank-core::plan_view::project` — table-driven over synthetic
  `RepoState` + `FeedbackView`:
  - `FirstReview` when the intro just landed and no one has
    reviewed yet.
  - `ReviewerApprovalsMissing { missing: [bob] }` when bob is a
    participant and the latest reviewable commit has no `bob.md`.
  - `MasterToRevise` when a reviewer left RequestChanges.
  - `MasterToFinalize` when gate is Approved + worktree clean.
  - `MasterToCommit` when gate Approved but worktree dirty.
- `clank-core::work::derive_work` — table-driven over a synthetic
  `[PlanView]`:
  - master role picks plans whose waiting_on is master-flavored.
  - reviewer role picks plans where `waiting_on` is
    `FirstReview`, OR `ReviewerApprovalsMissing` with `author`
    in `missing`.
  - role=reviewers + author not eligible on any plan → Idle.
- `clank status` integration: synth a temp repo with one active
  plan, assert `clank status -j` matches the documented JSON
  envelope and `clank status` (no flag) produces a non-empty
  human rendering containing the plan stem + waiting_on summary.
  Add a second active plan; assert default exit is non-zero
  with a candidate list, `--all` succeeds with both.
- `clank wfw` reviewer wake-up: synth temp repo, run
  `clank wfw --author alice --role reviewers` in a thread,
  THEN land a new reviewable commit (writing alice's
  `feedback/<plan>/<sha>/alice.md` would COMPLETE the work, not
  trigger it). Assert the command returns the expected
  `ReviewerAction` within the debounce window.
- `clank wfw` master wake-up: synth temp repo with an approved
  gate, run `clank wfw --author <plan-owner> --role master` in
  a thread, then have a second author write a
  `REQUEST_CHANGES` feedback file at the latest reviewable SHA.
  Assert wfw returns `MasterToRevise` (the gate flipped from
  approved → changes-requested).
- `clank wfw` linked-worktree regression: see the Watch Loop
  section for the precise setup. Resolve the linked worktree's
  git dir / common dir, watch the ref its HEAD points at,
  update that ref via a separate `git` invocation, assert wfw
  wakes and refolds against the linked-worktree HEAD.
- Missing `--author` OR missing `--role` on `wfw`: clap exits
  non-zero, stderr names the missing flag.

## Acceptance

- `clank status` in a one-plan repo prints the human rendering
  (gate state, waiting_on, worktree status) for that plan, no
  blocking.
- `clank status -j` in the same repo prints the JSON envelope
  with `waiting_on` populated.
- `clank status` with two active plans exits non-zero, message
  names `--all` and `--plan`.
- `clank status --all` lists every active plan.
- `clank wfw --author alice --role reviewers` (blocking) in a
  repo with no work for alice watches FS and returns the first
  work item when one appears.
- `clank wfw` with no `--author` or no `--role` exits non-zero
  with clap's standard "required argument missing" error naming
  the missing flag.
- `clank wfw --author alice --role master` and
  `--role reviewers` produce different work sets for the same
  repo/state.
- `clank wfw --author alice --role reviewers --timeout 30s`
  exits with code 2 after ~30s when no work appears.
- `clank wfw` running inside a linked git worktree wakes when
  the ref its own HEAD resolves to is updated via a separate
  `git` invocation. (Main-worktree HEAD changes that don't
  touch the linked worktree's resolved ref are NOT required to
  wake it.)
- `plan_view::project` is the single source of truth — both
  `status.rs` and `derive_work` consume it, no parallel
  computation of gate state / waiting_on.
- `clank-core` has no `std::fs::` / `std::process::Command` /
  `tokio::fs::` calls (grep-verifiable). All IO lives in
  `crates/cli/`.

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

- For `wfw -j` blocking mode: NDJSON stream (one work item per
  line as they appear) or a single envelope on wake? Lean:
  single envelope — `wfw` returns one round of work and exits,
  agent loops back for the next round.
