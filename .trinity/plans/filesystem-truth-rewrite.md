# Filesystem-Truth Rewrite — Delete the Database

## Summary

Trinity stops being a database that watches files. It becomes an in-memory cache + helpful metadata editor on top of plan files committed to git and feedback files held in the working tree. SQL is deleted in full: no sqlx, no migrations, no `src/storage/`, no `apply.rs`, no `curator.rs`.

Commit-to-session attribution is **derived from git topology alone**: a commit that touches exactly one plan file owns that session; a commit that touches zero plan files walks first-parent until it finds one; a commit that touches multiple plan files is unattributed. A commit can be both a plan revision and an implementation commit (touches a plan and code). No commit trailers, no `git notes`, no claim files, no overrides. If you want to change attribution, amend the commit.

Trinity reacts to **any HEAD change** (branch switch, commit, reset, rebase, merge) by rebuilding the affected repo's in-memory state from disk + git. The straight-line workflow is "one project, plans applied in some order"; parallel work uses separate worktrees with separate Trinity instances.

Live activity is an in-memory ring buffer driving SSE and the existing chime; restarting drops the live feed. History lives in git and is rendered per-session on demand.

Goal: aggressively delete code. Current `src/` is 10,412 lines. Target: under 5,000.

## Core Invariant

The filesystem and git are the source of truth. For each known repo:

- `<repo>/.trinity/plans/<session>.md` — **committed**. Plan body, plain markdown, no frontmatter.
- `<repo>/.trinity/plans/done/<session>.md` — **committed**. Finished/archived sessions live here (`mv` from `plans/`; operator stages and commits).
- `<repo>/.trinity/feedback/<session>/<plan|impl>/<target-sha>/<author>.md` — **gitignored**. One file per (phase, target SHA, author). Marker is `APPROVE` or `REQUEST_CHANGES` on the first non-empty line.
- Git history — implementation commits and plan revisions. Walk-back attribution derives ownership.

Trinity's in-memory state is a derived cache. Any HEAD change for a repo rebuilds that repo's state from scratch.

**What Trinity never does:** edit user-authored plan content, write to the git index, create or amend commits, write git notes, modify history in any form. Trinity reads git and the working tree; it edits files in the working tree (feedback file moves, `.gitignore` setup at `start_plan`, the plan-file creation that `start_plan` performs explicitly); it never reaches into git's own data.

## Commit Attribution — pure git walk

The four rules:

1. Commit touches exactly one plan file → `Attributed { session, plan_touch: Some(...), has_code_changes: <true if any non-`.trinity/` file changed> }`.
2. Commit touches zero plan files → walk first-parent recursively until a single-plan-touch ancestor is found; `Attributed { session: ancestor's session, plan_touch: None, has_code_changes: <true if this commit touched any non-`.trinity/` file> }`.
3. Commit touches multiple plan files → **Unattributed**. Walk-back consumers treat it as if it touched zero plans (transparent to descendants).
4. Walk reaches root without finding a single-plan-touch commit → **Unattributed**.

Plan-touch detection uses `git diff-tree -r --name-status -M <sha>` so renames (e.g. `plans/<id>.md` → `plans/done/<id>.md`) are recognized as a single `R` entry rather than a `D` + `A` pair that would otherwise look like a multi-plan commit.

`plan_touch` kinds:
- `Intro` — first appearance of `plans/<session>.md` in git (`A` status on `plans/<session>.md` with no prior history).
- `Revision` — `M` (or `A` if a removed plan was recreated) on `plans/<session>.md`.
- `DoneMove` — `R` from `plans/<session>.md` to `plans/done/<session>.md` (or reverse).

A **mixed commit** (touches plan A + code) is `Attributed { session: A, plan_touch: Some(Revision), has_code_changes: true }`. It appears in BOTH the plan-revisions list AND the impl-commits list. UI marks it "Plan revision + impl."

To switch sessions on the same branch, commit a touch to the new plan file. To override attribution on a single commit, amend it to change which plan paths it touches. There is no claim file, no trailer convention, no note ref, no operator override.

**Workflow constraint:** alternating sessions on the same linear chain requires a plan-touch when switching. `impl_A, impl_no_plan_touch_for_B, impl_A_2` would mis-attribute the middle and (depending on its descendants) `impl_A_2` to A because walk-back doesn't see B's plan. The user's rule "to move to a new plan you commit a new plan" prevents this and leaves a useful audit signal.

**Branches:** Trinity supports the current worktree's HEAD only. Plans on un-checked-out branches are invisible. For parallel work on multiple branches, use separate worktrees (each runs its own Trinity instance against its own HEAD). The straight-line model is the default; branches in a single worktree are not a supported parallelism story.

## HEAD-change → full rebuild

Whenever Trinity observes a HEAD change for a repo (branch switch, new commit, reset, rebase, merge), it rebuilds that repo's in-memory state from scratch:

1. Walk `<repo>/.trinity/plans/` and `.../plans/done/`; rebuild `sessions` map.
2. Compute `plan_intro` for each session via `git log --diff-filter=A --follow --format=%H -- <plan_path> | tail -1`.
3. Walk reachable history from current HEAD (e.g. `git log --format=%H --first-parent <oldest_plan_intro>..HEAD`); for each commit, run `git diff-tree -r --name-status -M`, classify per the four rules, populate `attribution`.
4. Reload feedback files into `plan_feedback` / `impl_feedback` maps.
5. Broadcast a single live event noting the rebuild (so the homepage refreshes any open SSE viewers).

No incremental walk, no `<prior_head>..<new_head>` cleverness. Always full rebuild. Cold-start cost is small enough (sub-250 ms for 10 sessions × 50 commits each) that incrementalism isn't worth the bug surface.

## What Goes Away

| | Today | After |
|---|---|---|
| sqlx | 1.x dep | **deleted** |
| migrations | `migrations/` (137 lines) | **deleted** |
| storage modules | `src/storage/*` (~1,800 lines) | **deleted** |
| apply layer | `src/daemon/apply.rs` (395) | **deleted** |
| recovery / curator | `src/daemon/curator.rs` (200) | **deleted** |
| events table & cursors | + every cursor callsite | **deleted** |
| review_gate_overrides table | + storage + apply hooks | **deleted** — no overrides at all |
| repo_effective_sessions table | + storage + apply hooks | **deleted** — walk-back attribution |
| `claim_session` MCP tool | explicit-claim flow | **deleted** — commit a plan-touch instead |
| `.trinity/claimed-session` file | claim hint | **deleted** |
| feedback_files table | sidecar tracking | **deleted** — in-memory only |
| plans VIEW + plans table compat | every storage call site | **deleted** |
| plan_revisions table | revision row per body change | **deleted** — `git log --follow` |
| implementation_revisions table | commit row per impl | **deleted** — walk-back attribution |
| `SessionService` god struct | 1,478 lines | shrunk to a few hundred |
| `LifecycleServiceError` 8 variants | conflated reducer + apply + SQL + watcher | 2–3 focused error types |

Net target: 4,500–5,500 LOC removed from `src/`. Tests shrink similarly — assertions move from "SQL row state" to "filesystem state" or "git log shows X."

## State Shape (in memory)

```rust
pub struct Trinity {
    pub repos: BTreeMap<RepoRoot, RepoState>,
    pub live_events: VecDeque<LiveEvent>,   // ring buffer ~200, ephemeral
    pub watchers: WatcherRegistry,
}

pub struct RepoState {
    pub root: PathBuf,
    pub sessions: BTreeMap<SessionId, Session>,
    pub head: Option<CommitSha>,
    pub attribution: BTreeMap<CommitSha, AttributionResult>,
}

pub enum AttributionResult {
    Attributed {
        session: SessionId,
        plan_touch: Option<PlanTouchKind>,    // Some if this commit touched the session's plan
        has_code_changes: bool,                // true if this commit touched any non-`.trinity/` file
    },
    Unattributed,                              // multi-plan-touch OR walk hit root
}

pub enum PlanTouchKind { Intro, Revision, DoneMove }

pub struct Session {
    pub id: SessionId,                         // = basename of plan file
    pub plan_path: PathBuf,                    // canonical: plans/<id>.md or plans/done/<id>.md
    pub body: String,
    pub body_hash: ContentHash,
    pub plan_feedback: BTreeMap<(CommitSha, AgentLabel), Feedback>,
    pub impl_feedback: BTreeMap<(CommitSha, AgentLabel), Feedback>,
}

pub struct Feedback {
    pub path: PathBuf,                         // .../<phase>/<target-sha>/<author>.md
    pub body: String,
    pub verdict: Verdict,
}

pub enum Verdict { Approve, RequestChanges, Unmarked }

pub struct LiveEvent {
    pub ts: i64,
    pub repo: RepoRoot,
    pub session_id: Option<SessionId>,
    pub kind: &'static str,   // "plan_revision" | "feedback" | "impl_commit" | "session_moved_to_done" | "repo_rebuilt" | ...
    pub payload: serde_json::Value,
}
```

Derivations:

- **Phase per session:** `attribution` contains some `Attributed { session: X, has_code_changes: true, .. }` past `plan_intro` → `implementing`. Else, if plan file is committed → `planning`. Else (plan file in working tree but not committed) → `pre_committed`.
- **Currently-focused session for UI:** session of the most-recently-`Attributed`-with-`has_code_changes` commit; falls back to most-recent plan-touch commit. Derived; no state file.
- **Plan revisions list for session X:** `Attributed { session: X, plan_touch: Some(Revision | DoneMove), .. }`. `git log --follow --format=%H -- <current_plan_path>` is the canonical query.
- **Implementation commits list for session X:** `Attributed { session: X, has_code_changes: true, .. }`.
- **Mixed commit:** appears in both the plan-revisions and impl-commits lists. UI marks the row.
- **Review gate:** derived from `plan_feedback` / `impl_feedback` against current target SHA. SHA-anchored — stale ≡ "verdict for a SHA that isn't the current target." No persistence, no overrides.

## Disk Format

### Plan files

Plain markdown. No frontmatter required. Trinity ignores any frontmatter you keep for your own purposes.

State encoding via directory:
- `.trinity/plans/<session>.md` → active (planning or implementing, derived from attribution).
- `.trinity/plans/done/<session>.md` → finished/archived. Trinity uses `git log --follow` to render history.

`POST /sessions/{id}/done` does a plain `std::fs::rename` of `<repo>/.trinity/plans/<id>.md` to `<repo>/.trinity/plans/done/<id>.md`. **Trinity does not stage or commit the move.** The operator runs `git add` + `git commit` themselves. (Same pattern as `start_plan`: Trinity edits the working tree; the human owns the index and the commits.)

### Feedback files

`<repo>/.trinity/feedback/<session>/<plan|impl>/<target-sha>/<author>.md`

First non-empty line: `APPROVE` or `REQUEST_CHANGES`. (The target SHA is in the path, so the marker line stays simple.) Body follows.

**Auto-organization for flat drops.** If a reviewer writes `<phase>/<author>.md` without a SHA subdirectory, Trinity moves it to `<phase>/<current-target-sha>/<author>.md` on observation. The reviewer wrote a verdict; Trinity infers the target from current state and parks the file at the canonical path.

**History preservation.** Old feedback files for past targets stay in their `<old-sha>/` directories. The UI surfaces them as historical verdicts. Only files under the current target SHA count toward the current gate.

### `.gitignore` setup

`start_plan` ensures the repo's `.gitignore` contains:

```
.trinity/feedback/
.trinity/cache/
```

If `.trinity/` is wholesale-ignored, `start_plan` returns a clear error and refuses to proceed.

## Reducer

Per-repo, in memory. One `tokio::sync::Mutex<RepoState>` per repo.

```rust
pub enum Observation {
    PlanFileChanged { session_id, body, current_path: PathBuf },
    PlanFileMoved { from: SessionId, to_path: PathBuf },         // active↔done
    PlanFileGone { session_id },
    HeadChanged,                                                  // full rebuild signal
    FeedbackChanged { session_id, phase, target_sha: Option<CommitSha>, author, body, path },
    FeedbackGone { session_id, phase, target_sha: Option<CommitSha>, author },
    OperatorMoveToDone { session_id },                            // does the working-tree mv only
}

pub enum Effect {
    MoveFeedbackFile { from: PathBuf, to: PathBuf },              // auto-organize flat drops
    MovePlanToDone { from: PathBuf, to: PathBuf },                // plain mv; no git operations
    RebuildRepo,                                                  // triggers the cold-start-style rebuild
    BroadcastLive { event: LiveEvent },
}

pub fn step(repo: &mut RepoState, obs: Observation, now: i64) -> Vec<Effect>;
```

Properties:

1. `step` is the only state mutation point for the in-memory state. `RebuildRepo` is the one effect that wholesale replaces `RepoState` from disk + git.
2. `step` is pure. No `tokio::fs`, no `chrono::now()`, no broadcast sends. Caller supplies `now`; caller runs effects.
3. Effects are filesystem operations + the rebuild trigger + broadcasts. No SQL, no git index writes, no commits, no notes.

Self-write ring (`pending_self_writes: HashMap<PathBuf, ContentHash>`, ~20 entries, 5s expiry) covers feedback file moves and `plans/<id>.md` → `plans/done/<id>.md` moves done by Trinity. Plans, the git index, and notes are never written by Trinity.

## Watchers

`notify` recursive watcher per repo, rooted at `<repo>/.trinity/`. Plus `notify` on `<repo>/.git/HEAD` and `<repo>/.git/logs/HEAD`. One thread aggregates events into a single channel keyed by repo.

Path → observation translation:

- `.trinity/plans/<session>.md` write → `PlanFileChanged`
- `.trinity/plans/<session>.md` removed → `PlanFileGone` or (if `.trinity/plans/done/<session>.md` create event arrives in the same debounce window) `PlanFileMoved`
- `.trinity/feedback/<session>/<plan|impl>/[<sha>/]<author>.md` write → `FeedbackChanged` (`target_sha` parsed from path, `None` for flat drops)
- `.trinity/feedback/.../<author>.md` removed → `FeedbackGone`
- `.git/HEAD` change OR `.git/logs/HEAD` change → `HeadChanged` → `RebuildRepo` effect

One watcher per repo. The per-session-per-feedback-dir explosion is gone.

## Timeline — two distinct things

**Live activity feed (in-memory, ephemeral, fires the chime):**

- Each `Effect::BroadcastLive` appends to the ring buffer (~200) and broadcasts via SSE.
- Existing chime infrastructure (`ui.rs:1199–1230`, `data-sound-test`) plays on each new SSE event by default.
- Reconnect gets the ring suffix. Older events live in git anyway.
- Restart wipes the ring. **Intentional.**

**Per-session history view (on-demand, derived from git):**

- Session detail page: `git log --follow --format=... -- <current_canonical_path>` for plan revisions. Each row links to a body diff against the previous plan-touching commit.
- Implementation commits: filter `attribution` map for `Attributed { session: this, has_code_changes: true }` entries. Each links to `/sessions/{id}/commit/{sha}` rendering `git show <sha>`.
- Mixed commits appear in both lists with a "Plan revision + impl" marker.
- No caching. Rebuilds on every render. `git log --follow` is fast enough.

The homepage shows current state per session + the live ring. No persistent timeline view.

## Cold Start

```
log_loudly_about_legacy_sqlite_if_present();    // never delete
for repo in read_known_repos("~/.trinity/repos") {
    register_repo_watcher(&repo);
    rebuild_repo(&repo).await;                  // same path used for any HEAD change
    trinity.repos.insert(repo, state);
}
```

`rebuild_repo`:
1. Walk `<repo>/.trinity/plans/` and `.../plans/done/`; build `sessions` map (id, plan_path, body, body_hash).
2. `git rev-parse HEAD` → store `head`.
3. For each session, compute `plan_intro` via `git log --diff-filter=A --follow`.
4. Walk reachable commits from HEAD over `oldest_plan_intro..HEAD`, classify each via `git diff-tree -r --name-status -M`, populate `attribution`.
5. Walk feedback directories per session; populate `plan_feedback` / `impl_feedback`.

Typical cost: well under 250 ms for 10 sessions × 50 commits past plan_intro per session.

## Multi-Process Safety

`flock` on `~/.trinity/daemon.lock`. Second instance refuses to start.

## MCP Surface

Three tools.

### `start_plan`

Inputs: `session_id`, `path` (optional), `label`.

- **Create-if-missing.** If `<repo>/.trinity/plans/<session>.md` doesn't exist, create it (empty body, or body supplied in args). If it exists, adopt — return the canonical path, never overwrite.
- Ensures `.gitignore` contains the two required lines; appends if missing. Refuses if `.trinity/` is wholesale-ignored.
- Records the repo in `~/.trinity/repos` if new.
- Returns `{canonical_path, phase, base_advice}` where `base_advice` reminds the agent to commit the plan file before any impl work.

### `get_context`

Inputs: `session_id`, `author_label`. Returns:

- `phase` (`pre_committed` | `planning` | `implementing`), `expected_action`
- `review_gate` (derived, SHA-anchored, no overrides)
- `review_target`, `latest_plan_revision`, `latest_implementation_revision` (derived)
- `write_feedback`: `{ kind, path: ".../<phase>/<current-target-sha>/<author>.md", status }`
- `prior_feedback`, `other_feedback_files` (each entry includes `target_sha`)
- `pr_hint` (when phase = implementing):
  ```json
  {
    "plan_intro": "<sha-where-plan-was-added>",
    "plan_intro_parent": "<parent-of-that>",
    "implementation_commits": ["<sha1>", "<sha2>", ...],
    "options": [
      { "name": "keep_plan_in_pr",      "base": "<plan_intro_parent>", "command": "git reset --soft <plan_intro_parent> && git commit -m '...'" },
      { "name": "exclude_plan_from_pr", "base": "<plan_intro_parent>", "command": "git reset --soft <plan_intro_parent> && git rm <plan_path> && git commit -m '...'" }
    ],
    "suggested_message": "<derived from plan title>"
  }
  ```

For `phase: pre_committed`, `expected_action: commit_plan_file` and the response includes the exact `git add` command.

### `list_sessions`

Inputs: optional `repo_root`. Returns all sessions across known repos. From in-memory state, with current phase + currently-focused indicator (derived from the most-recently-attributed commit).

## HTTP Surface

```
GET  /                                   home: all sessions
GET  /sessions/{id}                      session detail (plan revisions + impl commits + mixed via git log + attribution)
GET  /sessions/{id}/plan/{sha}           plan body at sha (git show)
GET  /sessions/{id}/commit/{sha}         commit diff (git show)
GET  /events                             home SSE — live activity feed
GET  /sessions/{id}/events               per-session SSE
POST /sessions/{id}/done                 plain mv plans/<id>.md plans/done/<id>.md (operator stages and commits)
```

That's the full route list. No claim, no override, no attribute-retroactively. To change attribution, amend the commit. To change a review verdict, edit the feedback file. To finish/archive (Trinity makes no semantic distinction), use `POST /sessions/{id}/done`.

## Self-Edits

The invariant: **Trinity does not autonomously edit user-authored plan content and never writes to git history or the git index.** What Trinity may write:

- Feedback file content/path (marker normalization, auto-organization). Gitignored.
- Plan file at `start_plan` when the file does not exist — explicit user MCP action, not autonomous.
- `.gitignore` at `start_plan` — explicit user MCP action.
- `<repo>/.trinity/plans/<id>.md` → `<repo>/.trinity/plans/done/<id>.md` plain `mv` on operator's explicit `POST /sessions/{id}/done`.
- `~/.trinity/repos` (append) at `start_plan`.

What Trinity never does: write a commit, amend a commit, run `git add`, `git mv`, `git rm`, write git notes, or modify any committed content.

The self-write ring covers the writes Trinity does perform so its own edits don't loop back as foreign observations.

## Implementation Steps

One large rewrite, sequenced as six commits.

1. **New core, alongside old.** `src/repo_state.rs`, `src/reducer.rs`, `src/disk_format.rs`, `src/attribution.rs`. Pure types + pure reducer + pure tests for marker parsing, target-sha path layout, the four attribution rules over synthetic commit graphs (including mixed commits, multi-plan-touch, walks-to-root), state derivation.
2. **New watchers + main loop.** Recursive watcher per repo, plus `.git/HEAD` + `.git/logs/HEAD`. New main loop owns the reducer + effect runner. `HeadChanged` → `RebuildRepo`. Old watcher tree shuts off for `.trinity/` paths.
3. **HTTP migration.** Each route reads from in-memory state. Session detail swaps to `git log --follow` + attribution map. Live SSE switches to the ring buffer; chime wiring preserved.
4. **MCP migration.** Rewrite `start_plan`, `get_context`, `list_sessions` against in-memory state. Add `pr_hint` to `get_context`. Drop `claim_session`. Replace `tools/master.rs` + `tools/reviewer.rs` with one ~150-line `src/tools.rs`.
5. **Rip out SQL.** Delete `migrations/`, `src/storage/`, `src/daemon/apply.rs`, `src/daemon/curator.rs`, the old `src/lifecycle.rs`, the old `src/daemon/service.rs`. Remove `sqlx` and SQL-only deps. Log loudly about legacy `~/.trinity/trinity.sqlite*` on startup; do not delete.
6. **Rewrite tests.** SQL state assertions → filesystem or git-log assertions. Drop SQL setup helpers.

## Acceptance Criteria

- **No `sqlx`.** `cargo tree | grep sqlx` is empty.
- **No `migrations/`, no `src/storage/`, no `src/daemon/apply.rs`, no `src/daemon/curator.rs`.**
- **`src/` total LOC under 5,000.**
- **No `INSERT`, `UPDATE`, or `DELETE` SQL anywhere.**
- **No commit trailers required.** Trinity reads no special trailers and writes none.
- **No git notes ref.** Trinity reads no notes and writes none.
- **No claim file.** `.trinity/claimed-session` does not exist.
- **No override file.** `.trinity/overrides/` does not exist.
- **No git index writes.** Trinity never runs `git add`, `git mv`, `git rm`, `git commit`, `git commit --amend`, or `git notes`.
- **HEAD change triggers full rebuild.** Any change to `.git/HEAD` or `.git/logs/HEAD` rebuilds the repo's in-memory state from scratch within one debounce window.
- **Branch switch behaves correctly.** Switching branches with different plan-file states yields a correct in-memory view for the new HEAD (sessions absent on the new branch are gone; sessions present on the new branch are populated with their reachable history).
- **Cold start under 250 ms** for 10 repos × 10 sessions × 50 commits past plan_intro per session.
- **Restart preserves state.** Killing and restarting reproduces the same homepage + per-session views from disk + git alone. Ring buffer is empty.
- **Chime fires on each new live SSE event** by default; mute toggle preserved.
- **`.gitignore` setup works.** `start_plan` in a fresh repo adds the two required lines; refuses if `.trinity/` is wholesale-ignored.
- **Plans in git, feedback not.** After a full workflow, `git status` shows plan files committed, feedback ignored.
- **Walk-back attribution.** Commits attribute correctly per the four rules: single-plan-touch → `Attributed`; zero-plan-touch → walk-back; multi-plan-touch → `Unattributed`; root reached → `Unattributed`.
- **Mixed commits.** A commit touching plan A + code-for-A appears in both the plan-revisions list and the impl-commits list with a "Plan revision + impl" marker, and walk-back descendants find it as the plan anchor.
- **Switching sessions via plan-touch commit.** Committing a plan-touch for session B causes subsequent zero-plan commits to attribute to B.
- **Plan history follows renames.** `mv plans/<id>.md plans/done/<id>.md` + operator commit preserves history via `git log --follow`. Rename detection uses `git diff-tree -r --name-status -M`.
- **Feedback auto-organization.** Writing `<phase>/<author>.md` flat results in Trinity moving it to `<phase>/<current-target-sha>/<author>.md` within one watcher round.
- **`pr_hint` is unambiguous.** Both `plan_intro` and `plan_intro_parent` exposed; per-option commands run correctly.
- **No `~/.trinity/trinity.sqlite*` deletion.** Legacy DB logged about, never mutated.
- **Attribution overrides require amending.** Trinity offers no UI/MCP/HTTP affordance for changing a commit's session. Operators amend the commit to change which plan paths it touches.

## Tests

Pure (`reducer.rs`, `attribution.rs`, `disk_format.rs`):

- Attribution: single-plan-touch (no code) → `Attributed { plan_touch: Some(_), has_code_changes: false }`.
- Attribution: single-plan-touch + code → `Attributed { plan_touch: Some(Revision), has_code_changes: true }` (mixed).
- Attribution: zero-plan-touch with code → walks to first single-plan-touch ancestor; `Attributed { plan_touch: None, has_code_changes: true }`.
- Attribution: zero-plan-touch with no code (e.g., empty merge commit) → walks back; `has_code_changes: false`.
- Attribution: multi-plan-touch → `Unattributed`; descendants walk through it transparently.
- Attribution: walk reaches root without finding a plan → `Unattributed`.
- Attribution: `DoneMove` recognized via `R` status in `--name-status -M` output.
- Rebuild on `HeadChanged`: prior in-memory state is replaced; sessions absent on new HEAD are removed; new sessions are added.
- `PlanFileMoved` from active → done: state updates; broadcast event emitted; no git operations triggered.
- `FeedbackChanged` flat `<phase>/<author>.md` → `MoveFeedbackFile` to `<phase>/<current-target-sha>/<author>.md`.
- `FeedbackChanged` at a stale target SHA → verdict parsed; recorded under that SHA's history; does not affect current gate.
- `OperatorMoveToDone` → `MovePlanToDone` effect (plain mv); no `StageGitMv`-style effect, no auto-commit.

Integration (`tests/`):

- `start_plan` in a fresh repo: plan file created, `.gitignore` updated, repo registered. Idempotent on re-call.
- `start_plan` adopts an existing plan file without overwriting body.
- `start_plan` in a repo with `.trinity/` wholesale-gitignored: clear error.
- Commit the plan (`A` status); commit code: attribution map records the impl commit against the session; phase flips to implementing.
- Commit the plan for B (while A also exists); commit code: walk-back finds B's plan → impl commit attributes to B.
- Mixed commit (plan revision + code in same commit): appears in both plan-revisions and impl-commits lists; phase is implementing; descendants walk back to this commit as the plan anchor.
- Amend a commit to add a plan path: HEAD changes → full rebuild → attribution recomputes; UI updates within one watcher round.
- Multi-plan commit: attribution records `Unattributed`; descendants walk through transparently.
- Branch switch: `git checkout other_branch` triggers `HeadChanged` → rebuild; in-memory state reflects the new branch's plan files and attribution.
- Drop a flat feedback file: Trinity moves it to the current-target subdir.
- Drop a feedback file under a stale target SHA: kept where it is; visible in UI history; does not affect current gate.
- Restart mid-workflow: homepage + per-session views reproduce identically; ring buffer empty.
- `mv plans/<id>.md plans/done/<id>.md` + operator commit: `git status` shows the rename in the index after operator runs `git add`; session detail page renders plan-revision history via `--follow`.
- `pr_hint.options[*].command` produces a clean staged diff that excludes the plan when `exclude_plan_from_pr` chosen.
- Legacy `~/.trinity/trinity.sqlite` present: startup logs about it, does not delete it, runs normally.
- No `git add`/`git commit`/`git mv` observed in process traces during any Trinity action.

## Non-Goals

- **No persistent timeline outside git.** Events ring is in-memory only. Git is the history.
- **No backward compatibility with `.trinity/trinity.sqlite`.** Operators delete it themselves.
- **No Trinity autocommits, amends, or index writes.** Plans are author-owned. Trinity edits files in the working tree only.
- **No "structural feedback" table.** Feedback is files. Verdicts are markers. Targets are encoded in paths. Gates are derived.
- **No claim concept.** To work on session X, commit a touch to X's plan. The commit is the claim.
- **No overrides.** Attribution: amend the commit. Review gate: edit the feedback file. There are no escape-hatch mechanisms beyond what's already in git or on disk.
- **No multi-branch parallelism in a single worktree.** Trinity supports the current worktree's HEAD only. Plans on un-checked-out branches are invisible. Use separate worktrees for parallel work.
- **No incremental attribution walk on `HeadChanged`.** Always full rebuild. Removes a whole class of consistency bugs at small cost.
- **No new MCP tool beyond the three.** Anything that would be a fourth is either an HTTP route or a file the agent can edit directly.

## Implementation Notes

Non-blocking specifics surfaced during plan review. Pinned here so they don't get rediscovered mid-implementation.

### One daemon, many repos

The `~/.trinity/daemon.lock` is global. The model is "one Trinity per machine, tracking many repo roots from `~/.trinity/repos`." Don't introduce per-worktree daemons without first making the lock per-worktree.

### Linked-worktree gitdir resolution

In a linked worktree from `git worktree add`, `<worktree>/.git` is a *file* containing `gitdir: /path/to/main/.git/worktrees/<name>`, not a directory. Watcher attachment must:

1. Stat `<worktree>/.git`. If it's a regular file, parse the `gitdir:` line.
2. Watch `<real-gitdir>/HEAD` and `<real-gitdir>/logs/HEAD`, not `<worktree>/.git/HEAD`.
3. The main worktree (where `<worktree>/.git` is a directory) uses the worktree-relative paths directly.

Worth a helper `resolve_git_dir(worktree_root: &Path) -> PathBuf` plus a test against a real `git worktree add` setup before the watcher code is considered done.

## Risks

- **Recursive watcher coverage of `.trinity/`.** `notify` recursive mode must handle create+delete+move+modify across macOS and Linux. Mitigation: integration tests per path-event combination on both platforms.
- **`.git/HEAD` watcher reliability.** `notify` on `.git/HEAD` may not fire on every git operation (some operations use refs/ updates that don't touch HEAD directly). Mitigation: also watch `.git/logs/HEAD` (which appends on essentially every HEAD movement). The union catches all real cases.
- **Plan-touch discipline when switching sessions on one branch.** Users alternating between sessions on a single linear chain without committing a plan-touch will see mis-attributed commits. Mitigation: document the constraint in `start_plan` response and `get_context` advice; the amend workflow is the escape hatch. Branch-per-session via separate worktrees is the supported parallelism story.
- **Cold-start cost as commit history grows.** Walk-back over very long histories could be slow. Mitigation: bound the walk at `oldest_plan_intro..HEAD`; if needed later, persist `.trinity/cache/attribution.bincode`. Defer until measured.
- **PR squash UX.** `pr_hint` is the only production support shipped. Real workflows may need richer guidance; iterate based on one real workflow.
